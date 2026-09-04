//! Shim client framing tests against the fake shim in
//! `tests/fixtures/fake_shim.js`: request/response dispatch, typed
//! lifecycle results, step outcomes, the external watchdog's cancel and
//! kill paths, and clean shutdown. The fake shim needs only `node` on
//! `PATH`; no browser is involved.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;
use tokio::time::sleep;
use whirl::run::shim::{
    CaptureResult, EndFlowParams, ShimClient, ShimLaunch, StartFlowParams, StepCommand,
    StepOutcome, StepRequest, ViewportParams,
};

/// Launch parameters for the fake shim: `node` from `PATH` and the
/// fixture next to this test.
fn fake_shim_launch() -> ShimLaunch {
    let shim_js = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake_shim.js");
    ShimLaunch {
        node: PathBuf::from("node"),
        shim_js,
    }
}

/// A spawned fake-shim client with a short watchdog grace so timeout
/// tests stay fast.
fn spawn_fake_shim() -> ShimClient {
    let mut client = ShimClient::spawn(&fake_shim_launch()).expect("the fake shim should spawn");
    client.set_watchdog_grace(Duration::from_millis(200));
    client
}

/// An `evalAction` step whose script directs the fake shim's response.
fn eval_step(script: &str, timeout_ms: u64) -> StepRequest {
    StepRequest {
        entry_start: false,
        command: StepCommand::EvalAction {
            script: script.to_owned(),
        },
        timeout_ms,
        title: format!("EVAL \"{script}\""),
    }
}

#[tokio::test]
async fn hello_start_flow_and_end_flow_round_trip() {
    let mut client = spawn_fake_shim();
    let hello = client.hello().await.expect("hello should succeed");
    assert_eq!(hello.protocol, 1);
    assert_eq!(hello.playwright_version, "0.0.0-fake");

    let start = StartFlowParams {
        browser:            "chromium".to_owned(),
        headed:             false,
        viewport:           ViewportParams {
            width:  1280,
            height: 720,
        },
        storage_state_path: None,
        dialogs:            "dismiss".to_owned(),
        allow_hosts:        None,
        nav_timeout_ms:     30_000,
        user_agent:         None,
        reduced_motion:     None,
        video:              None,
        har_path:           None,
        trace:              false,
    };
    client
        .start_flow(&start)
        .await
        .expect("startFlow should succeed");

    let end = client
        .end_flow(&EndFlowParams {
            save_storage_path: None,
            trace_path:        None,
        })
        .await
        .expect("endFlow should succeed");
    assert_eq!(end.blocked_hosts, vec!["a.example", "b.example"]);
    assert_eq!(end.video_path, None);

    client.shutdown().await.expect("shutdown should be clean");
}

#[tokio::test]
async fn a_step_ok_reply_is_returned_as_its_result() {
    let mut client = spawn_fake_shim();
    let outcome = client.run_step(&eval_step("ok", 1_000)).await;
    let StepOutcome::Ok(result) = outcome else {
        panic!("expected Ok, got {outcome:?}");
    };
    assert_eq!(result, json!({}));
    client.shutdown().await.expect("shutdown should be clean");
}

#[tokio::test]
async fn a_step_error_reply_carries_the_protocol_error_object() {
    let mut client = spawn_fake_shim();
    let outcome = client.run_step(&eval_step("error", 1_000)).await;
    let StepOutcome::ShimError(error) = outcome else {
        panic!("expected ShimError, got {outcome:?}");
    };
    assert_eq!(error.kind, "assert");
    assert_eq!(error.message, "fake assertion failed");
    assert_eq!(error.expected.as_deref(), Some("a"));
    assert_eq!(error.actual.as_deref(), Some("b"));
    client.shutdown().await.expect("shutdown should be clean");
}

#[tokio::test]
async fn step_params_carry_the_common_timeout_and_title() {
    // Unknown commands echo their cmd and params back, so the framing
    // of a full step request is observable.
    let mut client = spawn_fake_shim();
    let locator = json!([{"type": "text", "text": "Add to cart", "exact": true}]);
    let step = StepRequest {
        entry_start: false,
        command:     StepCommand::Click {
            locator: locator.clone(),
        },
        timeout_ms:  1_000,
        title:       "CLICK text:\"Add to cart\"".to_owned(),
    };
    let outcome = client.run_step(&step).await;
    let StepOutcome::Ok(result) = outcome else {
        panic!("expected Ok, got {outcome:?}");
    };
    assert_eq!(
        result,
        json!({
            "cmd": "click",
            "params": {
                "locator": locator,
                "timeoutMs": 1_000,
                "title": "CLICK text:\"Add to cart\"",
            },
        })
    );
    client.shutdown().await.expect("shutdown should be clean");
}

#[tokio::test]
async fn the_watchdog_cancels_an_unanswered_step_and_the_client_stays_usable() {
    // The "never" step is never answered directly: the watchdog sends
    // cancelFlow (a later id), whose reply arrives before the step's
    // cancelled-error reply — responses out of request order, matched
    // by id.
    let mut client = spawn_fake_shim();
    let outcome = client.run_step(&eval_step("never", 100)).await;
    let StepOutcome::StepTimeout { process_killed } = outcome else {
        panic!("expected StepTimeout, got {outcome:?}");
    };
    assert!(!process_killed, "cancelFlow was answered, so no kill");
    assert!(client.is_alive());

    // The same process serves the next step.
    let outcome = client.run_step(&eval_step("ok", 1_000)).await;
    assert!(
        matches!(outcome, StepOutcome::Ok(_)),
        "expected Ok after cancel, got {outcome:?}"
    );
    client.shutdown().await.expect("shutdown should be clean");
}

#[tokio::test]
async fn an_unresponsive_shim_is_killed_and_reported_dead() {
    let mut client = spawn_fake_shim();
    let outcome = client.run_step(&eval_step("ignorecancel", 1_000)).await;
    assert!(
        matches!(outcome, StepOutcome::Ok(_)),
        "expected Ok arming ignore-cancel, got {outcome:?}"
    );

    let outcome = client.run_step(&eval_step("never", 100)).await;
    let StepOutcome::StepTimeout { process_killed } = outcome else {
        panic!("expected StepTimeout, got {outcome:?}");
    };
    assert!(
        process_killed,
        "cancelFlow was ignored, so the kill path ran"
    );
    assert!(!client.is_alive(), "a killed client reports dead");
}

#[tokio::test]
async fn a_dying_process_reports_process_died_with_the_stderr_tail() {
    let mut client = spawn_fake_shim();
    let outcome = client
        .run_step(&eval_step("stderr:boom from the shim", 1_000))
        .await;
    assert!(
        matches!(outcome, StepOutcome::Ok(_)),
        "expected Ok from the stderr step, got {outcome:?}"
    );

    let outcome = client.run_step(&eval_step("die", 1_000)).await;
    assert!(
        matches!(outcome, StepOutcome::ProcessDied { .. }),
        "expected ProcessDied, got {outcome:?}"
    );
    assert!(!client.is_alive(), "a dead process reports dead");

    // The stderr capture task races the death observation; poll briefly.
    let mut tail = client.stderr_tail();
    for _ in 0..50 {
        if tail.contains("boom from the shim") {
            break;
        }
        sleep(Duration::from_millis(20)).await;
        tail = client.stderr_tail();
    }
    assert!(
        tail.contains("boom from the shim"),
        "stderr tail should hold the shim's diagnostics, got: {tail}"
    );
}

#[tokio::test]
async fn requests_after_a_death_fail_without_hanging() {
    let mut client = spawn_fake_shim();
    let outcome = client.run_step(&eval_step("die", 1_000)).await;
    assert!(
        matches!(outcome, StepOutcome::ProcessDied { .. }),
        "expected ProcessDied, got {outcome:?}"
    );
    let outcome = client.run_step(&eval_step("ok", 1_000)).await;
    assert!(
        matches!(outcome, StepOutcome::ProcessDied { .. }),
        "expected ProcessDied on a dead client, got {outcome:?}"
    );
}

#[tokio::test]
async fn a_capture_result_deserializes_to_its_typed_form() {
    let mut client = spawn_fake_shim();
    let step = StepRequest {
        entry_start: false,
        command:     StepCommand::Capture {
            source: json!({"type": "url"}),
            filter: json!(null),
        },
        timeout_ms:  1_000,
        title:       "cart_url: url".to_owned(),
    };
    let outcome = client.run_step(&step).await;
    let StepOutcome::Ok(result) = outcome else {
        panic!("expected Ok, got {outcome:?}");
    };
    let capture: CaptureResult =
        serde_json::from_value(result).expect("a capture result should deserialize");
    assert_eq!(capture.value, "captured");
    client.shutdown().await.expect("shutdown should be clean");
}
