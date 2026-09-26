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
             [Asserts]\nrole:heading \"Added\" visible\n"
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
         [Asserts]\nlabel:Password value == {{env.WHIRL_ACT_SECRET}}\n",
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
         [Asserts]\nrole:heading \"Large chosen\" visible\n",
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
    for engine in ["firefox", "webkit"] {
        let dir = TestDir::new();
        let twin = ModelTwin::start();
        twin.answer(&[click("e3", false)]);
        let flow = dir.file(
            "click.whirl",
            &format!(
                "[Options]\nmodel: gpt-test\n{SHOP}ACT \"add the item to the cart\"\n\
                 [Asserts]\nrole:heading \"Added\" visible\n"
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
             [Asserts]\nrole:heading \"Added\" visible\n",
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
             [Asserts]\nrole:heading \"Canada chosen\" visible\n",
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
             [Asserts]\nrole:heading \"Bought\" visible\n",
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
             [Asserts]\nrole:heading \"clicked\" visible\n",
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
