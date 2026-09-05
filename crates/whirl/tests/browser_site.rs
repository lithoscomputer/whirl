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
fn respond(mut request: tiny_http::Request) {
    let url = request.url().to_owned();
    let path = url.split('?').next().unwrap_or("/").trim_start_matches('/');
    if path == "api/large" {
        let threshold = if url.contains("chunked") {
            1
        } else {
            usize::MAX
        };
        let _ = request.respond(
            Response::from_string("x".repeat(1_048_577)).with_chunked_threshold(threshold),
        );
        return;
    }
    if path == "user-agent" {
        let user_agent = request
            .headers()
            .iter()
            .find(|header| header.field.equiv("User-Agent"))
            .map_or("", |header| header.value.as_str())
            .to_owned();
        let _ = request.respond(Response::from_string(user_agent));
        return;
    }
    if path == "api/http-check" {
        let header = |name: &'static str| {
            request
                .headers()
                .iter()
                .find(|header| header.field.equiv(name))
                .map_or("", |header| header.value.as_str())
                .to_owned()
        };
        let cookie = header("Cookie");
        let authenticated = header("Authorization") == "Bearer whirl-test-key"
            || cookie.contains("session=browser");
        let mut body = String::new();
        request
            .as_reader()
            .read_to_string(&mut body)
            .expect("request body is readable");
        let json =
            serde_json::json!({"authenticated": authenticated, "cookie": cookie, "body": body});
        let _ = request.respond(
            Response::from_string(json.to_string())
                .with_status_code(if authenticated { 200 } else { 401 })
                .with_header(
                    Header::from_bytes("Set-Cookie", "http-session=changed; Path=/")
                        .expect("valid header"),
                ),
        );
        return;
    }
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
    if path == "api/orders" {
        if request.method().as_str() != "POST" {
            let _ = request.respond(Response::empty(405));
            return;
        }
        let status = request
            .headers()
            .iter()
            .find(|header| header.field.equiv("X-Test-Status"))
            .and_then(|header| header.value.as_str().parse::<u16>().ok())
            .unwrap_or(201);
        let content_type =
            Header::from_bytes("Content-Type", "application/json").expect("valid header");
        let body = r#"{"id":"order-42","status":"paid","active":true,"items":[{"id":"item-1"}],"a/b":{"~key":"escaped"},"none":null}"#;
        let _ = request.respond(
            Response::from_string(body)
                .with_status_code(status)
                .with_header(content_type),
        );
        return;
    }
    if path == "api/malformed" {
        let _ = request.respond(Response::from_string("not-json"));
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
    run_whirl_env(dir, args, &[])
}

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
fn user_agent_aliases_set_headers_and_navigator_without_changing_browser_or_viewport() {
    let dir = TestDir::new();
    let server = SiteServer::start();
    let chrome = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/151.0.7922.34 Safari/537.36";
    let firefox =
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:153.0) Gecko/20100101 Firefox/153.0";
    let safari = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.5 Safari/605.1.15";

    for (value, flag, expected) in [
        ("chrome", None, chrome),
        ("\"chrome\"", None, chrome),
        ("firefox", None, firefox),
        ("safari", None, safari),
        ("{{agent}}", None, firefox),
        ("chorme", None, "chorme"),
        ("Chrome", None, "Chrome"),
        ("\"Whirl/1 (custom)\"", None, "Whirl/1 (custom)"),
        ("\"Whirl/1 (custom)\"", Some("chrome"), chrome),
        ("chrome", Some("firefox"), firefox),
        ("chrome", Some("safari"), safari),
        ("chrome", Some("Whirl/1 (override)"), "Whirl/1 (override)"),
    ] {
        dir.file(
            "agent.whirl",
            &format!(
                "[Options]\nbase: {}\nviewport: 960x540\nuser-agent: {value}\n\
                 VISIT /user-agent\n\
                 [Captures]\n\
                 header: css:body text\n\
                 navigator: eval \"navigator.userAgent\"\n\
                 viewport: eval \"innerWidth + 'x' + innerHeight\"\n",
                server.base(),
            ),
        );
        let mut args = vec!["--report-json", "report.json", "--var", "agent=firefox"];
        if let Some(flag) = flag {
            args.extend(["--user-agent", flag]);
        }
        args.push("agent.whirl");
        let output = run_whirl(&dir, &args);
        assert_eq!(
            exit_code(&output),
            0,
            "{value}, {flag:?}: {}",
            stdout_text(&output)
        );
        let report: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(dir.path.join("report.json")).expect("report exists"),
        )
        .expect("report is JSON");
        let file = &report["files"][0];
        let captures = &file["entries"][0]["captures"];
        assert_eq!(captures["header"], expected, "{value}, {flag:?}");
        assert_eq!(captures["navigator"], expected, "{value}, {flag:?}");
        assert_eq!(captures["viewport"], "960x540");
        assert_eq!(file["runtime"]["userAgent"], expected);
        assert_eq!(file["runtime"]["browser"], "chromium");
        assert_eq!(
            file["runtime"]["viewport"],
            serde_json::json!({"width": 960, "height": 540})
        );
    }
}

#[test]
fn user_agent_runtime_metadata_masks_environment_values() {
    let dir = TestDir::new();
    let server = SiteServer::start();
    dir.file(
        "agent.whirl",
        "[Options]\nuser-agent: \"Whirl/{{env.WHIRL_TEST_SECRET}}\"\nVISIT /form.html\n",
    );
    let output = run_whirl_env(
        &dir,
        &[
            "--base",
            &server.base(),
            "--report-json",
            "report.json",
            "agent.whirl",
        ],
        &[("WHIRL_TEST_SECRET", "secret-agent")],
    );
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
    let text = fs::read_to_string(dir.path.join("report.json")).expect("report exists");
    assert!(
        !text.contains("secret-agent"),
        "user agent secrets must be masked"
    );
    let report: serde_json::Value = serde_json::from_str(&text).expect("report is JSON");
    assert_eq!(report["files"][0]["runtime"]["userAgent"], "Whirl/***");
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
    let state = dir.path.join("nested/state.json");
    let state_arg = "nested/state.json";

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
            "[Options]\nbase: {base}\nstorage: nested/state.json\n\n\
             VISIT /login.html\n[Asserts]\ncss:\"#status\" text == \"already logged in\"\n",
            base = server.base()
        ),
    );
    let output = run_whirl(&dir, &["reuse.whirl"]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    // CLI storage paths also resolve against the process working directory.
    let output = run_whirl(&dir, &["--storage", state_arg, "reuse.whirl"]);
    assert_eq!(exit_code(&output), 0, "stdout:\n{}", stdout_text(&output));
}

#[test]
fn a_setup_flow_runs_once_and_hands_state_and_captures_to_its_dependents() {
    let server = SiteServer::start();
    let dir = TestDir::new();
    // login.whirl signs in and captures the token. Two dependents start
    // from its saved state and read the capture as {{setup.token}}; the
    // login button was clicked once in that state, not once per file.
    dir.file(
        "login.whirl",
        &format!(
            "[Options]\nbase: {base}\n\n\
             VISIT /login.html\nCLICK \"Log in\"\n\
             [Asserts]\ncss:\"#status\" text == \"logged in\"\n\
             [Captures]\ntoken: css:\"#token\" text\n",
            base = server.base()
        ),
    );
    for name in ["a", "b"] {
        dir.file(
            &format!("{name}.whirl"),
            &format!(
                "[Options]\nbase: {base}\nsetup: login.whirl\n\n\
                 VISIT /login.html\n\
                 EVAL \"document.title = 'token={{{{setup.token}}}}'\"\n\
                 [Asserts]\n\
                 css:\"#status\" text == \"already logged in\"\n\
                 css:\"#logins\" text == 1\n\
                 title == token=t123\n",
                base = server.base()
            ),
        );
    }
    let output = run_whirl(&dir, &[
        "--report-json",
        "report.json",
        "a.whirl",
        "b.whirl",
    ]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    let report: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path.join("report.json")).expect("report"))
            .expect("valid JSON report");
    let paths: Vec<&str> = report["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|file| file["path"].as_str().expect("path"))
        .collect();
    assert_eq!(
        paths,
        vec!["login.whirl", "a.whirl", "b.whirl"],
        "report:\n{report}"
    );
    assert!(stdout.contains("login.whirl passed"), "stdout:\n{stdout}");

    assert_eq!(
        report["files"][0]["roles"],
        serde_json::json!({"requested": false, "setup": true})
    );
    assert_eq!(
        report["files"][1]["roles"],
        serde_json::json!({"requested": true, "setup": false})
    );
    assert_run_record(&report, &dir);

    // Naming the setup flow as an input too runs it once, as the setup.
    let output = run_whirl(&dir, &[
        "--report-json",
        "report.json",
        "./login.whirl",
        "a.whirl",
    ]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    let report: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path.join("report.json")).expect("report"))
            .expect("valid JSON report");
    assert_eq!(
        report["files"].as_array().expect("files").len(),
        2,
        "report:\n{report}"
    );
    assert_eq!(
        report["files"][0]["roles"],
        serde_json::json!({"requested": true, "setup": true})
    );
    assert_run_record(&report, &dir);
}

#[test]
fn a_failing_setup_flow_fails_its_dependents_without_running_them() {
    let server = SiteServer::start();
    let dir = TestDir::new();
    dir.file(
        "bad-login.whirl",
        &format!(
            "[Options]\nbase: {base}\nstep-timeout: 500ms\n\n\
             VISIT /login.html\n[Asserts]\ncss:\"#status\" text == \"never\"\n",
            base = server.base()
        ),
    );
    dir.file(
        "dependent.whirl",
        &format!(
            "[Options]\nbase: {base}\nsetup: bad-login.whirl\n\n\
             VISIT /login.html\n[Asserts]\ncss:\"#status\" text == \"already logged in\"\n",
            base = server.base()
        ),
    );
    let output = run_whirl(&dir, &["--report-json", "report.json", "dependent.whirl"]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 1, "stdout:\n{stdout}");
    assert!(
        stdout.contains("bad-login.whirl FAILED"),
        "stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("dependent.whirl FAILED"),
        "stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("setup flow 'bad-login.whirl' failed"),
        "stdout:\n{stdout}"
    );
    let report: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path.join("report.json")).expect("report"))
            .expect("valid JSON report");
    assert_eq!(
        report["files"][1]["entries"][0]["name"], "[setup]",
        "report:\n{report}"
    );
    assert_run_record(&report, &dir);
    assert_eq!(
        report["files"][1]["roles"],
        serde_json::json!({"requested": true, "setup": false})
    );
    assert!(report["files"][1].get("runtime").is_none());
    // The dependent never opened a browser: no failure screenshot.
    assert!(
        !dir.artifacts()
            .join("dependent")
            .join("failure.png")
            .exists(),
        "the dependent should not have run"
    );
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

#[test]
fn frame_locators_fill_assert_and_capture_across_origins_and_nested_frames() {
    let site = SiteServer::start();
    let dir = TestDir::new();
    dir.file(
        "frames.whirl",
        &format!(
            r##"[Options]
base: {}
VISIT /frames.html
FILL css:"#payments" >> frame:iframe >> label:Email alice@example.com
TYPE frame:"#payment" >> label:Code 4242
CHECK frame:"#payment" >> label:Notifications
FILL frame:"#nested" >> frame:iframe >> label:Email nested@example.com
[Asserts]
frame:"#payment" >> label:Email value == alice@example.com
frame:"#payment" >> label:Notifications checked
frame:"#payment" >> css:"#typed-keys" text == 4242
frame:"#nested" >> frame:iframe >> label:Email value == nested@example.com
[Captures]
email: frame:"#payment" >> label:Email value
FILL frame:iframe >> nth:1 >> label:Email {{{{email}}}}
[Asserts]
frame:iframe >> nth:1 >> label:Email value == alice@example.com
"##,
            site.base()
        ),
    );
    let output = run_whirl(&dir, &["frames.whirl"]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
}

#[test]
fn frame_locators_wait_for_a_frame_created_after_the_action() {
    let site = SiteServer::start();
    let dir = TestDir::new();
    dir.file(
        "delayed.whirl",
        &format!(
            r##"[Options]
base: {}
VISIT /frames.html
CLICK role:button "Load frame"
FILL frame:"#delayed" >> label:Email late@example.com
[Asserts]
frame:"#delayed" >> label:Email value == late@example.com
"##,
            site.base()
        ),
    );
    let output = run_whirl(&dir, &["delayed.whirl"]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
}

#[test]
fn frame_locators_reject_multiple_matching_frames() {
    let site = SiteServer::start();
    let dir = TestDir::new();
    dir.file(
        "ambiguous.whirl",
        &format!(
            r"[Options]
base: {}
VISIT /frames.html
FILL frame:iframe >> label:Email wrong@example.com @1s
",
            site.base()
        ),
    );
    let output = run_whirl(&dir, &["ambiguous.whirl"]);
    assert_eq!(exit_code(&output), 1, "{}", stdout_text(&output));
    assert!(
        stdout_text(&output).contains("strict"),
        "{}",
        stdout_text(&output)
    );
}

#[test]
fn popups_are_named_without_switching_and_return_after_self_closure() {
    let site = SiteServer::start();
    let dir = TestDir::new();
    dir.file(
        "popup.whirl",
        &format!(
            r#"[Options]
base: {}
VISIT /popups.html
CLICK role:button "Pay with provider"
POPUP payment
[Asserts]
role:heading Checkout visible
TAB payment
FILL label:Name Alice
[Asserts]
role:heading "Confirm payment" visible
[Captures]
name: label:Name value
CLICK role:button Alert
[Asserts]
role:button Dismissed visible
CLICK role:button Confirm
[Asserts]
tab:payment closed @5s
TAB main
[Asserts]
text:"Payment complete" visible
FILL label:Customer {{{{name}}}}
[Asserts]
label:Customer value == Alice
"#,
            site.base()
        ),
    );
    let output = run_whirl(&dir, &["--trace", "popup.whirl"]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
}

#[test]
fn popup_closure_before_click_delivery_fails_the_action() {
    let site = SiteServer::start();
    let dir = TestDir::new();
    dir.file(
        "early-close.whirl",
        &format!(
            r#"[Options]
base: {}
VISIT /popups.html
CLICK role:button "Pay with provider"
POPUP payment
TAB payment
EVAL "setTimeout(() => window.close(), 50)"
CLICK role:button Unavailable
"#,
            site.base()
        ),
    );
    let output = run_whirl(&dir, &["early-close.whirl"]);
    assert_eq!(exit_code(&output), 1, "{}", stdout_text(&output));
    assert!(
        stdout_text(&output).contains("step: CLICK role:button Unavailable"),
        "{}",
        stdout_text(&output)
    );
    assert!(
        stdout_text(&output).contains("closed"),
        "{}",
        stdout_text(&output)
    );
}

#[test]
fn named_tabs_support_nested_popups_and_explicit_close() {
    let site = SiteServer::start();
    let dir = TestDir::new();
    dir.file(
        "nested.whirl",
        &format!(
            r#"[Options]
base: {}
VISIT /popups.html
CLICK role:button "Pay with provider"
POPUP payment
TAB payment
CLICK role:button Receipt
POPUP receipt
TAB receipt
PAGE /second.html
CLOSE receipt
[Asserts]
tab:receipt closed
TAB payment
CLOSE payment
TAB main
[Asserts]
role:heading Checkout visible
"#,
            site.base()
        ),
    );
    let output = run_whirl(&dir, &["nested.whirl"]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
}

#[test]
fn a_popup_from_an_earlier_entry_does_not_satisfy_popup() {
    let site = SiteServer::start();
    let dir = TestDir::new();
    dir.file(
        "stale.whirl",
        &format!(
            r#"[Options]
base: {}
VISIT /popups.html
CLICK role:button "Pay with provider"
[Asserts]
text:"Provider ready" visible
POPUP stale @300ms
"#,
            site.base()
        ),
    );
    let output = run_whirl(&dir, &["stale.whirl"]);
    assert_eq!(exit_code(&output), 1, "{}", stdout_text(&output));
    assert!(
        stdout_text(&output).contains("no popup"),
        "{}",
        stdout_text(&output)
    );
}

#[test]
fn selecting_a_closed_tab_fails_without_switching_implicitly() {
    let site = SiteServer::start();
    let dir = TestDir::new();
    dir.file(
        "closed.whirl",
        &format!(
            r#"[Options]
base: {}
VISIT /popups.html
CLICK role:button "Pay with provider"
POPUP payment
CLOSE payment
TAB payment
"#,
            site.base()
        ),
    );
    let output = run_whirl(&dir, &["closed.whirl"]);
    assert_eq!(exit_code(&output), 1, "{}", stdout_text(&output));
    assert!(
        stdout_text(&output).contains("tab payment is closed"),
        "{}",
        stdout_text(&output)
    );
}

#[test]
fn multiple_unnamed_popups_fail_strictly() {
    let site = SiteServer::start();
    let dir = TestDir::new();
    dir.file("multiple.whirl", &format!(r#"[Options]
base: {}
VISIT /popups.html
CLICK role:button "Open two"
EVAL "await new Promise(resolve => {{ const observer = new MutationObserver(() => {{ if (document.body.dataset.popups === '2') {{ observer.disconnect(); resolve(); }} }}); if (document.body.dataset.popups === '2') resolve(); else observer.observe(document.body, {{attributes: true}}); }})"
POPUP payment @1s
"#, site.base()));
    let output = run_whirl(&dir, &["multiple.whirl"]);
    assert_eq!(exit_code(&output), 1, "{}", stdout_text(&output));
    assert!(
        stdout_text(&output).contains("multiple unnamed popups"),
        "{}",
        stdout_text(&output)
    );
}

#[test]
fn response_assertions_and_captures_observe_the_request_before_click_returns() {
    let site = SiteServer::start();
    let dir = TestDir::new();
    dir.file(
        "response.whirl",
        &format!(
            r#"[Options]
base: {}
VISIT /network.html
EVAL "await fetch('/api/orders')"
CLICK role:button "Place order"
EVAL "await window.orderRequest"
RESPONSE order POST /api/orders
[Asserts]
response:order status == 201
response:order status >= 200
response:order status < 300
response:order header:Content-Type contains application/json
response:order json:/status == paid
response:order json:/active == true
response:order json:/none == null
response:order json:/a~1b/~0key == escaped
response:order json:/items/0/id matches /^item-/
text:"Order confirmed" visible
[Captures]
order_id: response:order json:/id
item_number: response:order json:/items/0/id regex /item-(\d+)/
VISIT /network.html?id={{{{order_id}}}}&item={{{{item_number}}}}
PAGE /network.html?id=order-42&item=1
[Asserts]
response:order json:/status == paid
"#,
            site.base()
        ),
    );
    let output = run_whirl(&dir, &["--report-json", "report.json", "response.whirl"]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
    let report = fs::read_to_string(dir.path.join("report.json")).expect("report exists");
    assert!(report.contains("order-42"));
}

#[test]
fn response_selection_does_not_replace_a_failed_request_with_a_successful_retry() {
    let site = SiteServer::start();
    let dir = TestDir::new();
    dir.file(
        "retry.whirl",
        &format!(
            r#"[Options]
base: {}
VISIT /network.html
CLICK role:button "Retry order"
EVAL "await window.orderRequest"
RESPONSE order POST /api/orders
[Asserts]
response:order status == 201
"#,
            site.base()
        ),
    );
    let output = run_whirl(&dir, &["retry.whirl"]);
    assert_eq!(exit_code(&output), 1, "{}", stdout_text(&output));
    assert!(
        stdout_text(&output).contains("500"),
        "{}",
        stdout_text(&output)
    );
}

#[test]
fn response_selection_excludes_requests_from_previous_entries() {
    let site = SiteServer::start();
    let dir = TestDir::new();
    dir.file(
        "stale-response.whirl",
        &format!(
            r#"[Options]
base: {}
VISIT /network.html
CLICK role:button "Place order"
[Asserts]
text:"Order confirmed" visible
RESPONSE stale POST /api/orders @300ms
"#,
            site.base()
        ),
    );
    let output = run_whirl(&dir, &["stale-response.whirl"]);
    assert_eq!(exit_code(&output), 1, "{}", stdout_text(&output));
    assert!(
        stdout_text(&output).contains("no matching request"),
        "{}",
        stdout_text(&output)
    );
}

#[test]
fn responses_from_cross_origin_frames_and_popup_navigation_are_observed() {
    let site = SiteServer::start();
    let dir = TestDir::new();
    let cross = site.base().replace("127.0.0.1", "localhost");
    dir.file(
        "contexts.whirl",
        &format!(
            r##"[Options]
base: {}
VISIT /network.html
CLICK frame:"#checkout" >> role:button "Place order"
RESPONSE embedded POST {cross}/api/orders
[Asserts]
response:embedded status == 201
CLICK role:button "Open checkout"
POPUP checkout
TAB checkout
RESPONSE navigation GET /network.html?embedded=1
[Asserts]
response:navigation status == 200
CLICK role:button "Place order"
RESPONSE order POST /api/orders
[Asserts]
response:order json:/id == order-42
CLOSE checkout
[Asserts]
tab:checkout closed
response:order json:/status == paid
TAB main
"##,
            site.base()
        ),
    );
    let engines: &[&str] = if env::var_os("WHIRL_TEST_ALL_BROWSERS").is_some() {
        &["chromium", "firefox", "webkit"]
    } else {
        &["chromium"]
    };
    for engine in engines {
        let output = run_whirl(&dir, &["--browser", engine, "contexts.whirl"]);
        assert_eq!(exit_code(&output), 0, "{engine}: {}", stdout_text(&output));
    }
}

#[test]
fn response_selection_does_not_use_another_tabs_requests() {
    let site = SiteServer::start();
    let dir = TestDir::new();
    dir.file(
        "wrong-tab.whirl",
        &format!(
            r#"[Options]
base: {}
VISIT /network.html
CLICK role:button "Place order"
EVAL "await window.orderRequest"
CLICK role:button "Open checkout"
POPUP checkout
TAB checkout
RESPONSE wrong POST /api/orders @300ms
"#,
            site.base()
        ),
    );
    let output = run_whirl(&dir, &["wrong-tab.whirl"]);
    assert_eq!(exit_code(&output), 1, "{}", stdout_text(&output));
    assert!(
        stdout_text(&output).contains("no matching request"),
        "{}",
        stdout_text(&output)
    );
}

#[test]
fn missing_json_fields_do_not_pass_inequality_assertions() {
    let site = SiteServer::start();
    let dir = TestDir::new();
    dir.file(
        "missing.whirl",
        &format!(
            r#"[Options]
base: {}
VISIT /network.html
CLICK role:button "Place order"
RESPONSE order POST /api/orders
[Asserts]
response:order json:/missing != paid
"#,
            site.base()
        ),
    );
    let output = run_whirl(&dir, &["missing.whirl"]);
    assert_eq!(exit_code(&output), 1, "{}", stdout_text(&output));
    assert!(
        stdout_text(&output).contains("does not exist"),
        "{}",
        stdout_text(&output)
    );
}

#[test]
fn absent_headers_and_malformed_json_fail_response_assertions() {
    let site = SiteServer::start();
    for (check, diagnostic) in [
        (
            "header:x-missing != present",
            "response header x-missing is absent",
        ),
        ("json:/status != paid", "JSON"),
    ] {
        let dir = TestDir::new();
        dir.file(
            "invalid-response.whirl",
            &format!(
                r#"[Options]
base: {}
VISIT /network.html
EVAL "await fetch('/api/malformed')"
RESPONSE invalid GET /api/malformed
[Asserts]
response:invalid {check}
"#,
                site.base()
            ),
        );
        let output = run_whirl(&dir, &["invalid-response.whirl"]);
        assert_eq!(exit_code(&output), 1, "{}", stdout_text(&output));
        assert!(
            stdout_text(&output).contains(diagnostic),
            "{}",
            stdout_text(&output)
        );
    }
}

#[test]
fn failed_network_requests_report_failure_instead_of_waiting_for_a_retry() {
    let site = SiteServer::start();
    let dir = TestDir::new();
    dir.file(
        "failed-request.whirl",
        &format!(
            r#"[Options]
base: {}
allow-hosts: 127.0.0.1
VISIT /network.html
EVAL "void fetch('https://blocked.invalid/fail').catch(() => {{}})"
RESPONSE rejected GET https://blocked.invalid/fail @2s
"#,
            site.base()
        ),
    );
    let output = run_whirl(&dir, &["failed-request.whirl"]);
    assert_eq!(exit_code(&output), 1, "{}", stdout_text(&output));
    assert!(
        stdout_text(&output).contains("request for response rejected failed"),
        "{}",
        stdout_text(&output)
    );
}

#[test]
fn waiting_for_response_headers_obeys_the_step_timeout() {
    let site = SiteServer::start();
    let dir = TestDir::new();
    dir.file(
        "slow-response.whirl",
        &format!(
            r#"[Options]
base: {}
VISIT /network.html
EVAL "void fetch('/stall').catch(() => {{}})"
RESPONSE stalled GET /stall @200ms
"#,
            site.base()
        ),
    );
    let output = run_whirl(&dir, &["slow-response.whirl"]);
    assert_eq!(exit_code(&output), 1, "{}", stdout_text(&output));
    assert!(
        stdout_text(&output).contains("tim"),
        "{}",
        stdout_text(&output)
    );
}

#[test]
fn http_authentication_uses_explicit_headers_without_browser_cookies() {
    let site = SiteServer::start();
    let dir = TestDir::new();
    dir.file("http.whirl", r#"VISIT /network.html
STORE cookie session browser
HTTP authorized POST /api/http-check header:Authorization "Bearer whirl-test-key" header:Content-Type application/json body:"{\"message\":\"hello\"}"
[Asserts]
response:authorized status == 200
response:authorized json:/authenticated == true
response:authorized json:/cookie == ""
response:authorized json:/body == "{\"message\":\"hello\"}"
[Captures]
authenticated: response:authorized json:/authenticated
HTTP unauthorized GET /api/http-check
[Asserts]
response:unauthorized status == 401
response:unauthorized json:/cookie == ""
EVAL "if (document.cookie !== 'session=browser') throw new Error('HTTP changed browser cookies')"
VISIT /network.html?authenticated={{authenticated}}
PAGE /network.html?authenticated=true
"#);
    let output = run_whirl(&dir, &["--base", &site.base(), "http.whirl"]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
}

#[test]
fn http_returns_redirects_without_following_them() {
    let site = SiteServer::start();
    let dir = TestDir::new();
    dir.file("redirect.whirl", "VISIT /network.html\nHTTP redirect GET /redirect-cross\n[Asserts]\nresponse:redirect status == 302\nresponse:redirect header:location contains localhost\n");
    let output = run_whirl(&dir, &["--base", &site.base(), "redirect.whirl"]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
}

#[test]
fn http_enforces_host_allowlists_before_sending_a_request() {
    let site = SiteServer::start();
    let dir = TestDir::new();
    dir.file("blocked.whirl", "[Options]\nallow-hosts: 127.0.0.1\nVISIT /network.html\nHTTP blocked GET https://blocked.invalid/\n");
    let output = run_whirl(&dir, &["--base", &site.base(), "blocked.whirl"]);
    assert_eq!(exit_code(&output), 1, "{}", stdout_text(&output));
    assert!(
        stdout_text(&output).contains("HTTP host blocked.invalid is blocked"),
        "{}",
        stdout_text(&output)
    );
}

#[test]
fn http_enforces_its_step_timeout() {
    let site = SiteServer::start();
    let dir = TestDir::new();
    dir.file(
        "timeout.whirl",
        "VISIT /network.html\nHTTP slow GET /stall @200ms\n",
    );
    let output = run_whirl(&dir, &["--base", &site.base(), "timeout.whirl"]);
    assert_eq!(exit_code(&output), 1, "{}", stdout_text(&output));
    assert!(
        stdout_text(&output).contains("timeout"),
        "{}",
        stdout_text(&output)
    );
}

#[test]
fn http_limits_declared_and_streamed_response_bodies() {
    let site = SiteServer::start();
    let dir = TestDir::new();
    for path in ["/api/large", "/api/large?chunked"] {
        dir.file(
            "large.whirl",
            &format!("VISIT /network.html\nHTTP large GET {path}\n"),
        );
        let output = run_whirl(&dir, &["--base", &site.base(), "large.whirl"]);
        assert_eq!(exit_code(&output), 1, "{path}: {}", stdout_text(&output));
        assert!(
            stdout_text(&output).contains("HTTP response exceeds the 1 MiB body limit"),
            "{path}: {}",
            stdout_text(&output)
        );
    }
}

#[test]
fn http_head_allows_a_large_content_length_without_a_response_body() {
    let site = SiteServer::start();
    let dir = TestDir::new();
    dir.file(
        "head.whirl",
        "VISIT /network.html\nHTTP large HEAD /api/large\n[Asserts]\n\
         response:large status == 200\nresponse:large header:content-length == 1048577\n",
    );
    let output = run_whirl(&dir, &["--base", &site.base(), "head.whirl"]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
}

#[test]
fn http_rejects_non_http_urls_and_embedded_credentials() {
    let site = SiteServer::start();
    let dir = TestDir::new();
    for url in ["data:text/plain,example", "http://user:password@127.0.0.1/"] {
        dir.file(
            "url.whirl",
            &format!("VISIT /network.html\nHTTP invalid GET {url}\n"),
        );
        let output = run_whirl(&dir, &["--base", &site.base(), "url.whirl"]);
        assert_eq!(exit_code(&output), 1, "{}", stdout_text(&output));
        assert!(
            stdout_text(&output).contains("HTTP or HTTPS URL without embedded credentials"),
            "{}",
            stdout_text(&output)
        );
    }
}

#[test]
fn http_interpolates_headers_and_bodies_without_leaking_secrets() {
    let site = SiteServer::start();
    let dir = TestDir::new();
    dir.file(
        "secret.whirl",
        "VISIT /network.html\n\
         HTTP account POST {{endpoint}} header:Authorization \"Bearer {{env.HTTP_TOKEN}}\" body:{{env.HTTP_BODY}}\n\
         [Asserts]\nresponse:account status == 200\n\
         response:account json:/body == {{env.HTTP_BODY}}\n\
         [Captures]\nbody: response:account json:/body\n",
    );
    let output = run_whirl_env(
        &dir,
        &[
            "--base",
            &site.base(),
            "--var",
            "endpoint=/api/http-check",
            "--report-json",
            "report.json",
            "secret.whirl",
        ],
        &[
            ("HTTP_TOKEN", "whirl-test-key"),
            ("HTTP_BODY", "secret-request-body"),
        ],
    );
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
    let report = fs::read_to_string(dir.path.join("report.json")).expect("report exists");
    for secret in ["whirl-test-key", "secret-request-body"] {
        assert!(!report.contains(secret), "report must mask HTTP secrets");
        assert!(!stdout_text(&output).contains(secret));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
    }
    let report: serde_json::Value = serde_json::from_str(&report).expect("report is JSON");
    assert_eq!(report["files"][0]["entries"][0]["captures"]["body"], "***");
}

#[test]
fn html_report_is_portable_and_preserves_results_and_author_context() {
    let dir = TestDir::new();
    let server = SiteServer::start();
    dir.file("pass.whirl", "# Inspect the page.\nVISIT /stable.html\nSCREENSHOT page\n[Asserts]\ntitle == \"Stable Page\"\n");
    dir.file("fail.whirl", "# A failed check.\nVISIT /stable.html\n[Asserts]\ntitle == Wrong @100ms\n# Never executed.\nVISIT /form.html\n");
    fs::create_dir(dir.path.join("context")).expect("metadata directory");
    dir.file("context/report.json", r#"{
        "title": "Critical browser evidence",
        "description": "Local services.\nAuthor-written scope.",
        "files": {"../pass.whirl": {"title": "Page access", "description": "Open the page and verify its title."}}
    }"#);
    let output = run_whirl(&dir, &[
        "--base",
        &server.base(),
        "--video",
        "--report-html",
        "report.html",
        "--report-json",
        "report.json",
        "--report-junit",
        "junit.xml",
        "--report-metadata",
        "context/report.json",
        "pass.whirl",
        "fail.whirl",
    ]);
    assert_eq!(exit_code(&output), 1, "{}", stdout_text(&output));
    let html = fs::read_to_string(dir.path.join("report.html")).expect("HTML report");
    assert!(html.contains("data:video/webm;base64,"));
    assert!(html.contains("data:image/png;base64,"));
    assert!(html.contains("Critical browser evidence"));
    assert!(html.contains("Never executed."));
    let json: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(dir.path.join("report.json")).expect("JSON report"),
    )
    .expect("valid JSON");
    assert_eq!(
        json["metadata"]["files"]["pass.whirl"]["title"],
        "Page access"
    );
    assert!(
        json["files"]
            .as_array()
            .expect("files")
            .iter()
            .any(|file| file["path"] == "fail.whirl" && file["status"] == "failed")
    );
    assert!(dir.path.join("junit.xml").is_file());

    // The saved command must produce identical HTML after sources disappear,
    // even from another directory and without an available browser runtime.
    fs::remove_file(dir.path.join("pass.whirl")).expect("remove flow");
    fs::remove_file(dir.path.join("fail.whirl")).expect("remove flow");
    fs::create_dir(dir.path.join("saved")).expect("saved directory");
    fs::copy(
        dir.path.join("report.json"),
        dir.path.join("saved/report.json"),
    )
    .expect("copy JSON");
    let saved = Command::new(env!("CARGO_BIN_EXE_whirl"))
        .current_dir(dir.path.join("saved"))
        .env("WHIRL_NODE", "/missing/runtime")
        .env("WHIRL_SHIM_JS", "/missing/shim")
        .args(["report", "report.json", "--html", "evidence.html"])
        .output()
        .expect("saved report command");
    assert_eq!(exit_code(&saved), 0, "{saved:?}");
    assert_eq!(
        fs::read_to_string(dir.path.join("saved/evidence.html")).expect("saved HTML"),
        html
    );
    fs::copy(
        dir.path.join("saved/evidence.html"),
        dir.path.join("report.html"),
    )
    .expect("use saved HTML");

    // A different directory and no source artifacts: the browser must decode
    // the embedded media and retain all results without a server or sidecars.
    fs::create_dir(dir.path.join("moved")).expect("moved directory");
    let moved = dir.path.join("moved/evidence.html");
    fs::rename(dir.path.join("report.html"), &moved).expect("move report");
    fs::remove_dir_all(dir.artifacts()).expect("remove source media");
    let url = reqwest::Url::from_file_path(&moved).expect("file URL");
    dir.file("verify.whirl", &format!(r#"VISIT "{url}"
[Asserts]
role:heading "Critical browser evidence" visible
role:heading "Page access" visible
css:article count == 2
css:article[data-status=passed] count == 1
css:article[data-status=failed] count == 1
css:.entry[data-status=skipped] count == 1
EVAL "const v = document.querySelector('video'); await v.play(); await new Promise((resolve, reject) => {{ if (v.videoWidth > 0) resolve(); else {{ v.addEventListener('loadeddata', resolve, {{once:true}}); v.addEventListener('error', () => reject(new Error('Video failed')), {{once:true}}); }} }}); if (!v.videoWidth) throw new Error('No video pixels'); v.pause();"
EVAL "for (const d of document.querySelectorAll('details')) d.open = true; for (const img of document.images) {{ img.loading = 'eager'; await img.decode(); if (!img.naturalWidth) throw new Error('No screenshot pixels'); }}"
EVAL "if (document.documentElement.scrollWidth > innerWidth) throw new Error('Report overflows viewport')"
"#));
    let verify = run_whirl(&dir, &["verify.whirl"]);
    assert_eq!(exit_code(&verify), 0, "{}", stdout_text(&verify));
    let source = fs::read_to_string(dir.path.join("verify.whirl")).expect("verification flow");
    dir.file(
        "mobile.whirl",
        &format!("[Options]\nviewport: 390x844\n{source}"),
    );
    let mobile = run_whirl(&dir, &["mobile.whirl"]);
    assert_eq!(exit_code(&mobile), 0, "{}", stdout_text(&mobile));
}

#[test]
fn html_report_escapes_hostile_text_and_keeps_environment_values_masked() {
    let dir = TestDir::new();
    let server = SiteServer::start();
    let hostile = "</title><script>document.body.dataset.injected='yes'</script><img src=x onerror=alert(1)> & \"quoted\"";
    dir.file("context.json", &serde_json::json!({"title": hostile, "description": hostile, "files": {"flow.whirl": {"title": hostile, "description": hostile}}}).to_string());
    dir.file("flow.whirl", &format!("# {hostile}\nVISIT /form.html\nEVAL \"document.title = 'safe'\"\n[Asserts]\ntitle == {{{{env.WHIRL_TEST_SECRET}}}} @100ms\n"));
    let output = run_whirl_env(
        &dir,
        &[
            "--base",
            &server.base(),
            "--report-html",
            "report.html",
            "--report-metadata",
            "context.json",
            "flow.whirl",
        ],
        &[("WHIRL_TEST_SECRET", "never-show-this-secret")],
    );
    assert_eq!(exit_code(&output), 1, "{}", stdout_text(&output));
    let html = fs::read_to_string(dir.path.join("report.html")).expect("HTML report");
    assert!(!html.contains("never-show-this-secret"));
    assert!(!html.contains("<script>"));
    assert!(!html.contains("<img src=x"));
    assert!(html.contains("&lt;script&gt;"));
    assert!(html.contains("&amp; &quot;quoted&quot;"));
    assert!(html.contains("Recording not requested."));
    let url = reqwest::Url::from_file_path(dir.path.join("report.html")).expect("file URL");
    dir.file("verify.whirl", &format!("VISIT \"{url}\"\n[Asserts]\ncss:script count == 0\ncss:body attr:data-injected != yes\ncss:article[data-status=failed] count == 1\n"));
    let verify = run_whirl(&dir, &["verify.whirl"]);
    assert_eq!(exit_code(&verify), 0, "{}", stdout_text(&verify));
}

#[test]
fn html_output_errors_preserve_other_reports_and_do_not_replace_inputs() {
    let dir = TestDir::new();
    let server = SiteServer::start();
    let flow = "VISIT /stable.html\n";
    dir.file("flow.whirl", flow);
    let conflict = run_whirl(&dir, &[
        "--base",
        &server.base(),
        "--report-html",
        "flow.whirl",
        "flow.whirl",
    ]);
    assert_eq!(exit_code(&conflict), 4);
    assert_eq!(
        fs::read_to_string(dir.path.join("flow.whirl")).expect("flow"),
        flow
    );
    let collision = run_whirl(&dir, &[
        "--report-html",
        "same",
        "--report-json",
        "./same",
        "flow.whirl",
    ]);
    assert_eq!(exit_code(&collision), 4);
    let output = run_whirl(&dir, &[
        "--base",
        &server.base(),
        "--report-html",
        "missing/report.html",
        "--report-json",
        "report.json",
        "flow.whirl",
    ]);
    assert_eq!(exit_code(&output), 3);
    assert!(dir.path.join("report.json").is_file());
    assert!(!dir.path.join("missing/report.html").exists());
    // An existing directory cannot be replaced by the temporary report.
    fs::create_dir(dir.path.join("existing.html")).expect("destination directory");
    dir.file("existing.html/keep", "untouched");
    let output = run_whirl(&dir, &[
        "--base",
        &server.base(),
        "--report-html",
        "existing.html",
        "flow.whirl",
    ]);
    assert_eq!(exit_code(&output), 3);
    assert_eq!(
        fs::read_to_string(dir.path.join("existing.html/keep")).expect("existing content"),
        "untouched"
    );
}

#[test]
fn html_report_marks_runtime_failure_and_missing_recording_separately() {
    let dir = TestDir::new();
    dir.file("flow.whirl", "VISIT /stable.html\n");
    let output = run_whirl_env(
        &dir,
        &["--video", "--report-html", "report.html", "flow.whirl"],
        &[("WHIRL_NODE", "/definitely/missing/node")],
    );
    assert_eq!(exit_code(&output), 3);
    let html = fs::read_to_string(dir.path.join("report.html")).expect("HTML report");
    assert!(html.contains("data-status=\"error\""));
    assert!(html.contains("Recording unavailable."));
    assert!(!html.contains("<video"));
}

#[test]
fn html_metadata_errors_preempt_execution_and_preserve_inputs() {
    let dir = TestDir::new();
    dir.file("flow.whirl", "VISIT /\n");
    for content in [
        "{broken",
        r#"{"title": 5}"#,
        r#"{"status": "passed"}"#,
        r#"{"files": {"flow.whirl": {"status": "passed"}}}"#,
        r#"{"files": {"missing.whirl": {"title": "Missing"}}}"#,
        r#"{"files": {"flow.whirl": {}, "./flow.whirl": {}}}"#,
        r#"{"files": {"flow.whirl": {}, "flow.whirl": {}}}"#,
    ] {
        dir.file("metadata.json", content);
        let output = run_whirl(&dir, &[
            "--report-html",
            "report.html",
            "--report-metadata",
            "metadata.json",
            "flow.whirl",
        ]);
        assert_eq!(exit_code(&output), 4, "metadata: {content}");
        assert!(!dir.path.join("report.html").exists());
        assert!(
            !dir.artifacts().exists(),
            "invalid metadata must not start a run"
        );
        assert_eq!(
            fs::read_to_string(dir.path.join("metadata.json")).expect("metadata input"),
            content
        );
    }
}

#[test]
fn html_missing_screenshot_does_not_turn_a_pass_into_a_failure() {
    let dir = TestDir::new();
    dir.file(
        "flow.whirl",
        "VISIT https://example.test/\nSCREENSHOT absent\n",
    );
    let shim = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_shim.js");
    let output = run_whirl_env(
        &dir,
        &["--video", "--report-html", "report.html", "flow.whirl"],
        &[("WHIRL_SHIM_JS", shim.to_str().expect("shim path"))],
    );
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
    let html = fs::read_to_string(dir.path.join("report.html")).expect("HTML report");
    assert!(html.contains("data-status=\"passed\""));
    assert!(html.contains("Screenshot unavailable:"));
    assert!(html.contains("Recording unavailable."));
    assert!(html.contains("Blocked hosts: a.example, b.example"));
}

#[test]
#[cfg(unix)]
fn html_report_rejects_artifact_symlinks_and_invalid_media() {
    use std::os::unix::fs::symlink;

    let dir = TestDir::new();
    dir.file(
        "flow.whirl",
        "VISIT https://example.test/\nSCREENSHOT image\n",
    );
    let artifact_dir = dir.artifacts().join("flow");
    fs::create_dir_all(&artifact_dir).expect("artifact directory");
    let external = dir.path.join("private.png");
    fs::write(&external, b"\x89PNG\r\n\x1a\nPRIVATE-OUTSIDE-ARTIFACTS").expect("external file");
    let artifact = artifact_dir.join("image.png");
    symlink(&external, &artifact).expect("artifact symlink");
    let shim = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_shim.js");
    let run = || {
        run_whirl_env(&dir, &["--report-html", "report.html", "flow.whirl"], &[(
            "WHIRL_SHIM_JS",
            shim.to_str().expect("shim path"),
        )])
    };
    let output = run();
    assert_eq!(exit_code(&output), 0);
    let html = fs::read_to_string(dir.path.join("report.html")).expect("HTML report");
    assert!(html.contains("artifact is outside its flow directory"));
    assert!(!html.contains("data:image/png;base64,"));
    fs::remove_file(&artifact).expect("remove symlink");
    fs::write(&artifact, b"<svg onload=alert(1)>").expect("invalid image");
    let output = run();
    assert_eq!(exit_code(&output), 0);
    let html = fs::read_to_string(dir.path.join("report.html")).expect("HTML report");
    assert!(html.contains("artifact has an invalid media header"));
    assert!(!html.contains("data:image/png;base64,"));
    assert_eq!(
        fs::read(&external).expect("external preserved"),
        b"\x89PNG\r\n\x1a\nPRIVATE-OUTSIDE-ARTIFACTS"
    );
}

#[test]
#[cfg(unix)]
fn html_metadata_matches_symlinked_flows_and_ignores_unselected_flows() {
    use std::os::unix::fs::symlink;

    let dir = TestDir::new();
    let server = SiteServer::start();
    dir.file("flow.whirl", "VISIT /stable.html\n");
    dir.file("unused.whirl", "VISIT /form.html\n");
    symlink("flow.whirl", dir.path.join("alias.whirl")).expect("flow alias");
    dir.file("metadata.json", r#"{"files":{"flow.whirl":{"title":"Canonical title"},"unused.whirl":{"title":"Must not appear"}}}"#);
    let output = run_whirl(&dir, &[
        "--base",
        &server.base(),
        "--report-html",
        "report.html",
        "--report-json",
        "report.json",
        "--report-metadata",
        "metadata.json",
        "alias.whirl",
    ]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
    let html = fs::read_to_string(dir.path.join("report.html")).expect("HTML report");
    assert!(html.contains("Canonical title"));
    assert!(!html.contains("Must not appear"));
    let json: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(dir.path.join("report.json")).expect("JSON report"),
    )
    .expect("valid JSON");
    let path = json["files"][0]["path"].as_str().expect("flow path");
    assert_eq!(json["metadata"]["files"][path]["title"], "Canonical title");
    assert_eq!(
        json["metadata"]["files"]
            .as_object()
            .expect("metadata files")
            .len(),
        1
    );
    let conflict = run_whirl(&dir, &["--report-html", "flow.whirl", "alias.whirl"]);
    assert_eq!(exit_code(&conflict), 4);
}

#[test]
fn html_report_does_not_replace_a_recorded_artifact() {
    let dir = TestDir::new();
    let server = SiteServer::start();
    dir.file("flow.whirl", "VISIT /stable.html\nSCREENSHOT image\n");
    let output = run_whirl(&dir, &[
        "--base",
        &server.base(),
        "--report-html",
        "artifacts/flow/image.png",
        "flow.whirl",
    ]);
    assert_eq!(exit_code(&output), 3);
    let image = fs::read(dir.artifacts().join("flow/image.png")).expect("screenshot remains");
    assert!(image.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert!(!String::from_utf8_lossy(&image).contains("<!doctype html>"));
}

/// Check wall-clock ordering and SHA-256 against the flow bytes on disk.
fn assert_run_record(report: &serde_json::Value, dir: &TestDir) {
    use chrono::{DateTime, FixedOffset};
    use sha2::{Digest as _, Sha256};
    let timestamp = |value: &serde_json::Value| -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(value.as_str().expect("timestamp")).expect("RFC3339 timestamp")
    };
    let start = timestamp(&report["startedAt"]);
    let finish = timestamp(&report["finishedAt"]);
    assert!(start <= finish);
    for file in report["files"].as_array().expect("files") {
        let file_start = timestamp(&file["startedAt"]);
        let file_finish = timestamp(&file["finishedAt"]);
        assert!(start <= file_start && file_start <= file_finish && file_finish <= finish);
        let source = fs::read(dir.path.join(file["path"].as_str().expect("path"))).expect("source");
        assert_eq!(
            file["sourceSha256"],
            format!("{:x}", Sha256::digest(source))
        );
    }
}

#[test]
fn combined_report_keeps_embedded_media_and_missing_scenarios_offline() {
    let dir = TestDir::new();
    let server = SiteServer::start();
    dir.file("first.whirl", "VISIT /stable.html\nSCREENSHOT first\n");
    dir.file("second.whirl", "VISIT /stable.html\nSCREENSHOT second\n");
    for name in ["first", "second"] {
        let output = run_whirl(&dir, &[
            "--base",
            &server.base(),
            "--video",
            "--report-json",
            &format!("{name}.json"),
            &format!("{name}.whirl"),
        ]);
        assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
    }
    dir.file(
        "expected.json",
        r#"["first.whirl","second.whirl","missing.whirl"]"#,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_whirl"))
        .current_dir(&dir.path)
        .args([
            "report",
            "first.json",
            "second.json",
            "--expected",
            "expected.json",
            "--html",
            "combined.html",
        ])
        .output()
        .expect("saved report command");
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
    fs::remove_dir_all(dir.artifacts()).expect("remove source media");
    let url = reqwest::Url::from_file_path(dir.path.join("combined.html")).expect("file URL");
    let source = format!(
        r#"VISIT "{url}"
[Asserts]
css:article count == 3
css:article[data-status=not-run] count == 1
css:article[data-status=passed] count == 2
css:video count == 2
css:img count == 2
EVAL "for (const video of document.querySelectorAll('video')) {{ await video.play(); if (!video.videoWidth) throw new Error('No video pixels'); video.pause(); }}"
EVAL "for (const details of document.querySelectorAll('details')) details.open = true; for (const img of document.images) {{ img.loading = 'eager'; await img.decode(); }}"
EVAL "for (const link of document.querySelectorAll('a')) if (!document.getElementById(link.hash.slice(1))) throw new Error('Broken report link'); if (document.documentElement.scrollWidth > innerWidth) throw new Error('Report overflows viewport')"
"#
    );
    dir.file("verify.whirl", &source);
    dir.file(
        "mobile.whirl",
        &format!("[Options]\nviewport: 390x844\n{source}"),
    );
    for flow in ["verify.whirl", "mobile.whirl"] {
        let output = run_whirl(&dir, &[flow]);
        assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
    }
}
