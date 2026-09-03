//! Browser acceptance tests (CLI acceptance-tests decision): each test
//! starts the `whirl` binary as a process and drives the real shim and
//! Chromium with tiny flows over `data:` URLs, so no test site is
//! needed. Every test uses its own temporary working directory, because
//! tests run in parallel processes.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};
use std::{env, fs, process};

/// A unique temporary directory for one test, removed on drop.
struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!("whirl-browser-test-{}-{id}", process::id()));
        fs::create_dir_all(&path).expect("temp dir should be creatable");
        Self { path }
    }

    /// Writes a flow (or any) file into the directory.
    fn file(&self, name: &str, content: &str) -> PathBuf {
        let path = self.path.join(name);
        fs::write(&path, content).expect("temp file should be writable");
        path
    }

    fn artifacts(&self) -> PathBuf {
        self.path.join("artifacts")
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Runs the built `whirl` binary against the real shim: `node` from
/// `PATH` and the repository's built shim entry, with the test's own
/// working and artifacts directories.
fn run_whirl(dir: &TestDir, args: &[&str]) -> Output {
    run_whirl_env(dir, args, &[])
}

/// Like [`run_whirl`], with extra environment variables for the child.
fn run_whirl_env(dir: &TestDir, args: &[&str], env: &[(&str, &str)]) -> Output {
    let shim_js = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../shim/dist/index.js");
    let artifacts = dir.artifacts();
    Command::new(env!("CARGO_BIN_EXE_whirl"))
        .env("WHIRL_NODE", "node")
        .env("WHIRL_SHIM_JS", shim_js)
        .envs(env.iter().copied())
        .current_dir(&dir.path)
        .arg("--artifacts")
        .arg(&artifacts)
        .args(args)
        .output()
        .expect("the whirl binary should run")
}

fn exit_code(output: &Output) -> i32 {
    output.status.code().expect("whirl should exit, not signal")
}

fn stdout_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn a_passing_flow_interpolates_a_capture_into_a_later_visit() {
    let dir = TestDir::new();
    let flow = dir.file(
        "pass.whirl",
        "# First page.\n\
         VISIT \"data:text/html,<h1>One</h1><input aria-label=\\\"Name\\\">\
         <button onclick=\\\"this.textContent='Done'\\\">Go</button>\"\n\
         FILL \"Name\" world\n\
         CLICK \"Go\"\n\
         [Asserts]\n\
         role:heading \"One\" visible\n\
         label:Name value == world\n\
         [Captures]\n\
         next_page: eval \"'data:text/html,<h1>Two</h1>'\"\n\
         \n\
         # Second page via the capture.\n\
         VISIT {{next_page}}\n\
         [Asserts]\n\
         role:heading \"Two\" visible\n",
    );
    let output = run_whirl(&dir, &[flow.to_str().expect("utf-8 path")]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    assert!(stdout.contains("passed"), "stdout:\n{stdout}");
}

#[test]
fn a_failing_assert_fails_the_file_and_skips_the_rest() {
    let dir = TestDir::new();
    let flow = dir.file(
        "fail.whirl",
        "# Failing entry.\n\
         VISIT \"data:text/html,<h1>Hi</h1>\"\n\
         [Asserts]\n\
         role:heading \"Hi\" text == Bye @1s\n\
         role:heading \"Hi\" visible\n\
         \n\
         # Skipped entry.\n\
         SCREENSHOT after\n",
    );
    let output = run_whirl(&dir, &[flow.to_str().expect("utf-8 path")]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 1, "stdout:\n{stdout}");
    assert!(stdout.contains("FAILED"), "stdout:\n{stdout}");
    assert!(
        stdout.contains("expected:") && stdout.contains("actual:"),
        "stdout:\n{stdout}"
    );
    let flow_artifacts = dir.artifacts().join("fail");
    assert!(
        flow_artifacts.join("failure.png").is_file(),
        "failure.png should be saved"
    );
    // The skipped entry never ran, so its screenshot does not exist.
    assert!(
        !flow_artifacts.join("after.png").exists(),
        "the skipped SCREENSHOT must not run"
    );
}

#[test]
fn an_undefined_option_variable_is_a_setup_failure() {
    let dir = TestDir::new();
    let flow = dir.file(
        "setup.whirl",
        "[Options]\nbase: {{missing_base}}\n\nVISIT /\n",
    );
    let output = run_whirl(&dir, &[flow.to_str().expect("utf-8 path")]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 1, "stdout:\n{stdout}");
    assert!(stdout.contains("[setup]"), "stdout:\n{stdout}");
    assert!(
        stdout.contains("undefined variable 'missing_base'"),
        "stdout:\n{stdout}"
    );
}

#[test]
fn two_files_run_in_parallel_workers() {
    let dir = TestDir::new();
    let first = dir.file(
        "first.whirl",
        "VISIT \"data:text/html,<h1>First</h1>\"\n\
         [Asserts]\nrole:heading \"First\" visible\n",
    );
    let second = dir.file(
        "second.whirl",
        "VISIT \"data:text/html,<h1>Second</h1>\"\n\
         [Asserts]\nrole:heading \"Second\" visible\n",
    );
    let output = run_whirl(&dir, &[
        "--jobs",
        "2",
        first.to_str().expect("utf-8 path"),
        second.to_str().expect("utf-8 path"),
    ]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    assert!(stdout.contains("first.whirl passed"), "stdout:\n{stdout}");
    assert!(stdout.contains("second.whirl passed"), "stdout:\n{stdout}");
}

#[test]
fn press_and_eval_action_and_eval_capture_work() {
    let dir = TestDir::new();
    let flow = dir.file(
        "press.whirl",
        "VISIT \"data:text/html,<input aria-label=\\\"Box\\\">\"\n\
         PRESS \"Box\" \"A\"\n\
         EVAL \"document.title = 'evaled'\"\n\
         [Asserts]\n\
         label:Box value == A\n\
         title == evaled\n\
         [Captures]\n\
         squared: eval \"7 * 7\"\n\
         \n\
         VISIT \"data:text/html,<h1>{{squared}}</h1>\"\n\
         [Asserts]\n\
         role:heading \"49\" visible\n",
    );
    let output = run_whirl(&dir, &[flow.to_str().expect("utf-8 path")]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
}

#[test]
fn type_sends_key_events_where_fill_does_not() {
    let dir = TestDir::new();
    // The page records every keydown. FILL fires none; TYPE fires one per
    // character, so the recorder shows only the typed text.
    let flow = dir.file(
        "type.whirl",
        "VISIT \"data:text/html,<input aria-label=\\\"Code\\\" onkeydown=\\\"document.getElementById('k').textContent+=event.key\\\"><div id=k></div>\"\n\
         FILL \"Code\" 99\n\
         TYPE \"Code\" 4242\n\
         [Asserts]\n\
         label:Code value == 994242\n\
         css:\"#k\" text == 4242\n",
    );
    let output = run_whirl(&dir, &[flow.to_str().expect("utf-8 path")]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
}

#[test]
fn store_cookie_on_a_data_url_fails_the_entry() {
    let dir = TestDir::new();
    // A data: URL has no http origin to attach a cookie to.
    let flow = dir.file(
        "cookie.whirl",
        "VISIT \"data:text/html,<h1>Hi</h1>\"\nSTORE cookie flag v1\n",
    );
    let output = run_whirl(&dir, &[flow.to_str().expect("utf-8 path")]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 1, "stdout:\n{stdout}");
    assert!(
        stdout.contains("needs an http or https page"),
        "stdout:\n{stdout}"
    );
}

#[test]
fn the_user_agent_option_and_flag_set_navigator_user_agent() {
    let dir = TestDir::new();
    // The file option sets the context's user agent; the flag beats the
    // file option (SPEC 5, 13).
    let flow = dir.file(
        "ua.whirl",
        "[Options]\nuser-agent: \"Whirl/1 (file option)\"\n\n\
         VISIT \"data:text/html,<h1>Hi</h1>\"\n\
         [Captures]\nua: eval \"navigator.userAgent\"\n",
    );
    let flow = flow.to_str().expect("utf-8 path");
    let output = run_whirl(&dir, &["--report-json", "report.json", flow]);
    assert_eq!(exit_code(&output), 0, "stdout:\n{}", stdout_text(&output));
    let report: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path.join("report.json")).expect("report"))
            .expect("valid JSON report");
    assert_eq!(
        report["files"][0]["entries"][0]["captures"]["ua"], "Whirl/1 (file option)",
        "report:\n{report}"
    );

    let output = run_whirl(&dir, &[
        "--user-agent",
        "Whirl/1 (flag)",
        "--report-json",
        "report.json",
        flow,
    ]);
    assert_eq!(exit_code(&output), 0, "stdout:\n{}", stdout_text(&output));
    let report: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path.join("report.json")).expect("report"))
            .expect("valid JSON report");
    assert_eq!(
        report["files"][0]["entries"][0]["captures"]["ua"], "Whirl/1 (flag)",
        "report:\n{report}"
    );
}

#[test]
fn a_run_writes_json_and_junit_reports_with_masking() {
    let dir = TestDir::new();
    let secret = "hunter2-report-secret";
    let pass = dir.file(
        "pass.whirl",
        "# Fill the box.\n\
         VISIT \"data:text/html,<h1>Pass</h1><input aria-label=\\\"Box\\\">\"\n\
         FILL \"Box\" {{env.WHIRL_TEST_SECRET}}\n\
         [Asserts]\n\
         role:heading \"Pass\" visible\n\
         label:Box value == {{env.WHIRL_TEST_SECRET}}\n\
         [Captures]\n\
         page_title: eval \"'captured-title'\"\n",
    );
    let fail = dir.file(
        "fail.whirl",
        "# Mismatched heading.\n\
         VISIT \"data:text/html,<h1>Real</h1>\"\n\
         [Asserts]\n\
         role:heading \"Real\" text == Wanted @1s\n",
    );
    let output = run_whirl_env(
        &dir,
        &[
            "--report-json",
            "report.json",
            "--report-junit",
            "report.xml",
            pass.to_str().expect("utf-8 path"),
            fail.to_str().expect("utf-8 path"),
        ],
        &[("WHIRL_TEST_SECRET", secret)],
    );
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 1, "stdout:\n{stdout}");

    let json_text =
        fs::read_to_string(dir.path.join("report.json")).expect("report.json should exist");
    let junit_text =
        fs::read_to_string(dir.path.join("report.xml")).expect("report.xml should exist");
    assert_json_report(&json_text, secret);
    assert_junit_report(&junit_text, secret);
}

/// Asserts the JSON report's core shape, content, and masking.
fn assert_json_report(text: &str, secret: &str) {
    let report: serde_json::Value =
        serde_json::from_str(text).expect("report.json should parse as JSON");
    assert_eq!(report["version"], 1, "report:\n{text}");
    let files = report["files"]
        .as_array()
        .expect("files should be an array");
    assert_eq!(files.len(), 2, "report:\n{text}");
    let passed = files
        .iter()
        .find(|file| file["status"] == "passed")
        .expect("one file should pass");
    let failed = files
        .iter()
        .find(|file| file["status"] == "failed")
        .expect("one file should fail");
    assert_eq!(passed["entries"][0]["name"], "Fill the box.");
    assert_eq!(
        passed["entries"][0]["captures"]["page_title"],
        "captured-title"
    );
    let failing_entry = &failed["entries"][0];
    assert_eq!(failing_entry["name"], "Mismatched heading.");
    let failing_step = failing_entry["steps"]
        .as_array()
        .expect("steps should be an array")
        .iter()
        .find(|step| step["status"] == "failed")
        .expect("a step should fail");
    assert_eq!(failing_step["kind"], "assert");
    let error = &failing_step["error"];
    assert_eq!(error["expected"], "text == \"Wanted\"", "report:\n{text}");
    assert_eq!(error["actual"], "Real", "report:\n{text}");
    assert!(!text.contains(secret), "report:\n{text}");
    assert!(text.contains("***"), "report:\n{text}");
}

/// Asserts the JUnit report's core shape, content, and masking.
fn assert_junit_report(xml: &str, secret: &str) {
    assert!(xml.contains("pass.whirl"), "xml:\n{xml}");
    assert!(xml.contains("fail.whirl"), "xml:\n{xml}");
    assert!(
        xml.contains("<testcase name=\"Fill the box.\""),
        "xml:\n{xml}"
    );
    assert!(
        xml.contains("<testcase name=\"Mismatched heading.\""),
        "xml:\n{xml}"
    );
    assert!(xml.contains("<failure"), "xml:\n{xml}");
    assert!(xml.contains("Wanted"), "xml:\n{xml}");
    assert!(xml.contains("actual: Real"), "xml:\n{xml}");
    assert!(!xml.contains(secret), "xml:\n{xml}");
}

#[test]
fn an_expiring_entry_timeout_fails_the_in_flight_step() {
    let dir = TestDir::new();
    let flow = dir.file(
        "hang.whirl",
        "[Options]\nentry-timeout: 1s\n\n\
         VISIT \"data:text/html,<h1>Hi</h1>\"\n\
         EVAL \"new Promise(()=>{})\" @30s\n",
    );
    let output = run_whirl(&dir, &[flow.to_str().expect("utf-8 path")]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 1, "stdout:\n{stdout}");
    assert!(stdout.contains("entry timeout"), "stdout:\n{stdout}");
}
