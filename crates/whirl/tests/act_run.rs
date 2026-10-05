//! `ACT` acceptance tests (CLI acceptance-tests decision, SPEC 7.4): each
//! test starts the `whirl` binary against the real shim and Chromium, and
//! points `WHIRL_LLM_ENDPOINT` at an in-process OpenAI-compatible model
//! twin whose answers the test scripts.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};
use std::{env, fs, process};

use reqwest::blocking::{Client as HttpClient, Response};
use serde_json::{Value as Json, json};
use tokio::net::TcpListener;
use tokio::runtime::{Builder, Runtime};
use twin_openai::config::{Config, Mode, RecordFormat};

/// A unique temporary directory for one test, removed on drop.
struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!("whirl-act-test-{}-{id}", process::id()));
        fs::create_dir_all(&path).expect("temp dir should be creatable");
        Self { path }
    }

    fn file(&self, name: &str, content: &str) -> PathBuf {
        let path = self.path.join(name);
        fs::write(&path, content).expect("temp file should be writable");
        path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// The OpenAI-compatible model twin, served on its own runtime.
struct ModelTwin {
    _runtime: Runtime,
    url:      String,
    http:     HttpClient,
}

/// The twin scopes scenarios and request logs by bearer token.
const API_KEY: &str = "act-acceptance";

impl ModelTwin {
    fn start() -> Self {
        let runtime = Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("the twin runtime should build");
        let listener = runtime
            .block_on(TcpListener::bind("127.0.0.1:0"))
            .expect("the twin should bind a local port");
        let url = format!(
            "http://{}",
            listener
                .local_addr()
                .expect("a bound listener has an address")
        );
        let app = twin_openai::build_app_with_config(Config {
            bind_addr:               "127.0.0.1:0".parse().expect("valid address"),
            require_auth:            true,
            enable_admin:            true,
            request_log_path:        None,
            scenarios_path:          None,
            allow_unmatched:         false,
            mode:                    Mode::Twin,
            upstream_url:            "https://api.openai.com".to_owned(),
            upstream_responses_path: None,
            upstream_api_key:        None,
            recording_path:          None,
            record_format:           RecordFormat::Semantic,
            recording_append:        false,
        })
        .expect("the twin app should build");
        runtime.spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("the twin should serve");
        });
        Self {
            _runtime: runtime,
            url,
            http: HttpClient::new(),
        }
    }

    /// Queues one scripted Chat Completions answer per scenario, in order.
    fn script(&self, scripts: &[Json]) {
        let scenarios: Vec<Json> = scripts
            .iter()
            .map(|script| json!({"matcher": {"endpoint": "chat.completions"}, "script": script}))
            .collect();
        let response = self
            .http
            .post(format!("{}/__admin/scenarios", self.url))
            .bearer_auth(API_KEY)
            .header("content-type", "application/json")
            .body(json!({"scenarios": scenarios}).to_string())
            .send()
            .expect("the twin should accept scenarios");
        assert!(response.status().is_success(), "{:?}", response.text());
    }

    /// Queues structured answers, one per model call.
    fn answer(&self, answers: &[Json]) {
        let scripts: Vec<Json> = answers
            .iter()
            .map(|answer| json!({"kind": "success", "structured_output": answer}))
            .collect();
        self.script(&scripts);
    }

    /// Queues Jev answers (`POST /v1/systemone`), one per request, in
    /// order. Each item is a full transcript script.
    fn jev(&self, scripts: &[Json]) {
        let scenarios: Vec<Json> = scripts
            .iter()
            .map(|script| json!({"matcher": {"endpoint": "systemone"}, "script": script}))
            .collect();
        let response = self
            .http
            .post(format!("{}/__admin/scenarios", self.url))
            .bearer_auth(API_KEY)
            .header("content-type", "application/json")
            .body(json!({"scenarios": scenarios}).to_string())
            .send()
            .expect("the twin should accept scenarios");
        assert!(response.status().is_success(), "{:?}", response.text());
    }

    /// Runs `whirl --jev` with Jev's endpoint and key pointing here.
    fn run_jev(&self, dir: &TestDir, flow: &Path, env: &[(&str, &str)]) -> Output {
        let mut env = env.to_vec();
        env.extend([
            ("WHIRL_JEV_ENDPOINT", self.url.as_str()),
            ("TYPESAFE_API_KEY", API_KEY),
        ]);
        self.run_with_args(dir, flow, &env, &["--jev"])
    }

    /// The twin's request log as text.
    fn request_log(&self) -> String {
        self.http
            .get(format!("{}/__admin/requests", self.url))
            .bearer_auth(API_KEY)
            .send()
            .and_then(Response::text)
            .expect("the twin should return its request log")
    }

    /// Runs `whirl` on `flow` with the model endpoint pointing here.
    fn run(&self, dir: &TestDir, flow: &Path, env: &[(&str, &str)]) -> Output {
        self.run_with_args(dir, flow, env, &[])
    }

    fn run_with_args(
        &self,
        dir: &TestDir,
        flow: &Path,
        env: &[(&str, &str)],
        args: &[&str],
    ) -> Output {
        let shim_js = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../shim/dist/index.js");
        Command::new(env!("CARGO_BIN_EXE_whirl"))
            .env("WHIRL_NODE", "node")
            .env("WHIRL_SHIM_JS", shim_js)
            .env("WHIRL_LLM_ENDPOINT", &self.url)
            .env("WHIRL_LLM_API_KEY", API_KEY)
            .envs(env.iter().copied())
            .current_dir(&dir.path)
            .arg("--artifacts")
            .arg(dir.path.join("artifacts"))
            .arg("--report-json")
            .arg(dir.path.join("report.json"))
            .arg("--report-html")
            .arg(dir.path.join("report.html"))
            .args(args)
            .arg(flow)
            .output()
            .expect("the whirl binary should run")
    }
}

fn exit_code(output: &Output) -> i32 {
    output.status.code().expect("whirl should exit, not signal")
}

fn stdout_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The report entry of the flow's single ACT step.
fn act_step(dir: &TestDir) -> Json {
    let text = fs::read_to_string(dir.path.join("report.json")).expect("the JSON report exists");
    let report: Json = serde_json::from_str(&text).expect("the JSON report parses");
    report["files"][0]["entries"][0]["steps"]
        .as_array()
        .expect("the entry has steps")
        .iter()
        .find(|step| step["act"].is_object())
        .cloned()
        .expect("one step is an ACT step")
}

fn click(element_id: &str, two_step: bool) -> Json {
    json!({
        "action": {"elementId": element_id, "description": "the button", "method": "click", "arguments": []},
        "twoStep": two_step
    })
}

const SHOP: &str = "VISIT \"data:text/html,<h1>Shop</h1>\
    <button onclick=\\\"document.querySelector('h1').textContent='Added'\\\">Add to cart</button>\"\n";

#[test]
fn act_clicks_the_element_the_model_chooses() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[click("e3", false)]);
    let flow = dir.file(
        "click.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{SHOP}ACT \"add the item to the cart\"\n\
             ASSERT role:heading \"Added\" visible\n"
        ),
    );
    let output = twin.run(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");

    let step = act_step(&dir);
    assert_eq!(step["act"]["model"], "gpt-test");
    assert_eq!(
        step["act"]["actions"][0]["line"],
        "CLICK role:button \"Add to cart\""
    );
    assert_eq!(step["act"]["usage"]["modelCalls"], 1);
    let html = fs::read_to_string(dir.path.join("report.html")).expect("the HTML report exists");
    assert!(
        html.contains("<code>CLICK role:button &quot;Add to cart&quot;</code> the button"),
        "the HTML report shows what ACT ran"
    );

    let log = twin.request_log();
    assert!(log.contains("add the item to the cart"), "log:\n{log}");
    assert!(
        log.contains("button \\\"Add to cart\\\" [ref=e3]"),
        "the model sees the snapshot; log:\n{log}"
    );
}

#[test]
fn a_masked_value_fills_the_page_but_never_reaches_the_model() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[json!({
        "action": {"elementId": "e3", "description": "password field", "method": "fill",
                   "arguments": ["%env.WHIRL_ACT_SECRET%"]},
        "twoStep": false
    })]);
    let flow = dir.file(
        "secret.whirl",
        "[Options]\nmodel: gpt-test\n\
         VISIT \"data:text/html,<h1>Login</h1><input type=password aria-label=Password>\"\n\
         ACT \"type {{env.WHIRL_ACT_SECRET}} into the password field\"\n\
         ASSERT label:Password value == {{env.WHIRL_ACT_SECRET}}\n",
    );
    let secret = "hunter2-act-secret";
    let output = twin.run(&dir, &flow, &[("WHIRL_ACT_SECRET", secret)]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    assert!(!stdout.contains(secret), "stdout:\n{stdout}");

    let log = twin.request_log();
    assert!(
        !log.contains(secret),
        "the secret reached the model:\n{log}"
    );
    assert!(log.contains("%env.WHIRL_ACT_SECRET%"), "log:\n{log}");

    let report = fs::read_to_string(dir.path.join("report.json")).expect("report exists");
    assert!(!report.contains(secret), "report:\n{report}");
    assert_eq!(
        act_step(&dir)["act"]["actions"][0]["line"],
        "FILL role:textbox \"Password\" \"%env.WHIRL_ACT_SECRET%\""
    );
}

#[test]
fn a_two_step_action_plans_again_on_a_fresh_snapshot() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[click("e3", true), click("e5", false)]);
    let flow = dir.file(
        "dropdown.whirl",
        "[Options]\nmodel: gpt-test\n\
         VISIT \"data:text/html,<h1>Size</h1>\
         <button onclick=\\\"document.getElementById('menu').hidden=false\\\">Choose size</button>\
         <div id=menu hidden><button onclick=\\\"document.querySelector('h1').textContent='Large chosen'\\\">\
         Large</button></div>\"\n\
         ACT \"choose Large from the size dropdown\"\n\
         ASSERT role:heading \"Large chosen\" visible\n",
    );
    let output = twin.run(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");

    let step = act_step(&dir);
    assert_eq!(step["act"]["usage"]["modelCalls"], 2);
    assert_eq!(
        step["act"]["actions"][1]["line"],
        "CLICK role:button \"Large\""
    );
    let log = twin.request_log();
    assert!(log.contains("step 1 of 2"), "log:\n{log}");
    assert!(
        log.contains("button \\\"Large\\\" [ref=e5]"),
        "step two sees the opened menu; log:\n{log}"
    );
}

#[test]
fn no_matching_element_fails_the_entry() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[json!({"action": null, "twoStep": false})]);
    let flow = dir.file(
        "nomatch.whirl",
        &format!("[Options]\nmodel: gpt-test\n{SHOP}ACT \"open the settings page\"\n"),
    );
    let output = twin.run(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 1, "stdout:\n{stdout}");
    assert!(stdout.contains("act-no-match"), "stdout:\n{stdout}");
    assert_eq!(act_step(&dir)["error"]["code"], "act-no-match");
}

#[test]
fn an_element_the_snapshot_never_showed_fails_the_entry() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[click("e99", false)]);
    let flow = dir.file(
        "unknown.whirl",
        &format!("[Options]\nmodel: gpt-test\n{SHOP}ACT \"add the item to the cart\"\n"),
    );
    let output = twin.run(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 1, "stdout:\n{stdout}");
    assert_eq!(act_step(&dir)["error"]["code"], "act-invalid-decision");
    assert!(stdout.contains("e99"), "stdout:\n{stdout}");
}

/// A file row whose own menu opens on a right click, as `ref=e3`.
const FILES: &str = "VISIT \"data:text/html,<h1>Files</h1>\
    <button oncontextmenu=\\\"event.preventDefault();document.querySelector('h1').textContent='Menu'\\\">report.pdf</button>\"\n";

fn click_with_button(element_id: &str, button: &str) -> Json {
    json!({
        "action": {"elementId": element_id, "description": "the file", "method": "click", "arguments": [button]},
        "twoStep": false
    })
}

#[test]
fn act_right_clicks_when_the_model_names_the_right_button() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[click_with_button("e3", "right")]);
    let flow = dir.file(
        "right-click.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{FILES}ACT \"right-click report.pdf\"\n\
             ASSERT role:heading \"Menu\" visible\n"
        ),
    );
    let output = twin.run(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    assert_eq!(
        act_step(&dir)["act"]["actions"][0]["line"],
        "RIGHTCLICK role:button \"report.pdf\""
    );
    let log = twin.request_log();
    assert!(
        log.contains(
            "When choosing non-left click actions, provide right or middle as the argument"
        ),
        "log:\n{log}"
    );
}

#[test]
fn an_unknown_mouse_button_fails_the_entry() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[click_with_button("e3", "sideways")]);
    let flow = dir.file(
        "sideways.whirl",
        &format!("[Options]\nmodel: gpt-test\n{FILES}ACT \"right-click report.pdf\"\n"),
    );
    let output = twin.run(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 1, "stdout:\n{stdout}");
    assert_eq!(act_step(&dir)["error"]["code"], "act-invalid-decision");
    assert!(stdout.contains("sideways"), "stdout:\n{stdout}");
}

/// A native drag source and drop zone, as `ref=e3` and `ref=e4`.
const BOARD: &str = "<h1>Board</h1>\
    <button id=\"card\" draggable=\"true\">Card</button>\
    <button id=\"done\">Done</button>\
    <script>\
    card.ondragstart = (e) => e.dataTransfer.setData('text/plain', 'Card');\
    done.ondragover = (e) => e.preventDefault();\
    done.ondrop = (e) => { e.preventDefault(); document.querySelector('h1').textContent = 'Dropped'; };\
    </script>";

fn drag(source: &str, target: &str) -> Json {
    json!({
        "action": {"elementId": source, "description": "the card", "method": "dragAndDrop", "arguments": [target]},
        "twoStep": false
    })
}

#[test]
fn act_drags_an_element_onto_the_target_the_model_names() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[drag("e3", "e4")]);
    let flow = dir.file(
        "drag.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{}ACT \"drag the card to Done\"\n\
             ASSERT role:heading \"Dropped\" visible\n",
            visit_html(BOARD)
        ),
    );
    let output = twin.run(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    assert_eq!(
        act_step(&dir)["act"]["actions"][0]["line"],
        "DRAG role:button \"Card\" to role:button \"Done\""
    );
    let log = twin.request_log();
    assert!(log.contains("choose the dragAndDrop method"), "log:\n{log}");
}

#[test]
fn a_drop_target_the_snapshot_never_showed_fails_the_entry() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[drag("e3", "e99")]);
    let flow = dir.file(
        "drag-unknown.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{}ACT \"drag the card to Done\"\n",
            visit_html(BOARD)
        ),
    );
    let output = twin.run(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 1, "stdout:\n{stdout}");
    assert_eq!(act_step(&dir)["error"]["code"], "act-invalid-decision");
    assert!(stdout.contains("e99"), "stdout:\n{stdout}");
}

/// A page taller than the viewport; `ref=e1` is its `<body>`.
const TALL: &str = "<h1>Feed</h1><div style=\"height: 3000px\">Posts</div>";

fn scroll_answer(method: &str, arguments: &[&str]) -> Json {
    json!({
        "action": {"elementId": "e1", "description": "the page", "method": method, "arguments": arguments},
        "twoStep": false
    })
}

#[test]
fn act_scrolls_the_page_when_the_model_names_its_body() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[scroll_answer("scrollTo", &["100%"])]);
    let flow = dir.file(
        "scroll.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{}ACT \"scroll to the bottom\"\n\
             ASSERT eval \"window.scrollY > 2000\" == true\n",
            visit_html(TALL)
        ),
    );
    let output = twin.run(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    assert_eq!(
        act_step(&dir)["act"]["actions"][0]["line"],
        "SCROLL to 100%"
    );
    let log = twin.request_log();
    assert!(
        log.contains("choose the root element of the tree"),
        "log:\n{log}"
    );
}

#[test]
fn a_scroll_position_that_is_not_a_percent_fails_the_entry() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[scroll_answer("scrollTo", &["halfway"])]);
    let flow = dir.file(
        "scroll-bad.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{}ACT \"scroll halfway down\"\n",
            visit_html(TALL)
        ),
    );
    let output = twin.run(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 1, "stdout:\n{stdout}");
    assert_eq!(act_step(&dir)["error"]["code"], "act-invalid-decision");
    assert!(stdout.contains("halfway"), "stdout:\n{stdout}");
}

#[test]
fn a_rejected_credential_is_a_runtime_error() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.script(&[json!({
        "kind": "error",
        "status": 401,
        "message": "Incorrect API key provided",
        "error_type": "invalid_request_error",
        "code": "invalid_api_key"
    })]);
    let flow = dir.file(
        "auth.whirl",
        &format!("[Options]\nmodel: gpt-test\n{SHOP}ACT \"add the item to the cart\"\n"),
    );
    let output = twin.run(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 3, "stdout:\n{stdout}");
    let step = act_step(&dir);
    assert_eq!(step["error"]["code"], "act-model");
    assert_eq!(step["status"], "error");
}

#[test]
fn act_works_in_every_engine_when_requested() {
    // Firefox and WebKit are installed only for check:nightly
    // (WHIRL_TEST_ALL_BROWSERS=1).
    if env::var_os("WHIRL_TEST_ALL_BROWSERS").is_none() {
        return;
    }
    // A frame from another origin, which only its title names.
    let page = "<h1>Shop</h1>\
        <button onclick=\"document.querySelector('h1').textContent='Added'\">Add to cart</button>\
        <iframe title=Reviews src=\"data:text/html,<p>Five stars</p>\"></iframe>";
    for engine in ["firefox", "webkit"] {
        let dir = TestDir::new();
        let twin = ModelTwin::start();
        twin.answer(&[click("e3", false)]);
        let flow = dir.file(
            "click.whirl",
            &format!(
                "[Options]\nmodel: gpt-test\n{}ACT \"add the item to the cart\"\n\
                 ASSERT role:heading \"Added\" visible\n",
                visit_html(page)
            ),
        );
        let output = twin.run_with_args(&dir, &flow, &[], &["--browser", engine]);
        let stdout = stdout_text(&output);
        assert_eq!(exit_code(&output), 0, "{engine} stdout:\n{stdout}");
        let log = twin.request_log();
        assert!(
            log.contains("button \\\"Add to cart\\\" [ref=e3]"),
            "{engine} snapshot refs; log:\n{log}"
        );
        assert!(
            log.contains("iframe \\\"Reviews\\\" [ref=e4]"),
            "{engine} names the frame; log:\n{log}"
        );
    }
}

/// A page whose "Add to cart" button is replaced, as a framework
/// re-render replaces it, after each delay in `renders_ms`.
fn rerendering_shop(renders_ms: &[u32]) -> String {
    let mut timers = String::new();
    for ms in renders_ms {
        write!(timers, "setTimeout(render,{ms});").expect("writing to a String cannot fail");
    }
    let html = format!(
        "<h1>Shop</h1><div id=slot><button>Add to cart</button></div><script>\
         const render=()=>{{document.getElementById('slot').innerHTML='<button>Add to cart</button>';\
         document.querySelector('#slot button').onclick=()=>{{document.querySelector('h1').textContent='Added';}};}};\
         {timers}</script>"
    );
    visit_html(&html)
}

/// A `VISIT` line for an HTML page in a percent-encoded data URL.
fn visit_html(html: &str) -> String {
    let mut encoded = String::new();
    for byte in html.bytes() {
        if byte.is_ascii_alphanumeric() {
            encoded.push(char::from(byte));
        } else {
            write!(encoded, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    format!("VISIT \"data:text/html,{encoded}\"\n")
}

/// A delayed answer, so the page can replace the element meanwhile.
fn delayed_click(element_id: &str, delay_ms: u32) -> Json {
    json!({"kind": "success", "structured_output": click(element_id, false), "delay_before_headers_ms": delay_ms})
}

#[test]
fn a_replaced_element_is_planned_again_on_a_fresh_snapshot() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    // The button is replaced at 300 ms, while the first answer is on its
    // way; the retry sees the new button as e5.
    twin.script(&[
        delayed_click("e4", 1_000),
        json!({"kind": "success", "structured_output": click("e5", false)}),
    ]);
    let flow = dir.file(
        "rerender.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{}ACT \"add the item to the cart\" @30s\n\
             ASSERT role:heading \"Added\" visible\n",
            rerendering_shop(&[300])
        ),
    );
    let output = twin.run(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    let step = act_step(&dir);
    assert_eq!(step["act"]["usage"]["modelCalls"], 2);
    assert_eq!(
        step["act"]["actions"][0]["line"],
        "CLICK role:button \"Add to cart\""
    );
}

#[test]
fn an_element_replaced_twice_fails_fast_with_stale_ref() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    // Replaced at 300 ms and again at 1500 ms: both answers arrive after
    // the element they name is gone.
    twin.script(&[delayed_click("e4", 1_000), delayed_click("e5", 1_000)]);
    let flow = dir.file(
        "rerender.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{}ACT \"add the item to the cart\" @30s\n",
            rerendering_shop(&[300, 1_500])
        ),
    );
    let output = twin.run(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 1, "stdout:\n{stdout}");
    let step = act_step(&dir);
    assert_eq!(step["error"]["code"], "stale-ref", "stdout:\n{stdout}");
    assert_eq!(step["act"]["usage"]["modelCalls"], 2);
    let duration = step["durationMs"].as_u64().expect("a duration");
    assert!(
        duration < 10_000,
        "a stale ref must not wait out the 30 s budget; took {duration} ms"
    );
}

#[test]
fn a_click_on_a_folded_wrapper_reaches_the_element_inside_it() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    // The snapshot folds the narrow trigger into its full-width wrapper,
    // e3. A click at the wrapper's center would miss the trigger.
    twin.answer(&[click("e3", true), click("e5", false)]);
    let page = "<h1>Shipping</h1><div><div id=t style=\"width:10rem\" \
        onclick=\"document.getElementById('o').hidden=false\">Select a country</div>\
        <div id=o hidden><div onclick=\"document.querySelector('h1').textContent='Canada chosen'\">\
        Canada</div></div></div>";
    let flow = dir.file(
        "folded.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{}ACT \"choose Canada from the country dropdown\" @30s\n\
             ASSERT role:heading \"Canada chosen\" visible\n",
            visit_html(page)
        ),
    );
    let output = twin.run(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    assert_eq!(
        act_step(&dir)["act"]["actions"].as_array().map(Vec::len),
        Some(2)
    );
}

#[test]
fn a_scoped_act_shows_the_model_only_that_element() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    // A scoped snapshot on a fresh page numbers the form's button e2.
    twin.answer(&[click("e2", false)]);
    let page = "<header><a href=\"https://example.com/menu\">Menu</a>\
        <button onclick=\"document.querySelector('h1').textContent='Wrong'\">Buy</button></header>\
        <h1>Shop</h1><form><button type=button \
        onclick=\"document.querySelector('h1').textContent='Bought'\">Buy</button></form>";
    let flow = dir.file(
        "scoped.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{}ACT css:form \"click Buy\" @30s\n\
             ASSERT role:heading \"Bought\" visible\n",
            visit_html(page)
        ),
    );
    let output = twin.run(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    let log = twin.request_log();
    assert!(log.contains("button \\\"Buy\\\" [ref=e2]"), "log:\n{log}");
    assert!(
        !log.contains("Menu"),
        "the header is outside the scope; log:\n{log}"
    );
}

/// Frames named by a title, by an `aria-label` over a title, from inside
/// another frame, from another origin, and not at all.
const FRAMED_HELP: &str = "<h1>Help</h1><main>\
    <iframe title=\"Incident history\" \
    srcdoc=\"<p>Resolved</p><iframe title='Uptime chart' srcdoc='<p>Up</p>'></iframe>\"></iframe>\
    <iframe aria-label=\"Live chat\" title=\"Chat widget\" srcdoc=\"<p>Hello</p>\"></iframe>\
    <iframe title=Weather src=\"data:text/html,<p>Sunny</p>\"></iframe>\
    <iframe srcdoc=\"<p>Advert</p>\"></iframe></main>";

#[test]
fn a_scoped_act_shows_the_model_each_iframe_by_its_name() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[json!({
        "action": {"elementId": "e3", "description": "the chat", "method": "scrollIntoView", "arguments": []},
        "twoStep": false
    })]);
    let flow = dir.file(
        "scoped-frames.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{}ACT css:main \"show the live chat\" @30s\n",
            visit_html(FRAMED_HELP)
        ),
    );
    let output = twin.run(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    assert_eq!(
        act_step(&dir)["act"]["actions"][0]["line"],
        "SCROLL role:iframe \"Live chat\""
    );
    let log = twin.request_log();
    for line in [
        r#"iframe \"Incident history\" [ref=e2]"#,
        r#"iframe \"Uptime chart\" [ref=f1e3]"#,
        r#"iframe \"Live chat\" [ref=e3]"#,
        r#"iframe \"Weather\" [ref=e4]"#,
        "iframe [ref=e5]",
    ] {
        assert!(log.contains(line), "the model sees {line}; log:\n{log}");
    }
    assert!(!log.contains("Chat widget"), "log:\n{log}");
}

#[test]
fn act_reaches_a_button_in_a_closed_shadow_root() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[click("e4", false)]);
    let page = "<h1>Closed</h1><div id=host></div><script>\
        const root=document.getElementById('host').attachShadow({mode:'closed'});\
        root.innerHTML='<button>Deep button</button>';\
        root.querySelector('button').onclick=()=>{document.querySelector('h1').textContent='clicked';};\
        </script>";
    let flow = dir.file(
        "closed.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{}ACT \"click the Deep button\" @30s\n\
             ASSERT role:heading \"clicked\" visible\n",
            visit_html(page)
        ),
    );
    let output = twin.run(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    let log = twin.request_log();
    assert!(
        log.contains("button \\\"Deep button\\\" [ref=e4]"),
        "log:\n{log}"
    );
}

#[test]
fn act_selects_a_radio_that_a_styled_overlay_covers() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    // A plain click on e4 fails: the span intercepts pointer events.
    twin.answer(&[click("e4", false)]);
    let page = "<h1>Size</h1><label style=\"position:relative;display:inline-block\">\
        <input type=radio name=size value=Medium><span aria-hidden=true \
        style=\"position:absolute;inset:0;background:white;border:1px solid\"></span> Medium</label>";
    let flow = dir.file(
        "radio.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{}ACT \"choose the Medium size\" @10s\n\
             ASSERT role:radio \"Medium\" checked\n",
            visit_html(page)
        ),
    );
    let output = twin.run(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
}

fn fill(element_id: &str, text: &str) -> Json {
    json!({
        "action": {"elementId": element_id, "description": "the field", "method": "fill", "arguments": [text]},
        "twoStep": false
    })
}

#[test]
fn a_fill_that_the_field_does_not_keep_fails_the_entry() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[fill("e3", "ABC-12345")]);
    let page = "<h1>Voucher</h1><input aria-label=Code maxlength=4>";
    let flow = dir.file(
        "truncated.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{}ACT \"enter the code ABC-12345\"\n",
            visit_html(page)
        ),
    );
    let output = twin.run(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 1, "stdout:\n{stdout}");
    let step = act_step(&dir);
    assert_eq!(step["error"]["code"], "act-fill-mismatch");
    assert_eq!(
        step["error"]["message"],
        "act-fill-mismatch: after FILL role:textbox \"Code\" \"ABC-12345\", the field holds \"ABC-\""
    );
}

#[test]
fn a_fill_that_the_field_formats_passes() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[fill("e3", "5551234567")]);
    let page = "<h1>Contact</h1><input aria-label=Phone oninput=\"const d=this.value.replace(/\\D/g,'');\
        this.value='('+d.slice(0,3)+') '+d.slice(3,6)+'-'+d.slice(6)\">";
    let flow = dir.file(
        "formatted.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{}ACT \"enter the phone number 5551234567\"\n\
             ASSERT label:Phone value == \"(555) 123-4567\"\n",
            visit_html(page)
        ),
    );
    let output = twin.run(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
}

#[test]
fn typed_text_keeps_the_characters_the_instruction_quotes() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[fill("e3", "abc 123")]);
    let page = "<h1>Search</h1><input aria-label=Search>";
    let flow = dir.file(
        "grounded.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{}ACT \"type \\\"AbC 123\\\" into the search field\"\n\
             ASSERT label:Search value == \"AbC 123\"\n",
            visit_html(page)
        ),
    );
    let output = twin.run(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    assert_eq!(
        act_step(&dir)["act"]["actions"][0]["line"],
        "FILL role:textbox \"Search\" \"AbC 123\""
    );
}

/// A Jev answer body, as `/v1/systemone` returns it.
fn jev_answer(answers: &Json) -> Json {
    json!({
        "kind": "transcript",
        "status": 200,
        "content_type": "application/json",
        "body": {"model": "jev-1.13.0", "answers": answers, "usage": {"input_tokens": 500, "output_tokens": 70}}
    })
}

/// A choice answer that gives `chosen` probability `p` and splits the rest
/// evenly, as the real API gives every option a probability.
fn jev_choice(options: &[&str], chosen: &str, p: f64) -> Json {
    let rest = (1.0 - p) / (options.len() - 1) as f64;
    let probabilities: serde_json::Map<String, Json> = options
        .iter()
        .map(|&option| {
            (
                option.to_owned(),
                json!(if option == chosen { p } else { rest }),
            )
        })
        .collect();
    json!({"type": "choice", "choice": chosen, "confidence": p, "probabilities": probabilities})
}

/// Jev's answers to the intent request: the family, and no key, special
/// mouse button, end state, suggestion, or scroll.
fn jev_intent(family: &str, confidence: f64) -> Json {
    jev_intent_answers(family, confidence, "left", ("not_scroll", "not_scroll"))
}

/// The same, with the mouse button Jev names.
fn jev_intent_with_button(family: &str, confidence: f64, button: &str) -> Json {
    jev_intent_answers(family, confidence, button, ("not_scroll", "not_scroll"))
}

/// A sure scroll, with its way and area.
fn jev_scroll_intent(way: &str, area: &str) -> Json {
    jev_intent_answers("scroll", 0.95, "left", (way, area))
}

fn jev_intent_answers(
    family: &str,
    confidence: f64,
    button: &str,
    (way, area): (&str, &str),
) -> Json {
    const FAMILIES: [&str; 10] = [
        "click",
        "double_click",
        "hover",
        "fill",
        "select",
        "press",
        "scroll",
        "drag",
        "not_an_action",
        "unsupported",
    ];
    const KEYS: [&str; 15] = [
        "Enter",
        "Tab",
        "Escape",
        "Space",
        "Backspace",
        "Delete",
        "ArrowUp",
        "ArrowDown",
        "ArrowLeft",
        "ArrowRight",
        "PageUp",
        "PageDown",
        "Home",
        "End",
        "other",
    ];
    jev_answer(&json!({
        "family": jev_choice(&FAMILIES, family, confidence),
        "mouse_button": jev_choice(&["left", "right", "middle"], button, 0.98),
        "toggle_state": jev_choice(&["on", "off", "unspecified"], "unspecified", 0.96),
        "after_typing": jev_choice(&["nothing", "pick_suggestion"], "nothing", 0.97),
        "key": jev_choice(&KEYS, "other", 0.93),
        "scroll_way": jev_choice(
            &["down", "up", "left", "right", "position", "into_view", "not_scroll"],
            way,
            0.95
        ),
        "scroll_area": jev_choice(&["page", "part", "not_scroll"], area, 0.95),
    }))
}

/// Jev's pick among several candidates: `strict` and `best` agree on
/// `element_id` with probability `p`.
fn jev_pick(element_id: &str, others: &[&str], p: f64) -> Json {
    let mut strict: Vec<&str> = others.to_vec();
    strict.push(element_id);
    strict.push("none_match");
    let mut best: Vec<&str> = others.to_vec();
    best.push(element_id);
    jev_answer(&json!({
        "strict": jev_choice(&strict, element_id, p),
        "best": jev_choice(&best, element_id, p),
    }))
}

/// Jev's pick among one candidate: `best` is a yes-or-no probability.
fn jev_only(element_id: &str, plausible: f64) -> Json {
    jev_answer(&json!({
        "strict": {"type": "choice", "choice": element_id, "confidence": 0.97,
                   "probabilities": {element_id: 0.97, "none_match": 0.03}},
        "best": {"type": "noul", "noul": plausible}
    }))
}

const TWO_BUTTONS: &str = "<h1>Shop</h1>\
    <button onclick=\"document.querySelector('h1').textContent='Saved'\">Save</button>\
    <button onclick=\"document.querySelector('h1').textContent='Shared'\">Share</button>";

/// A page whose second button has a name that Playwright's snapshot wraps
/// in YAML single quotes: `- 'button "Status: live" [ref=e4]'`.
const LIVE_STATUS: &str = "<h1>Status</h1>\
    <button onclick=\"document.querySelector('h1').textContent='Saved'\">Save</button>\
    <button onclick=\"document.querySelector('h1').textContent='Live'\">Status: live</button>";

#[test]
fn act_clicks_a_button_whose_name_playwright_quotes() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[click("e4", false)]);
    let flow = dir.file(
        "quoted-name.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{}ACT \"set the status to live\"\n\
             ASSERT role:heading \"Live\" visible\n",
            visit_html(LIVE_STATUS)
        ),
    );
    let output = twin.run(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    assert_eq!(
        act_step(&dir)["act"]["actions"][0]["line"],
        "CLICK role:button \"Status: live\""
    );
    let log = twin.request_log();
    assert!(
        log.contains(r#"- button \"Status: live\" [ref=e4]"#),
        "the model reads the line without its quotes; log:\n{log}"
    );
    assert!(!log.contains("'button"), "log:\n{log}");
}

#[test]
fn jev_picks_a_button_whose_name_playwright_quotes() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.jev(&[jev_intent("click", 0.95), jev_pick("e4", &["e3"], 0.95)]);
    let flow = dir.file(
        "jev-quoted-name.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{}ACT \"set the status to live\"\n\
             ASSERT role:heading \"Live\" visible\n",
            visit_html(LIVE_STATUS)
        ),
    );
    let output = twin.run_jev(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    let step = act_step(&dir);
    assert_eq!(
        step["act"]["actions"][0]["line"],
        "CLICK role:button \"Status: live\""
    );
    assert_eq!(step["act"]["actions"][0]["plannedBy"], "jev");
    assert_eq!(step["act"]["usage"]["modelCalls"], 0);
    let log = twin.request_log();
    assert!(
        log.contains(r#"\"e4\":{\"role\":\"button\",\"name\":\"Status: live\""#),
        "Jev sees the button among the buttons; log:\n{log}"
    );
}

/// A page whose second button has a name that holds the first button's
/// ref: `- button "Delete [ref=e3]" [ref=e4]`.
const REF_IN_NAME: &str = "<h1>Files</h1>\
    <button onclick=\"document.querySelector('h1').textContent='Kept'\">Keep</button>\
    <button onclick=\"document.querySelector('h1').textContent='Deleted'\">Delete [ref=e3]</button>";

#[test]
fn a_ref_in_a_name_cannot_redirect_jevs_pick() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.jev(&[jev_intent("click", 0.95), jev_pick("e4", &["e3"], 0.95)]);
    let flow = dir.file(
        "jev-ref-in-name.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{}ACT \"delete the file\"\n\
             ASSERT role:heading \"Deleted\" visible\n",
            visit_html(REF_IN_NAME)
        ),
    );
    let output = twin.run_jev(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    let step = act_step(&dir);
    assert_eq!(
        step["act"]["actions"][0]["line"],
        "CLICK role:button \"Delete [ref=e3]\""
    );
    assert_eq!(step["act"]["actions"][0]["plannedBy"], "jev");
    assert_eq!(step["act"]["usage"]["modelCalls"], 0);
    let log = twin.request_log();
    assert!(
        log.contains(r#"\"e3\":{\"role\":\"button\",\"name\":\"Keep\""#),
        "log:\n{log}"
    );
    assert!(
        log.contains(r#"\"e4\":{\"role\":\"button\",\"name\":\"Delete [ref=e3]\""#),
        "Jev sees the button with its own ref; log:\n{log}"
    );
}

/// A page whose second button has a name that Playwright writes without
/// quotes, because it starts and ends with `/`: `- button /api/ [ref=e4]`.
const SLASH_NAME: &str = "<h1>Endpoints</h1>\
    <button onclick=\"document.querySelector('h1').textContent='Docs'\">Docs</button>\
    <button onclick=\"document.querySelector('h1').textContent='Called'\">/api/</button>";

#[test]
fn jev_picks_a_button_whose_name_starts_and_ends_with_a_slash() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.jev(&[jev_intent("click", 0.95), jev_pick("e4", &["e3"], 0.95)]);
    let flow = dir.file(
        "jev-slash-name.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{}ACT \"call the api\"\n\
             ASSERT role:heading \"Called\" visible\n",
            visit_html(SLASH_NAME)
        ),
    );
    let output = twin.run_jev(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    let step = act_step(&dir);
    assert_eq!(
        step["act"]["actions"][0]["line"],
        "CLICK role:button \"/api/\""
    );
    assert_eq!(step["act"]["actions"][0]["plannedBy"], "jev");
    assert_eq!(step["act"]["usage"]["modelCalls"], 0);
    let log = twin.request_log();
    assert!(
        log.contains(r#"\"e4\":{\"role\":\"button\",\"name\":\"/api/\""#),
        "Jev sees the button among the buttons; log:\n{log}"
    );
}

/// Two days, each a region that its own heading names through
/// `aria-labelledby`, so the snapshot shows them as `- region [ref=e3]:`
/// and `- region [ref=e6]:`. The tram ride in Saturday drags natively.
const DAYS: &str = "<h1>Trip</h1>\
    <section aria-labelledby=sat><h2 id=sat>Saturday</h2>\
    <button id=tram draggable=true>Tram ride</button></section>\
    <section aria-labelledby=sun><h2 id=sun>Sunday</h2><p>Nothing yet</p></section>\
    <script>\
    tram.ondragstart = (e) => e.dataTransfer.setData('text/plain', 'tram');\
    for (const day of document.querySelectorAll('section')) {\
      day.ondragover = (e) => e.preventDefault();\
      day.ondrop = (e) => {\
        e.preventDefault();\
        day.append(tram);\
        document.querySelector('h1').textContent = 'Moved to ' + day.querySelector('h2').textContent;\
      };\
    }\
    </script>";

#[test]
fn jev_reads_a_region_that_its_own_heading_names_by_that_heading() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.jev(&[
        jev_intent("drag", 0.95),
        jev_pick("e5", &["e2", "e4", "e7", "e8"], 0.95),
        jev_pick("e6", &["e3"], 0.95),
    ]);
    let flow = dir.file(
        "jev-heading-region.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{}ACT \"move the tram ride to Sunday\"\n\
             ASSERT role:heading \"Moved to Sunday\" visible\n",
            visit_html(DAYS)
        ),
    );
    let output = twin.run_jev(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    let step = act_step(&dir);
    assert_eq!(
        step["act"]["actions"][0]["line"],
        "DRAG role:button \"Tram ride\" to role:region"
    );
    assert_eq!(step["act"]["actions"][0]["plannedBy"], "jev");
    assert_eq!(step["act"]["usage"]["modelCalls"], 0);
    let log = twin.request_log();
    assert!(
        log.contains(
            r#"\"e6\":{\"role\":\"region\",\"label\":\"Sunday\",\"heading\":\"Sunday\",\"position\":\"2 of 2\"}"#
        ),
        "Jev reads the Sunday region by its own heading, not Saturday's; log:\n{log}"
    );
    assert!(
        log.contains(
            r#"\"e3\":{\"role\":\"region\",\"label\":\"Saturday\",\"heading\":\"Saturday\",\"position\":\"1 of 2\"}"#
        ),
        "log:\n{log}"
    );
}

#[test]
fn jev_acts_without_a_model_call_when_it_is_sure() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.jev(&[jev_intent("click", 0.95), jev_only("e3", 0.95)]);
    let flow = dir.file(
        "jev-click.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{SHOP}ACT \"add the item to the cart\"\n\
             ASSERT role:heading \"Added\" visible\n"
        ),
    );
    let output = twin.run_jev(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");

    let step = act_step(&dir);
    assert_eq!(step["act"]["planner"], "jev");
    assert_eq!(step["act"]["actions"][0]["plannedBy"], "jev");
    assert_eq!(
        step["act"]["actions"][0]["line"],
        "CLICK role:button \"Add to cart\""
    );
    assert_eq!(step["act"]["usage"]["modelCalls"], 0);
    // 1000 Jev input tokens at the catalog's $0.042 per million; no model
    // call.
    assert_eq!(step["act"]["usage"]["costUsdMicros"], 42);
    assert_eq!(step["act"]["usage"]["jev"]["costUsdMicros"], 42);
    assert_eq!(step["act"]["usage"]["jev"]["requests"], 2);
    assert_eq!(step["act"]["usage"]["jev"]["inputTokens"], 1000);
    let log = twin.request_log();
    assert!(!log.contains("chat.completions"), "log:\n{log}");
    assert!(log.contains("add the item to the cart"), "log:\n{log}");
}

#[test]
fn jev_right_clicks_without_a_model_call() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.jev(&[
        jev_intent_with_button("click", 0.95, "right"),
        jev_only("e3", 0.95),
    ]);
    let flow = dir.file(
        "jev-right-click.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{FILES}ACT \"right-click report.pdf\"\n\
             ASSERT role:heading \"Menu\" visible\n"
        ),
    );
    let output = twin.run_jev(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    let step = act_step(&dir);
    assert_eq!(step["act"]["actions"][0]["plannedBy"], "jev");
    assert_eq!(
        step["act"]["actions"][0]["line"],
        "RIGHTCLICK role:button \"report.pdf\""
    );
    assert_eq!(step["act"]["usage"]["modelCalls"], 0);
}

#[test]
fn jev_scrolls_the_page_to_the_position_the_instruction_names() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.jev(&[jev_scroll_intent("position", "page")]);
    let flow = dir.file(
        "jev-scroll-bottom.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{}ACT \"scroll to the bottom\"\n\
             ASSERT eval \"window.scrollY > 2000\" == true\n",
            visit_html(TALL)
        ),
    );
    let output = twin.run_jev(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    let step = act_step(&dir);
    assert_eq!(step["act"]["actions"][0]["line"], "SCROLL to 100%");
    assert_eq!(step["act"]["actions"][0]["plannedBy"], "jev");
    assert_eq!(step["act"]["usage"]["modelCalls"], 0);
    assert_eq!(step["act"]["usage"]["jev"]["requests"], 1);
}

/// A status page with two frames that only their titles name.
const FRAMED_STATUS: &str = "<h1>Status page</h1>\
    <iframe id=incidents title=\"Incident history\" width=400 height=120 \
    srcdoc=\"<p>First</p><div style='height:1200px'>Posts</div><p>Last</p>\"></iframe>\
    <iframe title=Advertisement width=400 height=120 srcdoc=\"<p>Buy now</p>\"></iframe>";

#[test]
fn jev_scrolls_inside_the_iframe_that_its_title_names() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.jev(&[
        jev_scroll_intent("position", "part"),
        jev_pick("e3", &["e4"], 0.95),
    ]);
    let flow = dir.file(
        "jev-scroll-frame.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{}ACT \"scroll down 50% inside the incident history\"\n\
             ASSERT eval \"document.querySelector('#incidents').contentWindow.scrollY > 0\" == true\n",
            visit_html(FRAMED_STATUS)
        ),
    );
    let output = twin.run_jev(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    let step = act_step(&dir);
    assert_eq!(
        step["act"]["actions"][0]["line"],
        "SCROLL role:iframe \"Incident history\" to 50%"
    );
    assert_eq!(step["act"]["actions"][0]["plannedBy"], "jev");
    assert_eq!(step["act"]["usage"]["modelCalls"], 0);
    let log = twin.request_log();
    assert!(
        log.contains(r#"\"e3\":{\"role\":\"iframe\",\"name\":\"Incident history\""#),
        "Jev reads each frame by its title; log:\n{log}"
    );
    assert!(
        log.contains(r#"\"e4\":{\"role\":\"iframe\",\"name\":\"Advertisement\""#),
        "log:\n{log}"
    );
}

#[test]
fn jev_leaves_a_scroll_to_the_model_when_unsure_how() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.jev(&[jev_intent("scroll", 0.95)]);
    twin.answer(&[scroll_answer("nextChunk", &[])]);
    let flow = dir.file(
        "jev-scroll.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{}ACT \"scroll down a page\"\n\
             ASSERT eval \"window.scrollY > 0\" == true\n",
            visit_html(TALL)
        ),
    );
    let output = twin.run_jev(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    let step = act_step(&dir);
    assert_eq!(step["act"]["actions"][0]["line"], "SCROLL down");
    assert_eq!(step["act"]["actions"][0]["plannedBy"], "llm");
}

#[test]
fn jev_drags_without_a_model_call_when_it_is_sure() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.jev(&[
        jev_intent("drag", 0.95),
        jev_pick("e3", &["e2", "e4"], 0.95),
        jev_pick("e4", &["e2"], 0.95),
    ]);
    let flow = dir.file(
        "jev-drag-sure.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{}ACT \"drag the card to Done\"\n\
             ASSERT role:heading \"Dropped\" visible\n",
            visit_html(BOARD)
        ),
    );
    let output = twin.run_jev(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    let step = act_step(&dir);
    assert_eq!(
        step["act"]["actions"][0]["line"],
        "DRAG role:button \"Card\" to role:button \"Done\""
    );
    assert_eq!(step["act"]["actions"][0]["plannedBy"], "jev");
    assert_eq!(step["act"]["usage"]["modelCalls"], 0);
    assert_eq!(step["act"]["usage"]["jev"]["requests"], 3);
    let log = twin.request_log();
    assert!(
        log.contains("Onto which element or area does the instruction ask to drop"),
        "log:\n{log}"
    );
}

#[test]
fn jev_leaves_an_unsure_drag_to_the_model() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.jev(&[jev_intent("drag", 0.95), jev_pick("e3", &["e2", "e4"], 0.4)]);
    twin.answer(&[drag("e3", "e4")]);
    let flow = dir.file(
        "jev-drag.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{}ACT \"drag the card to Done\"\n\
             ASSERT role:heading \"Dropped\" visible\n",
            visit_html(BOARD)
        ),
    );
    let output = twin.run_jev(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    let step = act_step(&dir);
    assert_eq!(step["act"]["actions"][0]["plannedBy"], "llm");
    assert_eq!(step["act"]["usage"]["jev"]["requests"], 2);
}

#[test]
fn an_unsure_jev_leaves_the_step_to_the_model_with_its_likely_matches() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.jev(&[
        jev_intent("click", 0.9),
        jev_answer(&json!({
            "strict": {"type": "choice", "choice": "e3", "confidence": 0.6,
                       "probabilities": {"e3": 0.6, "e4": 0.3, "none_match": 0.1}},
            "best": {"type": "choice", "choice": "e3", "confidence": 0.6,
                     "probabilities": {"e3": 0.6, "e4": 0.4}}
        })),
    ]);
    twin.answer(&[click("e4", false)]);
    let flow = dir.file(
        "jev-unsure.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{}ACT \"share it\"\n\
             ASSERT role:heading \"Shared\" visible\n",
            visit_html(TWO_BUTTONS)
        ),
    );
    let output = twin.run_jev(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");

    let step = act_step(&dir);
    assert_eq!(step["act"]["planner"], "jev");
    assert_eq!(step["act"]["actions"][0]["plannedBy"], "llm");
    assert_eq!(step["act"]["usage"]["modelCalls"], 1);
    assert_eq!(step["act"]["usage"]["jev"]["requests"], 2);
    let log = twin.request_log();
    assert!(
        log.contains("A classifier found these likely matches"),
        "the model sees Jev's likely matches; log:\n{log}"
    );
    assert!(log.contains("- e3: button"), "log:\n{log}");
}

#[test]
fn a_failed_jev_request_leaves_the_step_to_the_model() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.jev(&[json!({
        "kind": "transcript",
        "status": 401,
        "content_type": "application/json",
        "body": {"detail": {"error_type": "authentication_error", "message": "Cannot authenticate"}}
    })]);
    twin.answer(&[click("e3", false)]);
    let flow = dir.file(
        "jev-error.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{SHOP}ACT \"add the item to the cart\"\n\
             ASSERT role:heading \"Added\" visible\n"
        ),
    );
    let output = twin.run_jev(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    let step = act_step(&dir);
    assert_eq!(step["act"]["actions"][0]["plannedBy"], "llm");
    assert_eq!(step["act"]["usage"]["jev"]["requests"], 1);
}

#[test]
fn jev_fills_a_masked_value_that_never_reaches_it() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.jev(&[jev_intent("fill", 0.95), jev_only("e3", 0.9)]);
    let flow = dir.file(
        "jev-secret.whirl",
        "[Options]\nmodel: gpt-test\n\
         VISIT \"data:text/html,<h1>Login</h1><input type=password aria-label=Password>\"\n\
         ACT \"type {{env.WHIRL_ACT_SECRET}} into the password field\"\n\
         ASSERT label:Password value == {{env.WHIRL_ACT_SECRET}}\n",
    );
    let secret = "hunter2-jev-secret";
    let output = twin.run_jev(&dir, &flow, &[("WHIRL_ACT_SECRET", secret)]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");

    let log = twin.request_log();
    assert!(!log.contains(secret), "the secret reached Jev:\n{log}");
    assert!(log.contains("%env.WHIRL_ACT_SECRET%"), "log:\n{log}");
    let step = act_step(&dir);
    assert_eq!(step["act"]["actions"][0]["plannedBy"], "jev");
    assert_eq!(
        step["act"]["actions"][0]["line"],
        "FILL role:textbox \"Password\" \"%env.WHIRL_ACT_SECRET%\""
    );
}

#[test]
fn jev_without_a_key_is_a_runtime_error() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    let flow = dir.file(
        "jev-no-key.whirl",
        &format!("[Options]\nmodel: gpt-test\n{SHOP}ACT \"add the item to the cart\"\n"),
    );
    let output = twin.run_with_args(&dir, &flow, &[("TYPESAFE_API_KEY", "")], &["--jev"]);
    assert_eq!(exit_code(&output), 3);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--jev needs TYPESAFE_API_KEY"),
        "stderr:\n{stderr}"
    );
}

#[test]
fn jev_reads_unquoted_text_to_type_with_a_small_model_call() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.jev(&[jev_intent("fill", 0.95), jev_only("e3", 0.9)]);
    // The model re-cases the text; Whirl types the instruction's own.
    twin.answer(&[json!({"text": "lovelace"})]);
    let page = "<h1>Profile</h1><input aria-label=\"Last name\">";
    let flow = dir.file(
        "jev-text.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{}ACT \"type Lovelace into the last name field\"\n\
             ASSERT label:\"Last name\" value == Lovelace\n",
            visit_html(page)
        ),
    );
    let output = twin.run_jev(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");

    let step = act_step(&dir);
    assert_eq!(step["act"]["actions"][0]["plannedBy"], "jev");
    assert_eq!(
        step["act"]["actions"][0]["line"],
        "FILL role:textbox \"Last name\" \"Lovelace\""
    );
    assert_eq!(step["act"]["usage"]["modelCalls"], 1);
    let log = twin.request_log();
    assert!(
        log.contains("the literal text the user wants typed"),
        "log:\n{log}"
    );
    assert!(
        !log.contains("Accessibility Tree"),
        "the text call must not send the page; log:\n{log}"
    );
}

#[test]
fn jev_looks_at_every_named_element_when_no_control_fits() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    // The page has no button or link, so the pointer view is empty and the
    // broad tier offers the heading and the plain div.
    twin.jev(&[
        jev_intent("click", 0.95),
        jev_answer(&json!({
            "strict": jev_choice(&["e2", "e3", "none_match"], "e3", 0.9),
            "best": jev_choice(&["e2", "e3"], "e3", 0.92),
        })),
    ]);
    let page = "<h1>Menu</h1>\
        <div onclick=\"document.querySelector('h1').textContent='Opened'\">Open the menu</div>";
    let flow = dir.file(
        "jev-broad.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{}ACT \"open the menu\"\n\
             ASSERT role:heading \"Opened\" visible\n",
            visit_html(page)
        ),
    );
    let output = twin.run_jev(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    let step = act_step(&dir);
    assert_eq!(step["act"]["actions"][0]["plannedBy"], "jev");
    assert_eq!(step["act"]["usage"]["modelCalls"], 0);
}

#[test]
fn copies_of_one_control_in_one_item_share_jevs_vote() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    // Neither copy wins alone, but together they pass and strict is sure
    // something fits.
    twin.jev(&[
        jev_intent("click", 0.95),
        jev_answer(&json!({
            "strict": jev_choice(&["e5", "e6", "none_match"], "e5", 0.6),
            "best": {"type": "choice", "choice": "e5", "confidence": 0.3,
                     "probabilities": {"e5": 0.5, "e6": 0.5}},
        })),
    ]);
    let page = "<h1>Shop</h1><ul><li>Blue mug \
        <button onclick=\"document.querySelector('h1').textContent='Added'\">Add to cart</button>\
        <button onclick=\"document.querySelector('h1').textContent='Added'\">Add to cart</button>\
        </li></ul>";
    let flow = dir.file(
        "jev-copies.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{}ACT \"add the blue mug to the cart\"\n\
             ASSERT role:heading \"Added\" visible\n",
            visit_html(page)
        ),
    );
    let output = twin.run_jev(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    let step = act_step(&dir);
    assert_eq!(step["act"]["actions"][0]["plannedBy"], "jev");
    assert_eq!(step["act"]["usage"]["modelCalls"], 0);
}

#[test]
fn jev_clicks_the_named_option_of_a_custom_listbox() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    // Sure it is a select, but the page has no native select: the option
    // the instruction names is clicked, after Jev confirms it.
    twin.jev(&[jev_intent("select", 0.98), jev_only("e5", 0.95)]);
    let page = "<h1>Country</h1><ul role=listbox aria-label=Country>\
        <li role=option onclick=\"document.querySelector('h1').textContent='Poland chosen'\">Poland</li>\
        <li role=option onclick=\"document.querySelector('h1').textContent='Portugal chosen'\">Portugal</li></ul>";
    let flow = dir.file(
        "jev-listbox.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{}ACT \"choose Portugal from the country list\"\n\
             ASSERT role:heading \"Portugal chosen\" visible\n",
            visit_html(page)
        ),
    );
    let output = twin.run_jev(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    let step = act_step(&dir);
    assert_eq!(
        step["act"]["actions"][0]["line"],
        "CLICK role:option \"Portugal\""
    );
    assert_eq!(step["act"]["actions"][0]["plannedBy"], "jev");
    assert_eq!(step["act"]["usage"]["modelCalls"], 0);
    assert_eq!(step["act"]["usage"]["jev"]["requests"], 2);
}

#[test]
fn jev_opens_a_custom_dropdown_and_chooses_the_named_option() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    // No option shows until the button opens the list: Jev picks the
    // button as step one, then the named option on the fresh snapshot.
    twin.jev(&[
        jev_intent("select", 0.98),
        jev_only("e3", 0.9),
        jev_only("e6", 0.95),
    ]);
    let page = "<h1>Mug</h1>\
        <button onclick=\"document.getElementById('colors').hidden=false\">Choose a color</button>\
        <ul id=colors hidden>\
        <li style=\"cursor:pointer\" onclick=\"document.querySelector('h1').textContent='Red chosen'\">Red</li>\
        <li style=\"cursor:pointer\" onclick=\"document.querySelector('h1').textContent='Blue chosen'\">Blue</li></ul>";
    let flow = dir.file(
        "jev-dropdown.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{}ACT \"choose Blue from the color dropdown\"\n\
             ASSERT role:heading \"Blue chosen\" visible\n",
            visit_html(page)
        ),
    );
    let output = twin.run_jev(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "stdout:\n{stdout}");
    let step = act_step(&dir);
    let actions = step["act"]["actions"].as_array().expect("actions");
    assert_eq!(actions.len(), 2);
    assert_eq!(actions[0]["line"], "CLICK role:button \"Choose a color\"");
    assert_eq!(actions[1]["line"], "CLICK role:listitem");
    assert!(actions.iter().all(|action| action["plannedBy"] == "jev"));
    assert_eq!(step["act"]["usage"]["modelCalls"], 0);
    assert_eq!(step["act"]["usage"]["jev"]["requests"], 3);
}
