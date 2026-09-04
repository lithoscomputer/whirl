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

/// Runs `whirl check` on files in the test directory; `check` takes no
/// artifacts flag and launches no browser.
fn run_check(dir: &TestDir, files: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_whirl"))
        .current_dir(&dir.path)
        .arg("check")
        .args(files)
        .output()
        .expect("the whirl binary should run")
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
fn check_validates_setup_flows_and_their_captures() {
    let dir = TestDir::new();
    dir.file(
        "login.whirl",
        "VISIT /login\n[Captures]\ntoken: css:\"#token\" text\n",
    );
    // A dependent that reads the capture: the setup file's capture counts
    // as used, so neither file warns.
    dir.file(
        "good.whirl",
        "[Options]\nsetup: login.whirl\n\nVISIT /u/{{setup.token}}\n",
    );
    let output = run_check(&dir, &["good.whirl"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(exit_code(&output), 0, "stderr:\n{stderr}");
    assert!(stderr.trim().is_empty(), "stderr:\n{stderr}");

    dir.file(
        "bad.whirl",
        "[Options]\nsetup: login.whirl\nstorage: state.json\n\nVISIT /u/{{setup.nope}}\n",
    );
    let output = run_check(&dir, &["bad.whirl"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(exit_code(&output), 2, "stderr:\n{stderr}");
    assert!(
        stderr.contains("storage and setup both set the starting state"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("setup flow 'login.whirl' has no capture `nope`"),
        "stderr:\n{stderr}"
    );

    dir.file("orphan.whirl", "VISIT /u/{{setup.token}}\n");
    let output = run_check(&dir, &["orphan.whirl"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(exit_code(&output), 2, "stderr:\n{stderr}");
    assert!(stderr.contains("needs a setup option"), "stderr:\n{stderr}");

    dir.file("nested.whirl", "[Options]\nsetup: good.whirl\n\nVISIT /\n");
    let output = run_check(&dir, &["nested.whirl"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(exit_code(&output), 2, "stderr:\n{stderr}");
    assert!(stderr.contains("names its own setup"), "stderr:\n{stderr}");

    dir.file(
        "missing.whirl",
        "[Options]\nsetup: nowhere.whirl\n\nVISIT /\n",
    );
    let output = run_check(&dir, &["missing.whirl"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(exit_code(&output), 3, "stderr:\n{stderr}");
    assert!(stderr.contains("does not exist"), "stderr:\n{stderr}");
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
fn a_hyphenated_screenshot_name_becomes_the_artifact_file_name() {
    let dir = TestDir::new();
    let flow = dir.file(
        "shot.whirl",
        "VISIT \"data:text/html,<h1>Hi</h1>\"\nSCREENSHOT after-verification-code\n",
    );
    let output = run_whirl(&dir, &[flow.to_str().expect("utf-8 path")]);
    assert_eq!(exit_code(&output), 0, "stdout:\n{}", stdout_text(&output));
    assert!(
        dir.artifacts()
            .join("shot")
            .join("after-verification-code.png")
            .is_file(),
        "the screenshot should be written under the hyphenated name"
    );
}

#[test]
fn the_reduced_motion_option_is_visible_to_the_page() {
    let dir = TestDir::new();
    let flow = dir.file(
        "motion.whirl",
        "[Options]\nreduced-motion: reduce\n\n\
         VISIT \"data:text/html,<h1>Hi</h1>\"\n\
         [Captures]\nreduced: eval \"matchMedia('(prefers-reduced-motion: reduce)').matches\"\n",
    );
    let output = run_whirl(&dir, &[
        "--report-json",
        "report.json",
        flow.to_str().expect("utf-8 path"),
    ]);
    assert_eq!(exit_code(&output), 0, "stdout:\n{}", stdout_text(&output));
    let report: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path.join("report.json")).expect("report"))
            .expect("valid JSON report");
    assert_eq!(
        report["files"][0]["entries"][0]["captures"]["reduced"], "true",
        "report:\n{report}"
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

#[test]
fn presence_before_hidden_requires_the_element_to_appear() {
    let dir = TestDir::new();
    dir.file("presence.whirl", "VISIT \"data:text/html,<p>Hello</p>\"\n[Asserts]\ntestid:spinner count >= 1 @100ms\ntestid:spinner hidden\n");
    let output = run_whirl(&dir, &["presence.whirl"]);
    assert_eq!(exit_code(&output), 1);
    assert!(String::from_utf8_lossy(&output.stderr).is_empty());
    assert!(stdout_text(&output).contains("actual: 0"));
}

#[test]
fn a_missing_element_reports_what_the_action_waited_for() {
    let dir = TestDir::new();
    dir.file(
        "missing.whirl",
        "VISIT \"data:text/html,<button>Sign in</button>\"\nCLICK role:button \"Log in\" @200ms\n",
    );
    let output = run_whirl(&dir, &["missing.whirl"]);
    assert_eq!(exit_code(&output), 1);
    assert!(
        stdout_text(&output).contains("waiting for getByRole"),
        "{}",
        stdout_text(&output)
    );
}

#[test]
fn a_covered_element_reports_the_overlay_and_masks_its_secret() {
    let dir = TestDir::new();
    dir.file("covered.whirl", "VISIT \"data:text/html,<button>Sign in</button><div style=position:fixed;inset:0>{{env.OVERLAY}}</div>\"\nCLICK role:button \"Sign in\" @200ms\n");
    let output = run_whirl_env(
        &dir,
        &["--trace", "--report-json", "report.json", "covered.whirl"],
        &[("OVERLAY", "private-overlay-token")],
    );
    assert_eq!(exit_code(&output), 1);
    let text = stdout_text(&output);
    assert!(text.contains("intercepts pointer events"), "{text}");
    assert!(text.contains("whirl show-trace -- '"), "{text}");
    assert!(!text.contains("private-overlay-token"));
    let json = fs::read_to_string(dir.path.join("report.json")).expect("report exists");
    assert!(json.contains("intercepts pointer events"));
    assert!(!json.contains("private-overlay-token"));
}

#[test]
fn show_trace_uses_the_selected_runtime_and_preserves_the_path_argument() {
    let dir = TestDir::new();
    let shim = dir.file("index.js", "");
    let package = dir.path.join("node_modules/@playwright/test");
    fs::create_dir_all(&package).expect("package directory exists");
    fs::write(
        package.join("cli.js"),
        "require('node:fs').writeFileSync('args.json', JSON.stringify(process.argv.slice(2))); ",
    )
    .expect("CLI fixture exists");
    let trace = dir.file("trace with 'quotes'.zip", "fixture");
    let output = Command::new(env!("CARGO_BIN_EXE_whirl"))
        .current_dir(&dir.path)
        .env("WHIRL_NODE", "node")
        .env("WHIRL_SHIM_JS", shim)
        .arg("show-trace")
        .arg(&trace)
        .output()
        .expect("CLI runs");
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
    let args: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(dir.path.join("args.json")).expect("arguments recorded"),
    )
    .expect("valid JSON");
    assert_eq!(
        args,
        serde_json::json!(["show-trace", trace.canonicalize().expect("trace exists")])
    );
}

#[test]
fn json_reports_include_actual_runtime_versions_and_error_codes() {
    let dir = TestDir::new();
    dir.file(
        "flow.whirl",
        "VISIT \"data:text/html,<title>Hello</title>\"\n[Asserts]\ntitle == Wrong @100ms\n",
    );
    let output = run_whirl(&dir, &["--report-json", "report.json", "flow.whirl"]);
    assert_eq!(exit_code(&output), 1);
    let report: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(dir.path.join("report.json")).expect("report exists"),
    )
    .expect("report is JSON");
    assert_eq!(report["whirlVersion"], env!("CARGO_PKG_VERSION"));
    assert_eq!(report["platform"], env::consts::OS);
    assert_eq!(report["files"][0]["runtime"]["browser"], "chromium");
    assert_eq!(report["files"][0]["runtime"]["playwrightVersion"], "1.62.1");
    assert!(
        report["files"][0]["runtime"]["browserVersion"]
            .as_str()
            .is_some_and(|v| !v.is_empty())
    );
    assert!(
        report["files"][0]["runtime"]["nodeVersion"]
            .as_str()
            .is_some_and(|v| !v.is_empty())
    );
    assert_eq!(report["files"][0]["runtime"]["viewport"]["width"], 1280);
    assert_eq!(
        report["files"][0]["entries"][0]["steps"][1]["error"]["code"],
        "assert"
    );
}

#[test]
fn rerun_selects_only_failed_files_and_runs_their_setup_again_from_another_directory() {
    let dir = TestDir::new();
    dir.file(
        "setup.whirl",
        "VISIT \"data:text/html,<title>first</title>\"\n[Captures]\ntoken: title\n",
    );
    dir.file("dependent.whirl", "[Options]\nsetup: setup.whirl\nVISIT \"data:text/html,<title>{{setup.token}}</title>\"\n[Asserts]\ntitle == second @100ms\n");
    dir.file("passed.whirl", "VISIT \"data:text/html,<p>Hello</p>\"\n");
    let first = run_whirl(&dir, &[
        "--report-json",
        "report.json",
        "dependent.whirl",
        "passed.whirl",
    ]);
    assert_eq!(exit_code(&first), 1);
    dir.file("passed.whirl", "BOGUS must not be selected\n");
    dir.file(
        "setup.whirl",
        "VISIT \"data:text/html,<title>second</title>\"\n[Captures]\ntoken: title\n",
    );
    let other = TestDir::new();
    let report_path = dir.path.join("report.json");
    let rerun = run_whirl(&other, &[
        "--rerun-failed",
        report_path.to_str().expect("UTF-8 path"),
    ]);
    assert_eq!(exit_code(&rerun), 0, "{}", stdout_text(&rerun));
    assert!(stdout_text(&rerun).contains("dependent.whirl passed"));
    assert!(stdout_text(&rerun).contains("setup.whirl passed"));
    assert!(!stdout_text(&rerun).contains("passed.whirl"));
}

#[test]
fn doctor_launches_the_real_browser_and_checks_the_runtime() {
    let dir = TestDir::new();
    let shim_js = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../shim/dist/index.js");
    let output = Command::new(env!("CARGO_BIN_EXE_whirl"))
        .current_dir(&dir.path)
        .env("WHIRL_NODE", "node")
        .env("WHIRL_SHIM_JS", shim_js)
        .arg("doctor")
        .output()
        .expect("doctor runs");
    assert_eq!(
        exit_code(&output),
        0,
        "{}\n{}",
        stdout_text(&output),
        String::from_utf8_lossy(&output.stderr)
    );
    let text = stdout_text(&output);
    assert!(text.contains("Playwright 1.62.1: OK"));
    assert!(text.contains("chromium: browser launch OK"));
    assert!(text.contains("Whirl is ready."));
}

#[test]
fn doctor_reports_the_selected_missing_browser_and_its_repair() {
    let dir = TestDir::new();
    let shim_js = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../shim/dist/index.js");
    let output = Command::new(env!("CARGO_BIN_EXE_whirl"))
        .current_dir(&dir.path)
        .env("WHIRL_NODE", "node")
        .env("WHIRL_SHIM_JS", shim_js)
        .env("PLAYWRIGHT_BROWSERS_PATH", dir.path.join("empty-browsers"))
        .args(["doctor", "--browser", "firefox"])
        .output()
        .expect("doctor runs");
    assert_eq!(exit_code(&output), 3);
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("whirl install firefox"), "{error}");
    if cfg!(target_os = "linux") {
        assert!(error.contains("install-deps firefox"), "{error}");
    }
}
