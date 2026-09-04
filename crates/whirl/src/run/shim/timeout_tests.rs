//! Regressions for stalled lifecycle exchanges and blocked stdin writes.

use super::*;

fn step(script: &str) -> StepRequest {
    StepRequest {
        entry_start: false,
        command:     StepCommand::EvalAction {
            script: script.to_owned(),
        },
        timeout_ms:  100,
        title:       "timeout regression".to_owned(),
    }
}

async fn stalled_client(mode: &str) -> ShimClient {
    let launch = ShimLaunch {
        node:    PathBuf::from("node"),
        shim_js: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_shim.js"),
    };
    let mut client = ShimClient::spawn(&launch).expect("the fake shim starts");
    client.hello().await.expect("the fake shim is ready");
    let mut arm = step(mode);
    arm.timeout_ms = 5_000;
    assert!(matches!(client.run_step(&arm).await, StepOutcome::Ok(_)));
    client.lifecycle_timeout = Duration::from_millis(100);
    client.watchdog_grace = Duration::from_millis(100);
    client
}

#[tokio::test]
async fn unanswered_hello_kills_the_process() {
    let mut client = stalled_client("ignorelifecycle").await;
    let result = timeout(Duration::from_secs(5), client.hello())
        .await
        .expect("hello is bounded");
    assert!(matches!(
        result,
        Err(ShimError::TimedOut {
            command: "hello",
            ..
        })
    ));
    assert!(!client.is_alive());
    assert!(
        client
            .child
            .try_wait()
            .expect("child status is readable")
            .is_some()
    );
}

#[tokio::test]
async fn unanswered_start_flow_kills_the_process() {
    let mut client = stalled_client("ignorelifecycle").await;
    let params = StartFlowParams {
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
    let result = timeout(Duration::from_secs(5), client.start_flow(&params))
        .await
        .expect("startFlow is bounded");
    assert!(matches!(
        result,
        Err(ShimError::TimedOut {
            command: "startFlow",
            ..
        })
    ));
    assert!(!client.is_alive());
}

#[tokio::test]
async fn unanswered_end_flow_kills_the_process() {
    let mut client = stalled_client("ignorelifecycle").await;
    let params = EndFlowParams {
        save_storage_path: None,
        trace_path:        None,
    };
    let result = timeout(Duration::from_secs(5), client.end_flow(&params))
        .await
        .expect("endFlow is bounded");
    assert!(matches!(
        result,
        Err(ShimError::TimedOut {
            command: "endFlow",
            ..
        })
    ));
    assert!(!client.is_alive());
}

#[tokio::test]
async fn a_blocked_step_write_is_killed_without_reusing_the_partial_frame() {
    let mut client = stalled_client("stopreading").await;
    // Larger than the OS pipe buffer, regardless of whether Node read ahead.
    let request = step(&"x".repeat(8 * 1024 * 1024));
    let result = timeout(Duration::from_secs(5), client.run_step(&request))
        .await
        .expect("the write is bounded");
    assert!(matches!(result, StepOutcome::StepTimeout {
        process_killed: true,
    }));
    assert!(!client.is_alive());
    assert!(
        client
            .child
            .try_wait()
            .expect("child status is readable")
            .is_some()
    );
}

#[tokio::test]
async fn a_blocked_lifecycle_write_is_also_bounded() {
    let mut client = stalled_client("stopreading").await;
    let params = EndFlowParams {
        save_storage_path: Some("x".repeat(8 * 1024 * 1024)),
        trace_path:        None,
    };
    let result = timeout(Duration::from_secs(5), client.end_flow(&params))
        .await
        .expect("the lifecycle write is bounded");
    assert!(matches!(
        result,
        Err(ShimError::TimedOut {
            command: "endFlow",
            ..
        })
    ));
    assert!(!client.is_alive());
}

#[tokio::test]
async fn unanswered_shutdown_is_bounded() {
    let client = stalled_client("ignoreshutdown").await;
    let result = timeout(Duration::from_secs(10), client.shutdown())
        .await
        .expect("shutdown is bounded");
    assert!(matches!(result, Err(ShimError::ProcessDied { .. })));
}
