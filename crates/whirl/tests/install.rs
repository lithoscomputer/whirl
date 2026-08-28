//! End-to-end acceptance test for `whirl install` (CLI acceptance-tests
//! decision): provisions a real bundle — Node runtime download, shim
//! files, the pinned Bun binary, `bun install` of the dependencies, and
//! browser builds — into a scratch data directory (`WHIRL_DATA_DIR`),
//! then runs a `data:` URL flow that must resolve the shim from that
//! bundle because `WHIRL_SHIM_JS` is unset.
//!
//! The test is `#[ignore]`d: it needs network access, downloads hundreds
//! of megabytes, and takes minutes. Run it deliberately with
//! `cargo nextest run --run-ignored all -E 'test(installs_the_bundle)'`.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::{env, fs, process};

/// A scratch directory removed on drop.
struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    fn new(name: &str) -> Self {
        let path = env::temp_dir().join(format!("whirl-install-test-{}-{name}", process::id()));
        fs::create_dir_all(&path).expect("scratch dir should be creatable");
        Self { path }
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Runs the built `whirl` binary with the bundle as the only shim
/// source: `WHIRL_SHIM_JS`/`WHIRL_NODE` are removed and
/// `WHIRL_DATA_DIR` points at the scratch data directory.
fn run_whirl_with_bundle(data_dir: &ScratchDir, cwd: &ScratchDir, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_whirl"))
        .env_remove("WHIRL_SHIM_JS")
        .env_remove("WHIRL_NODE")
        .env("WHIRL_DATA_DIR", &data_dir.path)
        .current_dir(&cwd.path)
        .args(args)
        .output()
        .expect("the whirl binary should run")
}

fn exit_code(output: &Output) -> i32 {
    output.status.code().expect("whirl should exit, not signal")
}

fn output_text(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
#[ignore = "network + disk heavy: downloads the Node runtime, Bun, dependencies, and browsers"]
fn installs_the_bundle_and_runs_a_flow_from_it() {
    let data_dir = ScratchDir::new("data");
    let work_dir = ScratchDir::new("work");

    // First install provisions everything from scratch.
    let install = run_whirl_with_bundle(&data_dir, &work_dir, &["install"]);
    let text = output_text(&install);
    assert_eq!(exit_code(&install), 0, "{text}");
    assert!(
        data_dir.path.join("bundle/node/bin/node").is_file(),
        "the bundled node should exist; {text}"
    );
    assert!(
        data_dir.path.join("bundle/shim/index.js").is_file(),
        "the bundled shim entry should exist; {text}"
    );
    assert!(
        data_dir.path.join("bundle/bun/bun").is_file(),
        "the bundled bun should exist; {text}"
    );
    assert!(
        data_dir
            .path
            .join("bundle/shim/node_modules/@playwright/test/package.json")
            .is_file(),
        "the bundled @playwright/test should exist; {text}"
    );

    // A second install is idempotent and skips the completed steps.
    let again = run_whirl_with_bundle(&data_dir, &work_dir, &["install"]);
    let text = output_text(&again);
    assert_eq!(exit_code(&again), 0, "{text}");
    assert!(text.contains("Node v24.19.0: already installed"), "{text}");
    assert!(text.contains("Bun 1.4.0: already installed"), "{text}");
    assert!(
        text.contains("@playwright/test 1.62.1: already installed"),
        "{text}"
    );

    // A flow now runs with the bundle as the only shim source.
    let flow = work_dir.path.join("bundled.whirl");
    fs::write(
        &flow,
        "VISIT \"data:text/html,<h1>Bundled</h1>\"\n\
         [Asserts]\nrole:heading \"Bundled\" visible\n",
    )
    .expect("the flow file should be writable");
    let run = run_whirl_with_bundle(&data_dir, &work_dir, &[
        "--artifacts",
        "artifacts",
        flow.to_str().expect("utf-8 path"),
    ]);
    let text = output_text(&run);
    assert_eq!(exit_code(&run), 0, "{text}");
    assert!(text.contains("passed"), "{text}");
}
