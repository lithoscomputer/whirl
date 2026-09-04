//! Browser acceptance tests against a local test site (CLI
//! acceptance-tests decision 1.2): each test starts the `whirl` binary
//! as a process and runs `.whirl` flows in a real browser against pages
//! served from `tests/site/` on an ephemeral loopback port. Every test
//! checks the CLI result and the required artifacts. Every test uses
//! its own temporary working directory, because tests run in parallel
//! processes.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use std::{env, fs, process, thread};

use tiny_http::{Header, Response, Server};

/// A unique temporary directory for one test, removed on drop.
struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!("whirl-site-test-{}-{id}", process::id()));
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

/// A static file server for `tests/site/` on an ephemeral loopback
/// port. Worker threads run until the test process exits; nextest runs
/// each test in its own process, so nothing leaks across tests.
struct SiteServer {
    port: u16,
}

impl SiteServer {
    fn start() -> Self {
        let server = Server::http((Ipv4Addr::LOCALHOST, 0)).expect("an ephemeral port should bind");
        let port = server
            .server_addr()
            .to_ip()
            .expect("the server binds an IP address")
            .port();
        let server = Arc::new(server);
        // A few workers, so a browser's parallel connections never
        // queue behind each other.
        for _ in 0..4 {
            let server = Arc::clone(&server);
            #[expect(
                clippy::disallowed_methods,
                reason = "the test's blocking HTTP server needs OS threads; there is no runtime"
            )]
            thread::spawn(move || {
                while let Ok(request) = server.recv() {
                    respond(request);
                }
            });
        }
        Self { port }
    }

    /// The site's base URL under the IPv4 loopback hostname.
    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

/// Serves one request from `tests/site/`, ignoring the query string.
fn respond(request: tiny_http::Request) {
    let url = request.url().to_owned();
    let path = url.split('?').next().unwrap_or("/").trim_start_matches('/');
    // The redirect test needs a server-side 302 to this same server
    // under its other loopback hostname.
    // The slow-load test needs a subresource that keeps the page's load
    // event pending well past the navigation timeout.
    if path == "stall" {
        #[expect(
            clippy::disallowed_methods,
            reason = "the test's blocking HTTP server holds a response open on its own OS thread"
        )]
        thread::sleep(Duration::from_secs(20));
        let _ = request.respond(Response::empty(204));
        return;
    }
    if path == "redirect-cross" {
        let port = request
            .headers()
            .iter()
            .find(|header| header.field.equiv("Host"))
            .and_then(|header| header.value.as_str().rsplit(':').next().map(str::to_owned))
            .unwrap_or_default();
        let location = Header::from_bytes(
            &b"Location"[..],
            format!("http://localhost:{port}/second.html").into_bytes(),
        )
        .expect("the redirect location is a valid header");
        let _ = request.respond(Response::empty(302).with_header(location));
        return;
    }
    let file = site_root().join(path);
    let Ok(bytes) = fs::read(&file) else {
        let _ = request.respond(Response::empty(404));
        return;
    };
    let content_type = if Path::new(path)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("html"))
    {
        "text/html; charset=utf-8"
    } else {
        "text/plain; charset=utf-8"
    };
    let content_type = Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes())
        .expect("a static content type is a valid header");
    // The cross-host test fetches this server through its other loopback
    // hostname; without this header the browser's CORS check would fail
    // that fetch even when allow-hosts permits it.
    let cors = Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..])
        .expect("a static CORS header is valid");
    let _ = request.respond(
        Response::from_data(bytes)
            .with_header(content_type)
            .with_header(cors),
    );
}

fn site_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/site")
}

/// Runs the built `whirl` binary against the real shim: `node` from
/// `PATH` and the repository's built shim entry, with the test's own
/// working and artifacts directories.
fn run_whirl(dir: &TestDir, args: &[&str]) -> Output {
    let shim_js = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../shim/dist/index.js");
    let artifacts = dir.artifacts();
    Command::new(env!("CARGO_BIN_EXE_whirl"))
        .env("WHIRL_NODE", "node")
        .env("WHIRL_SHIM_JS", shim_js)
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

/// The snapshot baseline platform tag (SPEC 7).
fn platform_tag() -> &'static str {
    if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "windows") {
        "win32"
    } else {
        "linux"
    }
}

/// The happy-path flow body; the `[Options]` header with the site's
/// base URL is prepended per run.
const HAPPY_FLOW_BODY: &str = r##"# Fill the form.
VISIT /form.html
FILL "Email" alice@example.com
TYPE "Code" 4242
CHECK "Notifications"
FILL placeholder:"Search things" widget
CLICK testid:save-button
PRESS placeholder:"Search things" "Enter"
SCREENSHOT overview
[Asserts]
role:heading "Form page" visible
label:Email value == alice@example.com
label:Code value == 4242
css:"#typed-keys" text == 4242
label:Notifications checked
placeholder:"Search things" value == widget
placeholder:"Search things" focused
css:"#press-result" text == enter-pressed
css:"#saved" text == saved
css:"li.item" count >= 3
css:"li.item" >> nth:2 text == Two
text~:"rder #ABC" visible
testid:order text matches /Order #\w+/
css:"#spaced" text matches /^spaced text$/
testid:state attr:data-state == open
css:"#ghost" hidden
role:button "Disabled btn" disabled
role:button Save enabled
label:Terms checked
label:Subscribe unchecked
url contains form.html
title == "Form Page"
[Captures]
order_id: testid:order text regex /Order #(\w+)/
next_path: css:"#next-link" attr:href

# Follow the captured link.
VISIT {{next_path}}
PAGE /second.html
[Asserts]
role:heading "Second page" visible

# Revisit with the captured query value.
VISIT /second.html?q={{order_id}}
PAGE /second.html?q=ABC123
[Asserts]
url contains q=ABC123
"##;

#[test]
fn a_full_flow_passes_against_the_site() {
    let server = SiteServer::start();
    let dir = TestDir::new();
    dir.file(
        "happy.whirl",
        &format!(
            "[Options]\nbase: {base}\n\n{HAPPY_FLOW_BODY}",
            base = server.base()
        ),
    );
    let output = run_whirl(&dir, &["happy.whirl"]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    assert!(stdout.contains("happy.whirl passed"), "stdout:\n{stdout}");
    assert!(
        dir.artifacts().join("happy/overview.png").is_file(),
        "SCREENSHOT should write overview.png"
    );
}

#[test]
fn check_and_uncheck_handle_hidden_inputs_and_role_switches() {
    let server = SiteServer::start();
    let dir = TestDir::new();
    // "Notifications" is a clipped input behind a styled track, "Dark
    // mode" is a role=switch button; CHECK twice in a row is a no-op.
    dir.file(
        "switch.whirl",
        "VISIT /form.html\n\
         CHECK \"Notifications\"\n\
         CHECK \"Notifications\"\n\
         [Asserts]\n\
         label:Notifications checked\n\
         UNCHECK \"Notifications\"\n\
         [Asserts]\n\
         label:Notifications unchecked\n\
         CHECK role:switch \"Dark mode\"\n\
         [Asserts]\n\
         role:switch \"Dark mode\" checked\n\
         UNCHECK role:switch \"Dark mode\"\n\
         [Asserts]\n\
         role:switch \"Dark mode\" unchecked\n",
    );
    let output = run_whirl(&dir, &["--base", &server.base(), "switch.whirl"]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
}

#[test]
fn check_on_a_display_none_input_fails_with_a_focus_message() {
    let server = SiteServer::start();
    let dir = TestDir::new();
    dir.file("gone.whirl", "VISIT /form.html\nCHECK \"Gone\"\n");
    let output = run_whirl(&dir, &["--base", &server.base(), "gone.whirl"]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 1, "stdout:\n{stdout}");
    assert!(stdout.contains("cannot take focus"), "stdout:\n{stdout}");
}

#[test]
fn store_session_and_cookie_reach_the_page_after_the_next_visit() {
    let server = SiteServer::start();
    let dir = TestDir::new();
    // The page mirrors the "flag" sessionStorage entry and the "flag"
    // cookie while loading, so each STORE shows after a reload.
    dir.file(
        "store.whirl",
        "VISIT /form.html\n\
         [Asserts]\n\
         css:\"#session-flag\" text == unset\n\
         css:\"#cookie-flag\" text == unset\n\
         STORE session flag \"from session\"\n\
         STORE cookie flag v1\n\
         VISIT /form.html\n\
         [Asserts]\n\
         css:\"#session-flag\" text == \"from session\"\n\
         css:\"#cookie-flag\" text == v1\n",
    );
    let output = run_whirl(&dir, &["--base", &server.base(), "store.whirl"]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
}

#[test]
fn visit_completes_at_domcontentloaded_while_a_subresource_stalls_load() {
    let server = SiteServer::start();
    let dir = TestDir::new();
    // The page's image never answers within the navigation timeout, so a
    // VISIT that waited for `load` would time out here.
    dir.file(
        "slow.whirl",
        &format!(
            "[Options]\nbase: {base}\nnav-timeout: 3s\n\n\
             VISIT /slow-load.html\n[Asserts]\nrole:heading \"Parsed\" visible\n",
            base = server.base()
        ),
    );
    let output = run_whirl(&dir, &["slow.whirl"]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
}

#[test]
fn asserts_retry_until_delayed_text_appears() {
    let server = SiteServer::start();
    let dir = TestDir::new();
    // The page rewrites #late from "pending" to "ready" after 300ms, so
    // this assert passes only because it retries.
    dir.file(
        "waits.whirl",
        "VISIT /form.html\n[Asserts]\ncss:\"#late\" text == ready\n",
    );
    let output = run_whirl(&dir, &["--base", &server.base(), "waits.whirl"]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
}

#[test]
fn store_local_is_visible_to_the_page_after_the_next_visit() {
    let server = SiteServer::start();
    let dir = TestDir::new();
    // The page mirrors the "flag" entry while loading, so the first visit
    // shows "unset", and the value written by STORE shows after a reload.
    dir.file(
        "store.whirl",
        "VISIT /form.html\n\
         [Asserts]\n\
         css:\"#stored-flag\" text == unset\n\
         STORE local flag \"seen it\"\n\
         VISIT /form.html\n\
         [Asserts]\n\
         css:\"#stored-flag\" text == \"seen it\"\n",
    );
    let output = run_whirl(&dir, &["--base", &server.base(), "store.whirl"]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
}

#[test]
fn a_wrong_assert_times_out_with_expected_and_actual() {
    let server = SiteServer::start();
    let dir = TestDir::new();
    // A wrong expectation retries until the step timeout, then reports
    // expected versus actual.
    dir.file(
        "wrong.whirl",
        "VISIT /form.html\n[Asserts]\ncss:\"#late\" text == never\n",
    );
    let output = run_whirl(&dir, &[
        "--base",
        &server.base(),
        "--step-timeout",
        "900ms",
        "wrong.whirl",
    ]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 1, "stdout:\n{stdout}");
    assert!(stdout.contains("wrong.whirl FAILED"), "stdout:\n{stdout}");
    assert!(stdout.contains("expected:"), "stdout:\n{stdout}");
    // 300ms in, the delayed rewrite has landed, so the reported actual
    // is the settled text.
    assert!(stdout.contains("actual: ready"), "stdout:\n{stdout}");
}

#[test]
fn select_check_uncheck_hover_dblclick_and_upload_reach_the_page() {
    let server = SiteServer::start();
    let dir = TestDir::new();
    // UPLOAD paths resolve relative to the .whirl file, which sits next
    // to this asset in the temp directory.
    dir.file("avatar.txt", "not really an image\n");
    dir.file(
        "controls.whirl",
        r##"VISIT /form.html
SELECT "Color" "Green"
CHECK "Subscribe"
UNCHECK "Terms"
CHECK "Small"
HOVER css:"#hover-me"
DBLCLICK css:"#dbl"
UPLOAD "Avatar" file:avatar.txt
[Asserts]
css:"#color-result" text == green
label:Color value == green
label:Subscribe checked
label:Terms unchecked
label:Small checked
css:"#hover-me" text == hovered
css:"#dbl" text == dblclicked
css:"#upload-name" text == avatar.txt
"##,
    );
    let output = run_whirl(&dir, &["--base", &server.base(), "controls.whirl"]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
}

#[test]
fn an_ambiguous_locator_fails_with_the_candidate_list() {
    let server = SiteServer::start();
    let dir = TestDir::new();
    // Two buttons carry the exact text "Dup": strict mode fails
    // immediately and lists the candidates (SPEC 6.2).
    dir.file("dup.whirl", "VISIT /form.html\nCLICK \"Dup\"\n");
    let output = run_whirl(&dir, &["--base", &server.base(), "dup.whirl"]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 1, "stdout:\n{stdout}");
    assert!(stdout.contains("dup.whirl FAILED"), "stdout:\n{stdout}");
    assert!(stdout.contains("candidate:"), "stdout:\n{stdout}");
}

#[test]
fn allow_hosts_blocks_a_cross_host_fetch_and_reports_the_host() {
    let server = SiteServer::start();
    let dir = TestDir::new();
    // The page is served from 127.0.0.1 and fetches the same server as
    // http://localhost:PORT — a different hostname, so allow-hosts
    // aborts the request and the page records the failure.
    dir.file(
        "cross.whirl",
        &format!(
            "[Options]\nbase: {base}\nallow-hosts: 127.0.0.1\n\n\
             VISIT /cross.html\n[Asserts]\ncss:\"#fetch-result\" text == blocked\n",
            base = server.base()
        ),
    );
    let output = run_whirl(&dir, &[
        "--report-json",
        "report.json",
        "--report-junit",
        "report.xml",
        "cross.whirl",
    ]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    let report = fs::read_to_string(dir.path.join("report.json")).expect("report.json exists");
    let report: serde_json::Value = serde_json::from_str(&report).expect("valid JSON report");
    let blocked = report["files"][0]["blockedHosts"]
        .as_array()
        .expect("blockedHosts is an array");
    assert!(
        blocked.iter().any(|host| host == "localhost"),
        "blockedHosts should list localhost, got {blocked:?}"
    );
    // The console output and the JUnit report list the blocked host too
    // (SPEC 5: "the reports list every blocked host").
    assert!(
        stdout.contains("blocked host: localhost"),
        "stdout:\n{stdout}"
    );
    let junit = fs::read_to_string(dir.path.join("report.xml")).expect("report.xml exists");
    assert!(junit.contains("blocked host: localhost"), "junit:\n{junit}");
}

#[test]
fn allow_hosts_blocks_a_server_side_redirect_to_a_cross_host_target() {
    let server = SiteServer::start();
    let dir = TestDir::new();
    // The page is served from 127.0.0.1 and /redirect-cross answers 302
    // to the same server as http://localhost:PORT — a different
    // hostname. The redirect hop must be aborted and recorded like a
    // direct request (SPEC 5).
    dir.file(
        "redir.whirl",
        &format!(
            "[Options]\nbase: {base}\nallow-hosts: 127.0.0.1\n\nVISIT /redirect-cross\n",
            base = server.base()
        ),
    );
    let output = run_whirl(&dir, &["--report-json", "report.json", "redir.whirl"]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 1, "stdout:\n{stdout}");
    assert!(stdout.contains("redir.whirl FAILED"), "stdout:\n{stdout}");
    let report = fs::read_to_string(dir.path.join("report.json")).expect("report.json exists");
    let report: serde_json::Value = serde_json::from_str(&report).expect("valid JSON report");
    let blocked = report["files"][0]["blockedHosts"]
        .as_array()
        .expect("blockedHosts is an array");
    assert!(
        blocked.iter().any(|host| host == "localhost"),
        "blockedHosts should list the redirect target, got {blocked:?}"
    );
}

#[test]
fn cross_host_requests_pass_without_allow_hosts() {
    let server = SiteServer::start();
    let dir = TestDir::new();
    dir.file(
        "open.whirl",
        "VISIT /cross.html\n[Asserts]\ncss:\"#fetch-result\" text == fetched\n",
    );
    let output = run_whirl(&dir, &["--base", &server.base(), "open.whirl"]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
}

#[test]
fn dialogs_are_dismissed_by_default_and_accepted_on_request() {
    let server = SiteServer::start();
    let dir = TestDir::new();
    // The page records the confirm() return value, so the configured
    // dialog response is observable in the DOM.
    dir.file(
        "dismiss.whirl",
        r##"VISIT /form.html
CLICK "Confirm thing"
[Asserts]
css:"#dialog-result" text == dismissed
"##,
    );
    dir.file(
        "accept.whirl",
        &format!(
            "[Options]\nbase: {base}\ndialogs: accept\n\n\
             VISIT /form.html\nCLICK \"Confirm thing\"\n\
             [Asserts]\ncss:\"#dialog-result\" text == accepted\n",
            base = server.base()
        ),
    );
    let output = run_whirl(&dir, &[
        "--base",
        &server.base(),
        "dismiss.whirl",
        "accept.whirl",
    ]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    assert!(stdout.contains("dismiss.whirl passed"), "stdout:\n{stdout}");
    assert!(stdout.contains("accept.whirl passed"), "stdout:\n{stdout}");
}

#[test]
fn saved_storage_state_logs_the_second_flow_in() {
    let server = SiteServer::start();
    let dir = TestDir::new();
    let state = dir.path.join("state.json");
    let state_arg = state.to_str().expect("utf-8 path");

    // A login flow sets localStorage and a cookie, and --save-storage
    // writes the final context state.
    dir.file(
        "login.whirl",
        r##"VISIT /login.html
CLICK "Log in"
[Asserts]
css:"#status" text == "logged in"
"##,
    );
    let output = run_whirl(&dir, &[
        "--base",
        &server.base(),
        "--save-storage",
        state_arg,
        "login.whirl",
    ]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    assert!(state.is_file(), "--save-storage should write {state:?}");

    // A second flow starts from that state (the `storage` option
    // resolves relative to the .whirl file) and is already logged in
    // without clicking anything.
    dir.file(
        "reuse.whirl",
        &format!(
            "[Options]\nbase: {base}\nstorage: state.json\n\n\
             VISIT /login.html\n[Asserts]\ncss:\"#status\" text == \"already logged in\"\n",
            base = server.base()
        ),
    );
    let output = run_whirl(&dir, &["reuse.whirl"]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
}

#[test]
fn update_snapshots_writes_a_baseline_that_a_second_run_matches() {
    let server = SiteServer::start();
    let dir = TestDir::new();
    dir.file("snap.whirl", "VISIT /stable.html\nSNAPSHOT hero\n");
    let baseline = dir.path.join(format!(
        "snap.whirl-snapshots/hero-chromium-{platform}.png",
        platform = platform_tag()
    ));

    // --update-snapshots writes the baseline next to the flow file,
    // keyed by browser and platform.
    let output = run_whirl(&dir, &[
        "--base",
        &server.base(),
        "--update-snapshots",
        "snap.whirl",
    ]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    assert!(baseline.is_file(), "baseline should exist at {baseline:?}");

    // A second run compares against the baseline and passes.
    let output = run_whirl(&dir, &["--base", &server.base(), "snap.whirl"]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
}

#[test]
fn a_snapshot_mismatch_fails_and_writes_actual_and_diff() {
    let server = SiteServer::start();
    let dir = TestDir::new();
    dir.file("snap.whirl", "VISIT /stable.html\nSNAPSHOT hero\n");

    // Write the baseline from the stable page.
    let output = run_whirl(&dir, &[
        "--base",
        &server.base(),
        "--update-snapshots",
        "snap.whirl",
    ]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");

    // The mutated page (the variant query flips a color) fails the
    // comparison and writes the actual and diff artifacts.
    dir.file(
        "snap.whirl",
        "VISIT /stable.html?variant=1\nSNAPSHOT hero\n",
    );
    let output = run_whirl(&dir, &[
        "--base",
        &server.base(),
        "--step-timeout",
        "1s",
        "snap.whirl",
    ]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 1, "stdout:\n{stdout}");
    assert!(stdout.contains("snap.whirl FAILED"), "stdout:\n{stdout}");
    let flow_artifacts = dir.artifacts().join("snap");
    assert!(
        flow_artifacts.join("snapshot-hero-actual.png").is_file(),
        "the actual image should be saved"
    );
    assert!(
        flow_artifacts.join("snapshot-hero-diff.png").is_file(),
        "the diff image should be saved"
    );
}

#[test]
fn a_snapshot_without_a_baseline_is_a_runtime_error() {
    let server = SiteServer::start();
    let dir = TestDir::new();
    // A snapshot with no baseline is a runtime error without
    // --update-snapshots.
    dir.file("fresh.whirl", "VISIT /stable.html\nSNAPSHOT fresh\n");
    let output = run_whirl(&dir, &["--base", &server.base(), "fresh.whirl"]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 3, "stdout:\n{stdout}");
}

#[test]
fn trace_video_and_har_artifacts_follow_their_flags() {
    let server = SiteServer::start();
    let dir = TestDir::new();

    // --trace on a failing flow saves trace.zip and lists it with the
    // failure.
    dir.file(
        "fail.whirl",
        "VISIT /second.html\n[Asserts]\ncss:\"#missing\" visible @500ms\n",
    );
    let output = run_whirl(&dir, &["--base", &server.base(), "--trace", "fail.whirl"]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 1, "stdout:\n{stdout}");
    assert!(
        dir.artifacts().join("fail/trace.zip").is_file(),
        "a failing traced flow should leave trace.zip"
    );

    // --trace, --video, and --har on a passing flow: no trace.zip, but
    // the video and network log exist.
    dir.file(
        "pass.whirl",
        "VISIT /second.html\n[Asserts]\nrole:heading \"Second page\" visible\n",
    );
    let output = run_whirl(&dir, &[
        "--base",
        &server.base(),
        "--trace",
        "--video",
        "--har",
        "pass.whirl",
    ]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    let pass_artifacts = dir.artifacts().join("pass");
    assert!(
        !pass_artifacts.join("trace.zip").exists(),
        "a passing flow saves no trace"
    );
    assert!(
        pass_artifacts.join("video.webm").is_file(),
        "--video should write video.webm"
    );
    assert!(
        pass_artifacts.join("network.har").is_file(),
        "--har should write network.har"
    );
}

#[test]
fn the_viewport_option_sizes_the_page() {
    let server = SiteServer::start();
    let dir = TestDir::new();
    // The configured viewport is observable from inside the page; the
    // eval capture surfaces it in the JSON report too.
    dir.file(
        "viewport.whirl",
        &format!(
            "[Options]\nbase: {base}\nviewport: 777x444\n\n\
             VISIT /second.html\n\
             EVAL \"document.title = window.innerWidth + 'x' + window.innerHeight\"\n\
             [Asserts]\ntitle == 777x444\n\
             [Captures]\nwidth: eval \"window.innerWidth\"\n",
            base = server.base()
        ),
    );
    let output = run_whirl(&dir, &["--report-json", "report.json", "viewport.whirl"]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    let report = fs::read_to_string(dir.path.join("report.json")).expect("report.json exists");
    let report: serde_json::Value = serde_json::from_str(&report).expect("valid JSON report");
    assert_eq!(
        report["files"][0]["entries"][0]["captures"]["width"], "777",
        "report:\n{report}"
    );
}

#[test]
fn fail_fast_stops_scheduling_after_the_first_failure() {
    let server = SiteServer::start();
    let dir = TestDir::new();
    dir.file(
        "a_fails.whirl",
        "VISIT /second.html\n[Asserts]\ncss:\"#missing\" visible @500ms\n",
    );
    dir.file(
        "b_later.whirl",
        "VISIT /second.html\n[Asserts]\nrole:heading \"Second page\" visible\n",
    );
    dir.file(
        "c_later.whirl",
        "VISIT /second.html\n[Asserts]\nrole:heading \"Second page\" visible\n",
    );
    // One worker runs the files in order: the first fails, so the later
    // files are never scheduled and never appear in the report.
    let output = run_whirl(&dir, &[
        "--base",
        &server.base(),
        "--fail-fast",
        "--jobs",
        "1",
        "a_fails.whirl",
        "b_later.whirl",
        "c_later.whirl",
    ]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 1, "stdout:\n{stdout}");
    assert!(stdout.contains("a_fails.whirl FAILED"), "stdout:\n{stdout}");
    assert!(!stdout.contains("b_later.whirl"), "stdout:\n{stdout}");
    assert!(!stdout.contains("c_later.whirl"), "stdout:\n{stdout}");
}

#[test]
fn all_engines_run_the_site_flow_when_requested() {
    // The full engine matrix needs firefox and webkit installed, so it
    // runs only in check:nightly (WHIRL_TEST_ALL_BROWSERS=1).
    if env::var_os("WHIRL_TEST_ALL_BROWSERS").is_none() {
        return;
    }
    let server = SiteServer::start();
    for engine in ["firefox", "webkit"] {
        let dir = TestDir::new();
        dir.file(
            "happy.whirl",
            &format!(
                "[Options]\nbase: {base}\n\n{HAPPY_FLOW_BODY}",
                base = server.base()
            ),
        );
        let output = run_whirl(&dir, &["--browser", engine, "happy.whirl"]);
        let stdout = stdout_text(&output);
        assert_eq!(exit_code(&output), 0, "{engine} stdout:\n{stdout}");
        assert!(
            stdout.contains("happy.whirl passed"),
            "{engine} stdout:\n{stdout}"
        );
    }
}
