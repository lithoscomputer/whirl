//! `ai:` target, AI cache, and heal acceptance tests (SPEC 6.3, 12.1):
//! each test starts the `whirl` binary against the real shim and Chromium,
//! and points `WHIRL_LLM_ENDPOINT` at an in-process OpenAI-compatible model
//! twin whose answers the test scripts.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use std::{env, fs, process};

use axum::Router;
use axum::response::Html;
use axum::routing::get;
use reqwest::blocking::{Client as HttpClient, Response};
use serde_json::{Value as Json, json};
use tokio::net::TcpListener;
use tokio::runtime::{Builder, Runtime};
use tokio::time::sleep;
use twin_openai::config::{Config, Mode, RecordFormat};

/// A unique temporary directory for one test, removed on drop.
struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!("whirl-ai-test-{}-{id}", process::id()));
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
            .arg("run")
            .arg("--out")
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

/// The JSON report's steps of the flow's first file.
fn steps(dir: &TestDir) -> Vec<Json> {
    let text = fs::read_to_string(dir.path.join("report.json")).expect("the JSON report exists");
    let report: Json = serde_json::from_str(&text).expect("the JSON report parses");
    report["files"][0]["entries"]
        .as_array()
        .expect("the file has entries")
        .iter()
        .flat_map(|entry| entry["steps"].as_array().cloned().unwrap_or_default())
        .collect()
}

/// A model answer that lists the elements it found.
fn found(elements: &[&str]) -> Json {
    let elements: Vec<Json> = elements
        .iter()
        .map(|element| json!({"elementId": element, "description": "an element"}))
        .collect();
    json!({"elements": elements})
}

const SHOP: &str = "VISIT \"data:text/html,<h1>Shop</h1>\
    <button onclick=\\\"document.querySelector('h1').textContent='Added'\\\">Add to cart</button>\
    <button>Wish list</button>\"\n";

#[test]
fn an_ai_target_clicks_the_one_element_the_model_finds() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[found(&["e3"])]);
    let flow = dir.file(
        "click.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{SHOP}CLICK ai:\"the add to cart button\"\n\
             ASSERT heading:\"Added\" visible\n"
        ),
    );
    let output = twin.run(&dir, &flow, &[]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
    let click = &steps(&dir)[1];
    assert_eq!(click["ai"]["model"], "gpt-test");
    assert_eq!(
        click["ai"]["targets"][0]["target"],
        "ai:\"the add to cart button\""
    );
    assert_eq!(
        click["ai"]["targets"][0]["locator"],
        "button:\"Add to cart\""
    );
    assert_eq!(click["ai"]["usage"]["modelCalls"], 1);
    let log = twin.request_log();
    assert!(log.contains("the add to cart button"), "{log}");
}

#[test]
fn an_ai_target_that_matches_two_elements_fails_with_strictness() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[found(&["e3", "e4"])]);
    let flow = dir.file(
        "strict.whirl",
        &format!("[Options]\nmodel: gpt-test\n{SHOP}CLICK ai:\"a button\"\n"),
    );
    let output = twin.run(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 1, "{stdout}");
    let click = &steps(&dir)[1];
    assert_eq!(click["error"]["code"], "strictness");
    let candidates = click["error"]["candidates"].to_string();
    assert!(candidates.contains("Add to cart"), "{candidates}");
    assert!(candidates.contains("Wish list"), "{candidates}");
}

#[test]
fn an_ai_absence_check_passes_when_the_model_finds_nothing() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[found(&[]), found(&[])]);
    let flow = dir.file(
        "absent.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{SHOP}ASSERT ai:\"an error banner\" hidden\n\
             ASSERT ai:\"a sold-out label\" text not exists\n"
        ),
    );
    let output = twin.run(&dir, &flow, &[]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
    let steps = steps(&dir);
    assert_eq!(steps[1]["ai"]["targets"][0]["cache"], "uncached");
    assert_eq!(steps[2]["ai"]["targets"][0]["cache"], "uncached");
}

#[test]
fn an_ai_check_and_capture_read_the_element_the_model_finds() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[found(&["e2"]), found(&["e4"])]);
    let flow = dir.file(
        "read.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{SHOP}ASSERT ai:\"the page title\" text == Shop\n\
             CAPTURE second: ai:\"the second button\" text\n\
             ASSERT button:{{{{second}}}} visible\n"
        ),
    );
    let output = twin.run(&dir, &flow, &[]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
}

#[test]
fn an_ai_target_asks_again_until_the_element_appears() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[found(&[]), found(&["e3"])]);
    let flow = dir.file(
        "late.whirl",
        "[Options]\nmodel: gpt-test\nVISIT \"data:text/html,<h1>Shop</h1><script>\
         setTimeout(() => { const b = document.createElement('button'); b.textContent = 'Late'; \
         b.onclick = () => { document.querySelector('h1').textContent = 'Clicked'; }; \
         document.body.append(b); }, 1000)</script>\"\n\
         CLICK ai:\"the late button\" @10s\nASSERT heading:Clicked visible\n",
    );
    let output = twin.run(&dir, &flow, &[]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
    assert_eq!(steps(&dir)[1]["ai"]["usage"]["modelCalls"], 2);
}

/// The flow's AI cache as JSON, when it exists.
fn cache_of(flow: &Path) -> Option<Json> {
    let mut name = flow.file_name().expect("a file name").to_os_string();
    name.push("-cache.json");
    let text = fs::read_to_string(flow.with_file_name(name)).ok()?;
    Some(serde_json::from_str(&text).expect("the cache is JSON"))
}

/// The step warnings' codes, in order.
fn warning_codes(dir: &TestDir) -> Vec<String> {
    steps(dir)
        .iter()
        .flat_map(|step| step["warnings"].as_array().cloned().unwrap_or_default())
        .map(|warning| warning["code"].as_str().unwrap_or_default().to_owned())
        .collect()
}

fn buy_flow(dir: &TestDir, button: &str) -> PathBuf {
    dir.file(
        "buy.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n\
             VISIT \"data:text/html,<h1>Shop</h1><button data-testid=wish>Wish list</button>\
             <button onclick=\\\"document.querySelector('h1').textContent='Bought'\\\">{button}</button>\"\n\
             CLICK ai:\"the buy button\"\nASSERT heading:Bought visible\n"
        ),
    )
}

#[test]
fn the_cache_records_a_target_and_later_runs_replay_it_without_the_model() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    let flow = buy_flow(&dir, "Buy now");

    // A replay run with no entry asks the model, warns, and writes nothing.
    twin.answer(&[found(&["e4"])]);
    let output = twin.run(&dir, &flow, &[]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
    assert_eq!(warning_codes(&dir), ["cache-miss"]);
    assert!(
        stdout_text(&output).contains(
            "AI cache: 1 step missed and 0 steps healed; run with --cache=update to write the cache"
        ),
        "{}",
        stdout_text(&output)
    );
    assert_eq!(cache_of(&flow), None);

    // An update run writes the entry.
    twin.answer(&[found(&["e4"])]);
    let output = twin.run_with_args(&dir, &flow, &[], &["--cache", "update"]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
    assert_eq!(
        cache_of(&flow),
        Some(json!({
            "version": 1,
            "entries": [{
                "kind": "ai-target",
                "line": "CLICK ai:\"the buy button\"",
                "occurrence": 1,
                "target": "ai:\"the buy button\"",
                "model": "gpt-test",
                "locator": "button:\"Buy now\"",
                "fingerprint": {"role": "button", "name": "Buy now"}
            }]
        }))
    );

    // A replay run hits the entry: no model call and no warning. The twin
    // has no answer queued, so any call would fail the run.
    let calls = twin.request_log().matches("chat/completions").count();
    let output = twin.run_with_args(&dir, &flow, &[], &["--cache", "only"]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
    assert_eq!(
        twin.request_log().matches("chat/completions").count(),
        calls
    );
    assert_eq!(steps(&dir)[1]["ai"]["targets"][0]["cache"], "hit");
    assert_eq!(steps(&dir)[1]["ai"]["usage"]["modelCalls"], 0);
    assert!(warning_codes(&dir).is_empty());
}

#[test]
fn a_redesign_heals_the_target_and_update_rewrites_the_entry() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    let flow = buy_flow(&dir, "Buy now");
    twin.answer(&[found(&["e4"])]);
    let output = twin.run_with_args(&dir, &flow, &[], &["--cache", "update"]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));

    // The button's name changes: the fingerprint no longer fits.
    buy_flow(&dir, "Purchase");
    let output = twin.run_with_args(&dir, &flow, &[], &["--cache", "only"]);
    assert_eq!(exit_code(&output), 1, "{}", stdout_text(&output));
    assert_eq!(steps(&dir)[1]["error"]["code"], "cache-miss");

    twin.answer(&[found(&["e4"])]);
    let output = twin.run(&dir, &flow, &[]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
    assert_eq!(warning_codes(&dir), ["healed"]);
    let target = &steps(&dir)[1]["ai"]["targets"][0];
    assert_eq!(target["cache"], "healed");
    assert_eq!(target["cached"], "button:\"Buy now\"");
    assert_eq!(target["locator"], "button:Purchase");

    twin.answer(&[found(&["e4"])]);
    let output = twin.run_with_args(&dir, &flow, &[], &["--cache", "update"]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
    let cache = cache_of(&flow).expect("the cache exists");
    assert_eq!(cache["entries"][0]["locator"], "button:Purchase");
}

#[test]
fn update_removes_entries_the_run_did_not_use_and_a_failed_file_writes_nothing() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    let flow = buy_flow(&dir, "Buy now");
    twin.answer(&[found(&["e4"])]);
    let output = twin.run_with_args(&dir, &flow, &[], &["--cache", "update"]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
    let written = cache_of(&flow).expect("the cache exists");

    // A failing file writes nothing.
    fs::write(
        &flow,
        fs::read_to_string(&flow)
            .expect("the flow reads")
            .replace("Bought visible", "Nothing visible @1s"),
    )
    .expect("the flow writes");
    let output = twin.run_with_args(&dir, &flow, &[], &["--cache", "update"]);
    assert_eq!(exit_code(&output), 1, "{}", stdout_text(&output));
    assert_eq!(cache_of(&flow), Some(written));

    // A passing file without the line removes its entry, and the file.
    dir.file("buy.whirl", "VISIT \"data:text/html,<h1>Shop</h1>\"\n");
    let output = twin.run_with_args(&dir, &flow, &[], &["--cache", "update"]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
    assert_eq!(cache_of(&flow), None);
}

#[test]
fn a_locator_that_matches_two_elements_gets_nth_and_one_that_finds_none_is_not_cached() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[found(&["e3"]), found(&["e4"])]);
    let flow = dir.file(
        "same.whirl",
        "[Options]\nmodel: gpt-test\n\
         VISIT \"data:text/html,<p>Item</p><p>Item</p><div><span>Other</span></div>\"\n\
         ASSERT ai:\"the second item\" text == Item\n\
         ASSERT ai:\"the other box\" text == Other\n",
    );
    let output = twin.run_with_args(&dir, &flow, &[], &["--cache", "update"]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
    let steps = steps(&dir);
    // `text:Item` matches both paragraphs, so the generator adds `nth:`.
    assert_eq!(
        steps[1]["ai"]["targets"][0]["locator"],
        "text:Item >> nth:1"
    );
    assert_eq!(steps[1]["ai"]["targets"][0]["cache"], "miss");
    // `text:Other` finds the span, not the box around it.
    assert_eq!(steps[2]["ai"]["targets"][0]["cache"], "uncached");
    assert_eq!(warning_codes(&dir), ["cache-miss", "cache-unstable"]);
    let cache = cache_of(&flow).expect("the cache exists");
    assert_eq!(cache["entries"].as_array().map(Vec::len), Some(1));
}

fn click(element_id: &str) -> Json {
    json!({
        "action": {"elementId": element_id, "description": "the button", "method": "click", "arguments": []},
        "twoStep": false
    })
}

fn act_flow(dir: &TestDir, button: &str) -> PathBuf {
    dir.file(
        "act.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n\
             VISIT \"data:text/html,<h1>Shop</h1>\
             <button onclick=\\\"document.querySelector('h1').textContent='Added'\\\">{button}</button>\"\n\
             ACT \"add the item to the cart\"\nASSERT heading:Added visible\n"
        ),
    )
}

#[test]
fn an_act_line_is_cached_and_replays_without_the_model() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    let flow = act_flow(&dir, "Add to cart");
    twin.answer(&[click("e3")]);
    let output = twin.run_with_args(&dir, &flow, &[], &["--cache", "update"]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
    assert_eq!(warning_codes(&dir), ["cache-miss"]);
    assert_eq!(
        cache_of(&flow),
        Some(json!({
            "version": 1,
            "entries": [{
                "kind": "act",
                "line": "ACT \"add the item to the cart\"",
                "occurrence": 1,
                "model": "gpt-test",
                "actions": [{
                    "line": "CLICK button:\"Add to cart\"",
                    "fingerprints": [{"role": "button", "name": "Add to cart"}]
                }]
            }]
        }))
    );

    let output = twin.run_with_args(&dir, &flow, &[], &["--cache", "only"]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
    let act = &steps(&dir)[1]["act"];
    assert_eq!(act["cache"], "hit");
    assert_eq!(act["usage"]["modelCalls"], 0);
    assert_eq!(act["actions"][0]["plannedBy"], "cache");
}

#[test]
fn a_cached_act_line_heals_when_the_page_changes() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    let flow = act_flow(&dir, "Add to cart");
    twin.answer(&[click("e3")]);
    let output = twin.run_with_args(&dir, &flow, &[], &["--cache", "update"]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));

    act_flow(&dir, "Add to bag");
    let output = twin.run_with_args(&dir, &flow, &[], &["--cache", "only"]);
    assert_eq!(exit_code(&output), 1, "{}", stdout_text(&output));
    assert_eq!(steps(&dir)[1]["error"]["code"], "cache-miss");

    twin.answer(&[click("e3")]);
    let output = twin.run(&dir, &flow, &[]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
    let act = &steps(&dir)[1]["act"];
    assert_eq!(act["cache"], "healed");
    assert_eq!(act["cached"], json!(["CLICK button:\"Add to cart\""]));
    assert_eq!(warning_codes(&dir), ["healed"]);
}

#[test]
fn a_cached_act_line_stores_a_secret_as_its_reference() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[json!({
        "action": {"elementId": "e3", "description": "password field", "method": "fill",
                   "arguments": ["%env.WHIRL_AI_SECRET%"]},
        "twoStep": false
    })]);
    let flow = dir.file(
        "secret.whirl",
        "[Options]\nmodel: gpt-test\n\
         VISIT \"data:text/html,<h1>Login</h1><input type=password aria-label=Password>\"\n\
         ACT \"type {{env.WHIRL_AI_SECRET}} into the password field\"\n\
         ASSERT label:Password value == {{env.WHIRL_AI_SECRET}}\n",
    );
    let secret = "hunter2-cache-secret";
    let env = [("WHIRL_AI_SECRET", secret)];
    let output = twin.run_with_args(&dir, &flow, &env, &["--cache", "update"]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
    let cache = cache_of(&flow).expect("the cache exists");
    assert!(!cache.to_string().contains(secret), "{cache}");
    assert_eq!(
        cache["entries"][0]["actions"][0]["line"],
        "FILL textbox:Password \"{{env.WHIRL_AI_SECRET}}\""
    );

    // The replay types the current value of the variable.
    let other = [("WHIRL_AI_SECRET", "another-secret")];
    let output = twin.run_with_args(&dir, &flow, &other, &["--cache", "only"]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
}

#[test]
fn an_act_action_that_fails_heals_once_with_a_new_plan() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    // The model first picks the button under the banner, which never
    // becomes clickable; the heal picks the other one.
    twin.answer(&[click("e3"), click("e4")]);
    let flow = dir.file(
        "covered.whirl",
        "[Options]\nmodel: gpt-test\n\
         VISIT \"data:text/html,<h1>Shop</h1>\
         <button style='position:absolute;top:10px'>Buy</button>\
         <button style='position:absolute;top:200px' \
         onclick=\\\"document.querySelector('h1').textContent='Bought'\\\">Buy now</button>\
         <div style='position:fixed;top:0;left:0;width:100%;height:100px;background:white'>Banner</div>\"\n\
         ACT \"buy the item\" @6s\nASSERT heading:Bought visible\n",
    );
    let output = twin.run(&dir, &flow, &[]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
    let act = &steps(&dir)[1]["act"];
    assert_eq!(act["usage"]["modelCalls"], 2);
    assert_eq!(act["actions"][0]["line"], "CLICK button:\"Buy now\"");
}

#[test]
fn the_generator_prefers_a_test_id_and_names_an_iframe_by_its_title() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[found(&["e3"]), found(&["f1e3"])]);
    let flow = dir.file(
        "frame.whirl",
        "[Options]\nmodel: gpt-test\n\
         VISIT \"data:text/html,<h1>Pay</h1><button data-testid=pay-now>Pay now</button>\
         <iframe title=Payment srcdoc='<label>Card <input></label>'></iframe>\"\n\
         ASSERT ai:\"the pay button\" visible\n\
         FILL ai:\"the card field in the payment frame\" 4242\n",
    );
    let output = twin.run_with_args(&dir, &flow, &[], &["--cache", "update"]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
    let cache = cache_of(&flow).expect("the cache exists");
    assert_eq!(cache["entries"][0]["locator"], "testid:pay-now");
    assert_eq!(
        cache["entries"][1]["locator"],
        "frame:iframe[title='Payment'] >> textbox:Card"
    );
}

const ORDER: &str = "VISIT \"data:text/html,<h1>Order</h1><section data-testid=summary>\
    <p>Mug $12.50</p><p>Plate $8.00</p><p>Total $20.50</p>\
    <a href='/docs/returns'>Returns</a></section>\"\n";

#[test]
fn extract_reads_a_typed_value_that_later_checks_read() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[json!({"total": 20.5, "items": ["Mug", "Plate"], "coupon": null})]);
    let flow = dir.file(
        "order.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{ORDER}\
             EXTRACT order testid:summary \"the order total and line items\"\n\
             {{\n\
                 \"type\": \"object\",\n\
                 \"properties\": {{\n\
                     \"total\": {{ \"type\": \"number\" }},\n\
                     \"items\": {{ \"type\": \"array\", \"items\": {{ \"type\": \"string\" }} }},\n\
                     \"coupon\": {{ \"type\": \"string\" }}\n\
                 }},\n\
                 \"required\": [\"total\", \"items\"]\n\
             }}\n\
             ASSERT extract:order json:$.total == 20.5\n\
             ASSERT extract:order json:$.items count == 2\n\
             ASSERT extract:order json:$.coupon not exists\n\
             CAPTURE first: extract:order json:$.items[0]\n\
             ASSERT text:~{{{{first}}}} visible\n"
        ),
    );
    let output = twin.run(&dir, &flow, &[]);
    assert_eq!(
        exit_code(&output),
        0,
        "{}{}",
        stdout_text(&output),
        String::from_utf8_lossy(&output.stderr)
    );
    let extract = &steps(&dir)[1]["extract"];
    assert_eq!(extract["model"], "gpt-test");
    assert_eq!(
        extract["value"],
        json!({"type": "object", "value": {"total": 20.5, "items": ["Mug", "Plate"]}})
    );
    assert_eq!(extract["usage"]["modelCalls"], 1);
    let log = twin.request_log();
    assert!(log.contains("the order total and line items"), "{log}");
}

#[test]
fn extract_without_a_schema_reads_text_and_null_is_missing() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[json!({"value": "Total $20.50"}), json!({"value": null})]);
    let flow = dir.file(
        "text.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{ORDER}\
             EXTRACT total \"the total line\"\n\
             EXTRACT coupon \"the coupon code\"\n\
             ASSERT extract:total == \"Total $20.50\"\n\
             ASSERT extract:coupon not exists\n"
        ),
    );
    let output = twin.run(&dir, &flow, &[]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
}

#[test]
fn extract_turns_a_link_ref_into_an_absolute_url() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[json!({"value": "e6"})]);
    let flow = dir.file(
        "link.whirl",
        "[Options]\nmodel: gpt-test\n\
         VISIT \"data:text/html,<h1>Order</h1><p>Mug</p><p>Plate</p><p>Total</p>\
         <a href='https://shop.test/docs/returns'>Returns</a>\"\n\
         EXTRACT returns \"the returns link\"\n\
         { \"type\": \"string\", \"format\": \"uri\" }\n\
         ASSERT extract:returns == \"https://shop.test/docs/returns\"\n",
    );
    let output = twin.run(&dir, &flow, &[]);
    let stdout = stdout_text(&output);
    assert_eq!(exit_code(&output), 0, "{stdout}\n{}", twin.request_log());
}

#[test]
fn a_null_answer_for_a_non_object_schema_is_missing() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[json!({"value": null})]);
    let flow = dir.file(
        "missing.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{ORDER}\
             EXTRACT coupon \"the coupon code\"\n{{ \"type\": \"string\" }}\n\
             ASSERT extract:coupon not exists\n"
        ),
    );
    let output = twin.run(&dir, &flow, &[]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
}

#[test]
fn an_extract_answer_outside_its_schema_fails_the_entry() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[json!({"value": "twenty"})]);
    let flow = dir.file(
        "bad.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{ORDER}\
             EXTRACT total \"the total\"\n{{ \"type\": \"number\" }}\n"
        ),
    );
    let output = twin.run(&dir, &flow, &[]);
    assert_eq!(exit_code(&output), 1, "{}", stdout_text(&output));
    let step = &steps(&dir)[1];
    assert_eq!(step["error"]["code"], "extract-schema");
}

/// A `JUDGE` answer.
fn verdict(verdict: &str, reason: &str) -> Json {
    json!({"verdict": verdict, "reason": reason})
}

#[test]
fn judge_passes_on_yes_and_sends_the_screenshot_and_the_outline() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[verdict("yes", "the total is the sum of the items")]);
    let flow = dir.file(
        "judge.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{ORDER}\
             ASSERT testid:summary visible\n\
             JUDGE testid:summary \"the total matches the sum of the line items\"\n"
        ),
    );
    let output = twin.run(&dir, &flow, &[]);
    assert_eq!(
        exit_code(&output),
        0,
        "{}{}",
        stdout_text(&output),
        String::from_utf8_lossy(&output.stderr)
    );
    let step = &steps(&dir)[2];
    assert_eq!(step["kind"], "judge");
    assert_eq!(
        step["text"],
        "JUDGE testid:summary \"the total matches the sum of the line items\""
    );
    assert_eq!(step["judge"]["model"], "gpt-test");
    assert_eq!(step["judge"]["verdict"], "yes");
    assert_eq!(step["judge"]["reason"], "the total is the sum of the items");
    assert_eq!(step["judge"]["usage"]["modelCalls"], 1);
    let html = fs::read_to_string(dir.path.join("report.html")).expect("the HTML report exists");
    assert!(
        html.contains(
            "<dt>Verdict</dt><dd>yes</dd><dt>Reason</dt><dd>the total is the sum of the items</dd>"
        ),
        "{html}"
    );
    let log = twin.request_log();
    assert!(
        log.contains("the total matches the sum of the line items"),
        "{log}"
    );
    // The twin checks the image part but does not log it.
    assert!(log.contains("Screenshot:"), "{log}");
    assert!(log.contains("Plate $8.00"), "{log}");
    assert!(!log.contains("heading \\\"Order\\\""), "{log}");
}

#[test]
fn judge_fails_on_no_with_the_models_reason() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[verdict("no", "the page shows an error banner")]);
    let flow = dir.file(
        "judge.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{ORDER}\
             ASSERT testid:summary visible\n\
             JUDGE \"the page shows no error message\"\n\
             ASSERT url exists\n"
        ),
    );
    let output = twin.run(&dir, &flow, &[]);
    assert_eq!(exit_code(&output), 1, "{}", stdout_text(&output));
    let stdout = stdout_text(&output);
    assert!(stdout.contains("judge-false"), "{stdout}");
    assert!(
        stdout.contains("the page shows an error banner"),
        "{stdout}"
    );
    let steps = steps(&dir);
    let error = &steps[2]["error"];
    assert_eq!(error["code"], "judge-false");
    assert_eq!(error["expected"], "the page shows no error message");
    assert_eq!(error["actual"], "the page shows an error banner");
    assert_eq!(steps[2]["judge"]["verdict"], "no");
    assert_eq!(
        steps[3]["status"], "skipped",
        "the entry stops at the failed JUDGE"
    );
}

#[test]
fn judge_warns_on_unsure_and_the_entry_goes_on() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[verdict("unsure", "the chart is cut off")]);
    let flow = dir.file(
        "judge.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{ORDER}\
             ASSERT testid:summary visible\n\
             JUDGE \"the chart trends upward\"\n\
             ASSERT url exists\n"
        ),
    );
    let output = twin.run(&dir, &flow, &[]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
    assert_eq!(warning_codes(&dir), ["judge-unsure"]);
    let steps = steps(&dir);
    assert_eq!(steps[2]["status"], "passed");
    assert_eq!(steps[2]["judge"]["verdict"], "unsure");
    assert_eq!(steps[3]["status"], "passed");
    assert!(
        stdout_text(&output).contains("the chart is cut off"),
        "{}",
        stdout_text(&output)
    );
}

#[test]
fn a_judge_claim_masks_secrets_and_never_uses_the_cache() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[verdict("yes", "it shows hunter2")]);
    let flow = dir.file(
        "judge.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{ORDER}\
             ASSERT testid:summary visible\n\
             JUDGE \"the page greets {{{{env.SECRET}}}}\"\n"
        ),
    );
    let output = twin.run_with_args(&dir, &flow, &[("SECRET", "hunter2")], &["--cache=only"]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
    let log = twin.request_log();
    assert!(!log.contains("greets hunter2"), "{log}");
    assert!(log.contains("greets "), "{log}");
    let step = &steps(&dir)[2];
    assert_eq!(step["judge"]["reason"], "it shows ***");
    assert!(cache_of(&flow).is_none());
}

#[test]
fn judge_without_credentials_stops_the_run_before_any_flow() {
    let dir = TestDir::new();
    let flow = dir.file(
        "judge.whirl",
        &format!(
            "[Options]\nmodel: anthropic/claude-sonnet-5\n{ORDER}\
             ASSERT testid:summary visible\n\
             JUDGE \"the page shows no error message\"\n"
        ),
    );
    let shim_js = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../shim/dist/index.js");
    let output = Command::new(env!("CARGO_BIN_EXE_whirl"))
        .env("WHIRL_NODE", "node")
        .env("WHIRL_SHIM_JS", shim_js)
        .env_remove("WHIRL_LLM_ENDPOINT")
        .env_remove("ANTHROPIC_API_KEY")
        .current_dir(&dir.path)
        .arg("run")
        .arg("--out")
        .arg(dir.path.join("artifacts"))
        .arg(&flow)
        .output()
        .expect("the whirl binary should run");
    assert_eq!(exit_code(&output), 3, "{}", stdout_text(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("JUDGE needs credentials for the model anthropic/claude-sonnet-5"),
        "{stderr}"
    );
    assert!(!dir.path.join("artifacts").exists());
}

const GOAL_SHOP: &str = "VISIT \"data:text/html,<h1>Shop</h1>\
    <label>Quantity <input id=q></label>\
    <button onclick=\\\"document.querySelector('output').textContent=document.querySelector('%23q').value\\\">Add to cart</button>\
    <p>Cart: <output data-testid=cart></output></p>\"\n";

/// A `GOAL` answer that runs one action.
fn goal_act(element: &str, method: &str, arguments: &[&str]) -> Json {
    json!({
        "status": "act",
        "reason": "one more step",
        "actions": [{"elementId": element, "description": "an element", "method": method, "arguments": arguments}]
    })
}

/// A `GOAL` answer that ends the goal.
fn goal_end(status: &str, reason: &str) -> Json {
    json!({"status": status, "reason": reason, "actions": []})
}

fn goal_flow(dir: &TestDir, goal: &str) -> PathBuf {
    dir.file(
        "goal.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{GOAL_SHOP}\
             GOAL \"{goal}\"\n\
             ASSERT testid:cart text == 2\n"
        ),
    )
}

#[test]
fn a_goal_runs_actions_until_done_and_the_cache_replays_its_path() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[
        goal_act("e4", "fill", &["2"]),
        goal_act("e5", "click", &[]),
        goal_end("done", "the cart holds 2"),
    ]);
    let flow = goal_flow(&dir, "add two to the cart");
    let output = twin.run_with_args(&dir, &flow, &[], &["--cache=update"]);
    assert_eq!(
        exit_code(&output),
        0,
        "{}{}",
        stdout_text(&output),
        twin.request_log()
    );
    let step = &steps(&dir)[1];
    let goal = &step["goal"];
    assert_eq!(goal["model"], "gpt-test");
    assert_eq!(goal["end"], "done");
    assert_eq!(goal["reason"], "the cart holds 2");
    assert_eq!(goal["cache"], "miss");
    assert_eq!(goal["usage"]["modelCalls"], 3);
    let lines: Vec<&str> = goal["actions"]
        .as_array()
        .expect("actions")
        .iter()
        .map(|action| action["line"].as_str().expect("a line"))
        .collect();
    assert_eq!(lines, [
        "FILL textbox:\"Quantity\" \"2\"",
        "CLICK button:\"Add to cart\""
    ]);
    assert_eq!(warning_codes(&dir), ["cache-miss"]);
    let log = twin.request_log();
    assert!(
        log.contains(
            "Steps taken so far: 1. FILL textbox:\\\"Quantity\\\" \\\"2\\\" 2. CLICK button:"
        ),
        "{log}"
    );
    let cache = cache_of(&flow).expect("the cache is written");
    assert_eq!(cache["entries"][0]["kind"], "goal");
    assert_eq!(cache["entries"][0]["line"], "GOAL \"add two to the cart\"");
    assert_eq!(
        cache["entries"][0]["actions"][0]["line"],
        "FILL textbox:Quantity \"2\""
    );

    // The replay makes no model call: the twin has no answer left.
    let output = twin.run(&dir, &flow, &[]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
    let goal = &steps(&dir)[1]["goal"];
    assert_eq!(goal["cache"], "hit");
    assert_eq!(goal["usage"]["modelCalls"], 0);
    assert_eq!(goal["actions"][1]["plannedBy"], "cache");
    assert!(warning_codes(&dir).is_empty());
    let html = fs::read_to_string(dir.path.join("report.html")).expect("the HTML report exists");
    assert!(html.contains("<dt>AI cache</dt><dd>hit</dd>"), "{html}");
}

#[test]
fn a_goal_the_model_calls_impossible_fails_with_its_reason() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[goal_end("impossible", "the shop has no checkout")]);
    let flow = goal_flow(&dir, "check out");
    let output = twin.run(&dir, &flow, &[]);
    assert_eq!(exit_code(&output), 1, "{}", stdout_text(&output));
    let step = &steps(&dir)[1];
    assert_eq!(step["error"]["code"], "goal-impossible");
    assert_eq!(step["error"]["actual"], "the shop has no checkout");
    assert_eq!(step["goal"]["end"], "impossible");
    assert!(cache_of(&flow).is_none());
    let html = fs::read_to_string(dir.path.join("report.html")).expect("the HTML report exists");
    assert!(
        html.contains("<dt>Ended</dt><dd>impossible: the shop has no checkout</dd>"),
        "{html}"
    );
}

#[test]
fn a_goal_stops_after_twenty_actions() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    let answers: Vec<Json> = (0..21).map(|_| goal_act("e5", "click", &[])).collect();
    twin.answer(&answers);
    let flow = goal_flow(&dir, "add two to the cart");
    let output = twin.run(&dir, &flow, &[]);
    assert_eq!(exit_code(&output), 1, "{}", stdout_text(&output));
    let step = &steps(&dir)[1];
    assert_eq!(step["error"]["code"], "goal-limit");
    assert_eq!(step["goal"]["actions"].as_array().map(Vec::len), Some(20));
    assert_eq!(step["goal"]["usage"]["modelCalls"], 21);
}

#[test]
fn a_failed_goal_action_goes_back_to_the_model() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    // Filling the button fails at once; the model then plans again.
    twin.answer(&[
        goal_act("e5", "fill", &["2"]),
        goal_act("e4", "fill", &["2"]),
        goal_act("e5", "click", &[]),
        goal_end("done", "the cart holds 2"),
    ]);
    let flow = goal_flow(&dir, "add two to the cart");
    let output = twin.run_with_args(&dir, &flow, &[], &["--cache=update"]);
    let log = twin.request_log();
    assert_eq!(exit_code(&output), 0, "{}{log}", stdout_text(&output));
    let actions = steps(&dir)[1]["goal"]["actions"].clone();
    assert_eq!(actions[0]["line"], "FILL button:\"Add to cart\" \"2\"");
    assert!(actions[0]["error"].is_string(), "{actions}");
    assert!(actions[1].get("error").is_none(), "{actions}");
    assert!(
        log.contains("1. FILL button:\\\"Add to cart\\\" \\\"2\\\" (failed: "),
        "{log}"
    );
    // The cache holds only the actions that ran.
    let cache = cache_of(&flow).expect("the cache is written");
    assert_eq!(
        cache["entries"][0]["actions"].as_array().map(Vec::len),
        Some(2)
    );
}

#[test]
fn a_goal_heals_from_the_current_page_when_a_cached_line_misses() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    let flow = goal_flow(&dir, "add two to the cart");
    fs::write(
        dir.path.join("goal.whirl-cache.json"),
        json!({
            "version": 1,
            "entries": [{
                "kind": "goal",
                "line": "GOAL \"add two to the cart\"",
                "occurrence": 1,
                "model": "gpt-test",
                "actions": [
                    {"line": "FILL label:Quantity 2", "fingerprints": [{"role": "textbox", "name": "Quantity"}]},
                    {"line": "CLICK button:Buy", "fingerprints": [{"role": "button", "name": "Buy"}]}
                ]
            }]
        })
        .to_string(),
    )
    .expect("the cache writes");
    // The model finds no element for the renamed button, so it plans the
    // rest of the goal.
    twin.answer(&[
        found(&[]),
        goal_act("e5", "click", &[]),
        goal_end("done", "the cart holds 2"),
    ]);
    let output = twin.run_with_args(&dir, &flow, &[], &["--cache=update"]);
    let log = twin.request_log();
    assert_eq!(exit_code(&output), 0, "{}{log}", stdout_text(&output));
    let goal = &steps(&dir)[1]["goal"];
    assert_eq!(goal["cache"], "healed");
    assert_eq!(goal["usage"]["modelCalls"], 3);
    assert_eq!(goal["actions"][0]["plannedBy"], "cache");
    assert_eq!(goal["actions"][1]["plannedBy"], "llm");
    assert_eq!(goal["cached"][1], "CLICK button:Buy");
    assert_eq!(warning_codes(&dir), ["healed"]);
    assert!(
        log.contains("Steps taken so far: 1. FILL label:Quantity 2 Accessibility"),
        "{log}"
    );
    let cache = cache_of(&flow).expect("the cache is written");
    assert_eq!(
        cache["entries"][0]["actions"][1]["line"],
        "CLICK button:\"Add to cart\""
    );
}

#[test]
fn a_goal_miss_fails_with_cache_only() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    let flow = goal_flow(&dir, "add two to the cart");
    let output = twin.run_with_args(&dir, &flow, &[], &["--cache=only"]);
    assert_eq!(exit_code(&output), 1, "{}", stdout_text(&output));
    assert_eq!(steps(&dir)[1]["error"]["code"], "cache-miss");
}

#[test]
fn a_goal_sends_secrets_as_placeholders_and_caches_their_reference() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[
        goal_act("e4", "fill", &["%env.QTY%"]),
        goal_act("e5", "click", &[]),
        goal_end("done", "added"),
    ]);
    let flow = goal_flow(&dir, "add {{env.QTY}} to the cart");
    let output = twin.run_with_args(&dir, &flow, &[("QTY", "2")], &["--cache=update"]);
    let log = twin.request_log();
    assert_eq!(exit_code(&output), 0, "{}{log}", stdout_text(&output));
    assert!(log.contains("Goal: add %env.QTY% to the cart"), "{log}");
    let cache = cache_of(&flow).expect("the cache is written");
    assert_eq!(
        cache["entries"][0]["actions"][0]["line"],
        "FILL textbox:Quantity \"{{env.QTY}}\""
    );
}

#[test]
fn a_goal_that_runs_out_of_time_fails_with_timeout() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[goal_act("e2", "click", &[])]);
    let flow = dir.file(
        "goal.whirl",
        "[Options]\nmodel: gpt-test\n\
         VISIT \"data:text/html,<button disabled>Buy</button>\"\n\
         GOAL \"buy it\" @1s\n\
         ASSERT url exists\n",
    );
    let output = twin.run(&dir, &flow, &[]);
    let log = twin.request_log();
    assert_eq!(exit_code(&output), 1, "{}{log}", stdout_text(&output));
    let error = &steps(&dir)[1]["error"];
    assert_eq!(error["code"], "timeout", "{error}");
    assert_eq!(
        error["message"],
        "timeout: GOAL did not finish within 1000ms"
    );
}

/// A page whose list arrives from a request that the server holds for
/// 800 ms, served on its own runtime.
struct SlowSite {
    _runtime: Runtime,
    url:      String,
}

impl SlowSite {
    fn start() -> Self {
        let runtime = Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("the site runtime should build");
        let listener = runtime
            .block_on(TcpListener::bind("127.0.0.1:0"))
            .expect("the site should bind a local port");
        let url = format!(
            "http://{}",
            listener
                .local_addr()
                .expect("a bound listener has an address")
        );
        let app = Router::new()
            .route(
                "/",
                get(|| async {
                    Html(
                        "<h1>Mugs</h1><ul id=list></ul><script>\
                         fetch('/items').then(r => r.text()).then(t => {\
                         document.getElementById('list').innerHTML = t; });</script>",
                    )
                }),
            )
            .route(
                "/items",
                get(|| async {
                    sleep(Duration::from_millis(800)).await;
                    "<li>Blue mug</li>"
                }),
            );
        runtime.spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("the site should serve");
        });
        Self {
            _runtime: runtime,
            url,
        }
    }
}

#[test]
fn a_model_reads_the_page_after_its_requests_finish() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    let site = SlowSite::start();
    twin.answer(&[json!({"value": "Blue mug"})]);
    let flow = dir.file(
        "slow.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\nVISIT {}/\n\
             EXTRACT item \"the first item\"\n\
             ASSERT extract:item == \"Blue mug\"\n",
            site.url
        ),
    );
    let output = twin.run(&dir, &flow, &[]);
    assert_eq!(exit_code(&output), 0, "{}", stdout_text(&output));
    let log = twin.request_log();
    assert!(log.contains("listitem [ref=e4]: Blue mug"), "{log}");
}

#[test]
fn a_goal_finds_a_renamed_element_again_and_replays_the_rest_of_its_path() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    let flow = goal_flow(&dir, "add two to the cart");
    fs::write(
        dir.path.join("goal.whirl-cache.json"),
        json!({
            "version": 1,
            "entries": [{
                "kind": "goal",
                "line": "GOAL \"add two to the cart\"",
                "occurrence": 1,
                "model": "gpt-test",
                "actions": [
                    {"line": "FILL textbox:Quantity \"2\"", "fingerprints": [{"role": "textbox", "name": "Quantity"}]},
                    {"line": "CLICK button:Buy", "fingerprints": [{"role": "button", "name": "Buy"}]}
                ]
            }]
        })
        .to_string(),
    )
    .expect("the cache writes");
    // One call finds the button again; one more confirms the goal is done.
    twin.answer(&[found(&["e5"]), goal_end("done", "the cart holds 2")]);
    let output = twin.run_with_args(&dir, &flow, &[], &["--cache=update"]);
    let log = twin.request_log();
    assert_eq!(exit_code(&output), 0, "{}{log}", stdout_text(&output));
    let goal = &steps(&dir)[1]["goal"];
    assert_eq!(goal["cache"], "healed");
    assert_eq!(goal["end"], "done");
    assert_eq!(goal["usage"]["modelCalls"], 2);
    assert_eq!(goal["actions"][1]["plannedBy"], "llm");
    assert_eq!(warning_codes(&dir), ["healed"]);
    assert!(log.contains("the button named \\\"Buy\\\""), "{log}");
    let cache = cache_of(&flow).expect("the cache is written");
    assert_eq!(
        cache["entries"][0]["actions"][1]["line"],
        "CLICK button:\"Add to cart\""
    );
}

#[test]
fn a_goal_fills_the_fields_of_a_form_in_one_answer() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[
        json!({
            "status": "act",
            "reason": "fill the form",
            "actions": [
                {"elementId": "e4", "description": "user", "method": "fill", "arguments": ["ada"]},
                {"elementId": "e6", "description": "password", "method": "fill", "arguments": ["%env.PASSWORD%"]}
            ]
        }),
        goal_end("done", "filled"),
    ]);
    let flow = dir.file(
        "form.whirl",
        "[Options]\nmodel: gpt-test\n\
         VISIT \"data:text/html,<h1>Sign in</h1><label>User <input></label><label>Password <input type=password></label>\"\n\
         GOAL \"sign in as ada with {{env.PASSWORD}}\"\n\
         ASSERT label:User value == ada\n\
         ASSERT label:Password value == {{env.PASSWORD}}\n",
    );
    let output = twin.run(&dir, &flow, &[("PASSWORD", "hunter2")]);
    let log = twin.request_log();
    assert_eq!(exit_code(&output), 0, "{}{log}", stdout_text(&output));
    let goal = &steps(&dir)[1]["goal"];
    assert_eq!(goal["usage"]["modelCalls"], 2);
    assert_eq!(goal["actions"].as_array().map(Vec::len), Some(2));
}

#[test]
fn consecutive_judge_lines_with_one_scope_share_one_call() {
    let dir = TestDir::new();
    let twin = ModelTwin::start();
    twin.answer(&[json!({"verdicts": [
        verdict("yes", "the total is right"),
        verdict("unsure", "the chart is cut off")
    ]})]);
    let flow = dir.file(
        "judge.whirl",
        &format!(
            "[Options]\nmodel: gpt-test\n{ORDER}\
             ASSERT testid:summary visible\n\
             JUDGE \"the total matches the line items\"\n\
             JUDGE \"the chart trends upward\"\n"
        ),
    );
    let output = twin.run(&dir, &flow, &[]);
    let log = twin.request_log();
    assert_eq!(exit_code(&output), 0, "{}{log}", stdout_text(&output));
    let steps = steps(&dir);
    assert_eq!(steps[2]["judge"]["usage"]["modelCalls"], 1);
    assert_eq!(steps[3]["judge"]["usage"]["modelCalls"], 0);
    assert_eq!(steps[3]["judge"]["verdict"], "unsure");
    assert_eq!(warning_codes(&dir), ["judge-unsure"]);
    assert!(
        log.contains("1. the total matches the line items 2. the chart trends upward"),
        "{log}"
    );
}
