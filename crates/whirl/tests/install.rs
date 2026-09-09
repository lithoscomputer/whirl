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

use std::path::{Path, PathBuf};
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

#[cfg(unix)]
fn executable(path: &Path, content: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    fs::create_dir_all(path.parent().expect("fixture parent")).expect("fixture directory");
    fs::write(path, content).expect("script fixture");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("executable fixture");
}

/// An already-provisioned bundle avoids network and records browser installer
/// arguments.
#[cfg(unix)]
fn installer_fixture(data_dir: &ScratchDir) {
    executable(
        &data_dir.path.join("bundle/node/bin/node"),
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo v24.19.0; else printf '%s\\n' \"$@\" > browser-args.txt; fi\n",
    );
    executable(
        &data_dir.path.join("bundle/bun/bun"),
        "#!/bin/sh\necho 1.4.0\n",
    );
    let package = data_dir
        .path
        .join("bundle/shim/node_modules/@playwright/test");
    fs::create_dir_all(&package).expect("package directory");
    fs::write(package.join("package.json"), r#"{"version":"1.62.1"}"#).expect("package fixture");
    fs::write(package.join("cli.js"), "").expect("CLI fixture");
}

#[test]
#[cfg(unix)]
fn install_chromium_provisions_only_the_selected_engine() {
    let data = ScratchDir::new("select-chromium");
    let cwd = ScratchDir::new("select-chromium-cwd");
    installer_fixture(&data);
    let output = run_whirl_with_bundle(&data, &cwd, &["install", "chromium"]);
    assert_eq!(exit_code(&output), 0, "{}", output_text(&output));
    let args = fs::read_to_string(data.path.join("bundle/shim/browser-args.txt"))
        .expect("installer arguments");
    assert!(args.ends_with("install\nchromium\n"), "{args}");
    assert!(!args.contains("firefox"));
    assert!(!args.contains("webkit"));
}

#[test]
#[cfg(unix)]
fn install_without_engines_preserves_all_browser_installation() {
    let data = ScratchDir::new("all-browsers");
    let cwd = ScratchDir::new("all-browsers-cwd");
    installer_fixture(&data);
    let output = run_whirl_with_bundle(&data, &cwd, &["install"]);
    assert_eq!(exit_code(&output), 0, "{}", output_text(&output));
    let args = fs::read_to_string(data.path.join("bundle/shim/browser-args.txt"))
        .expect("installer arguments");
    assert!(
        args.ends_with("install\nchromium\nfirefox\nwebkit\n"),
        "{args}"
    );
}

#[test]
#[cfg(unix)]
fn install_records_the_binary_version_and_doctor_rejects_another_versions_bundle() {
    let data = ScratchDir::new("version-marker");
    let cwd = ScratchDir::new("version-marker-cwd");
    installer_fixture(&data);
    let output = run_whirl_with_bundle(&data, &cwd, &["install", "chromium"]);
    assert_eq!(exit_code(&output), 0, "{}", output_text(&output));
    let marker = data.path.join("bundle/shim/whirl-version");
    let version = fs::read_to_string(&marker).expect("the version marker");
    assert_eq!(version.trim(), env!("CARGO_PKG_VERSION"));

    // An upgraded binary must not drive the shim an older one installed:
    // the run and the doctor both stop with the refresh command.
    fs::write(&marker, "0.1.0\n").expect("rewrite the marker");
    let doctor = run_whirl_with_bundle(&data, &cwd, &["doctor"]);
    assert_eq!(exit_code(&doctor), 3, "{}", output_text(&doctor));
    let text = output_text(&doctor);
    assert!(text.contains("from whirl 0.1.0"), "{text}");
    assert!(text.contains("whirl install"), "{text}");
    let flow = cwd.path.join("flow.whirl");
    fs::write(&flow, "VISIT \"data:text/html,<h1>Hi</h1>\"\n").expect("flow fixture");
    let run = run_whirl_with_bundle(&data, &cwd, &["flow.whirl"]);
    assert_eq!(exit_code(&run), 3, "{}", output_text(&run));
    assert!(output_text(&run).contains("whirl install"));
}

#[test]
fn doctor_reports_a_missing_bundle_without_installing_it() {
    let data = ScratchDir::new("doctor-missing");
    let cwd = ScratchDir::new("doctor-missing-cwd");
    let output = run_whirl_with_bundle(&data, &cwd, &["doctor"]);
    assert_eq!(exit_code(&output), 3);
    assert!(output_text(&output).contains("whirl install"));
    assert!(!data.path.join("bundle").exists());
}
