//! Saved reporting is tested through the CLI, without an available runtime.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::TempDir;

fn render(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_whirl"))
        .current_dir(dir)
        .env("WHIRL_NODE", "/missing/whirl-test-node")
        .env("WHIRL_SHIM_JS", "/missing/whirl-test-shim")
        .arg("report")
        .args(args)
        .output()
        .expect("report command should start")
}

fn legacy_report(dir: &Path) -> Value {
    json!({
        "version": 1, "workingDirectory": dir,
        "whirlVersion": "0.8.0", "platform": "original-os", "architecture": "original-arch",
        "durationMs": 1234,
        "metadata": {"title": "Original context", "files": {"gone.whirl": {"title": "Original flow"}}},
        "files": [{
            "path": "gone.whirl", "status": "failed", "durationMs": 1200,
            "artifactsDir": "artifacts/gone", "blockedHosts": [], "warnings": [], "artifacts": [],
            "entries": [{"name": "A check", "line": 1, "status": "failed", "durationMs": 1200,
                "steps": [{"line": 1, "kind": "assert", "text": "title == Wanted", "status": "failed", "durationMs": 1200,
                    "error": {"code": "assert", "message": "Mismatch", "expected": "Wanted", "actual": "***"}}],
                "captures": {"token": "***"}, "artifacts": []}]
        }]
    })
}

fn save(dir: &Path, report: &Value) -> String {
    let source = serde_json::to_string_pretty(report).expect("JSON");
    fs::write(dir.join("report.json"), &source).expect("save report");
    source
}

#[test]
fn saved_report_keeps_original_context_and_results_without_sources_or_runtime() {
    let dir = TempDir::new().expect("temp directory");
    let mut report = legacy_report(&dir.path().join("original-directory-no-longer-exists"));
    report["futureField"] = json!({"ignored": true});
    report["files"][0]["futureField"] = json!(true);
    report["metadata"]["futureField"] = json!(true);
    report["metadata"]["files"]["gone.whirl"]["futureField"] = json!(true);
    let source = save(dir.path(), &report);
    let output = render(dir.path(), &["report.json", "--html", "evidence.html"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let html = fs::read_to_string(dir.path().join("evidence.html")).expect("HTML");
    for expected in [
        "Whirl 0.8.0 · original-os / original-arch",
        "Original context",
        "Original flow",
        "Wanted",
        "***",
        "Not recorded",
        "Role not recorded",
        "recording settings were not recorded",
    ] {
        assert!(html.contains(expected), "missing {expected}");
    }
    assert!(html.contains("data-status=\"failed\""));
    assert_eq!(
        fs::read_to_string(dir.path().join("report.json")).expect("unchanged JSON"),
        source
    );

    fs::write(dir.path().join("context.json"), r#"{"title":"Revised <scope>","files":{"gone.whirl":{"description":"New description {{env.NEVER_READ}}"}}}"#).expect("metadata");
    let output = render(dir.path(), &[
        "report.json",
        "--html",
        "evidence.html",
        "--metadata",
        "context.json",
    ]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let html = fs::read_to_string(dir.path().join("evidence.html")).expect("HTML");
    assert!(html.contains("Revised &lt;scope&gt;"));
    assert!(html.contains("New description {{env.NEVER_READ}}"));
    assert!(!html.contains("Original context"));
    assert!(html.contains("data-status=\"failed\""));
    assert_eq!(
        fs::read_to_string(dir.path().join("report.json")).expect("unchanged JSON"),
        source
    );
}

#[test]
fn saved_report_rejects_malformed_data_without_replacing_the_destination() {
    let dir = TempDir::new().expect("temp directory");
    let original = legacy_report(dir.path());
    let mut cases = Vec::new();
    for (pointer, replacement) in [
        ("/version", json!(2)),
        ("/workingDirectory", json!("relative")),
        ("/startedAt", json!("yesterday")),
        ("/files/0/status", json!("maybe")),
        ("/files/0/sourceSha256", json!("abcdef")),
        (
            "/files/0/roles",
            json!({"requested": false, "setup": false}),
        ),
        ("/files/0/entries/0/captures", json!({"token": 3})),
    ] {
        let mut value = original.clone();
        // Insert optional fields as well as replacing required fields.
        let (parent, field) = pointer.rsplit_once('/').expect("JSON pointer");
        value
            .pointer_mut(parent)
            .expect("parent")
            .as_object_mut()
            .expect("object")
            .insert(field.to_owned(), replacement);
        cases.push(value);
    }
    fs::write(dir.path().join("evidence.html"), "previous report").expect("destination");
    for report in cases {
        save(dir.path(), &report);
        let output = render(dir.path(), &["report.json", "--html", "evidence.html"]);
        assert_eq!(output.status.code(), Some(4), "{output:?}");
        assert_eq!(
            fs::read_to_string(dir.path().join("evidence.html")).expect("destination"),
            "previous report"
        );
    }
}

#[test]
fn saved_report_protects_inputs_and_reports_output_errors() {
    let dir = TempDir::new().expect("temp directory");
    let report = legacy_report(dir.path());
    let source = save(dir.path(), &report);
    fs::write(dir.path().join("gone.whirl"), "source flow").expect("flow");
    fs::write(dir.path().join("context.json"), "{}").expect("metadata");
    for destination in ["report.json", "gone.whirl", "context.json"] {
        let output = render(dir.path(), &[
            "report.json",
            "--html",
            destination,
            "--metadata",
            "context.json",
        ]);
        assert_eq!(output.status.code(), Some(4), "{output:?}");
    }
    assert_eq!(
        fs::read_to_string(dir.path().join("report.json")).expect("JSON"),
        source
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("gone.whirl")).expect("flow"),
        "source flow"
    );
    let output = render(dir.path(), &[
        "report.json",
        "--html",
        "missing/evidence.html",
    ]);
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let output = render(dir.path(), &[
        "report.json",
        "--html",
        "evidence.html",
        "--working-directory",
        "report.json",
    ]);
    assert_eq!(output.status.code(), Some(4), "{output:?}");
}

#[test]
fn saved_report_can_relocate_relative_media_and_preserves_missing_media_status() {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;

    let dir = TempDir::new().expect("temp directory");
    let bundle = dir.path().join("bundle");
    fs::create_dir_all(bundle.join("artifacts/gone")).expect("media directory");
    let png = STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRZkAAAAASUVORK5CYII=").expect("PNG bytes");
    fs::write(bundle.join("artifacts/gone/page.png"), &png).expect("media");
    let mut report = legacy_report(&dir.path().join("old"));
    report["files"][0]["entries"][0]["artifacts"] = json!(["artifacts/gone/page.png"]);
    report["videoRequested"] = json!(true);
    save(dir.path(), &report);
    let output = render(dir.path(), &[
        "report.json",
        "--html",
        "evidence.html",
        "--working-directory",
        "bundle",
    ]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let html = fs::read_to_string(dir.path().join("evidence.html")).expect("HTML");
    assert!(html.contains(&format!("data:image/png;base64,{}", STANDARD.encode(&png))));
    assert!(html.contains("Recording unavailable."));
    let output = render(dir.path(), &[
        "report.json",
        "--html",
        "bundle/artifacts/gone/page.png",
        "--working-directory",
        "bundle",
    ]);
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    assert_eq!(
        fs::read(bundle.join("artifacts/gone/page.png")).expect("preserved media"),
        png
    );
    fs::write(bundle.join("gone.whirl"), "copied source").expect("copied flow");
    let output = render(dir.path(), &[
        "report.json",
        "--html",
        "bundle/gone.whirl",
        "--working-directory",
        "bundle",
    ]);
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    assert_eq!(
        fs::read_to_string(bundle.join("gone.whirl")).expect("preserved source"),
        "copied source"
    );
    fs::remove_dir_all(&bundle).expect("remove media");
    let output = render(dir.path(), &["report.json", "--html", "evidence.html"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let html = fs::read_to_string(dir.path().join("evidence.html")).expect("HTML");
    assert!(html.contains("Screenshot unavailable"));
    assert!(html.contains("data-status=\"failed\""));
}

#[test]
fn source_hash_uses_parsed_bytes_even_when_the_file_changes_during_execution() {
    let dir = TempDir::new().expect("temp directory");
    let original = "# Original\r\nVISIT https://example.test/\r\nSCREENSHOT evidence\r\n";
    fs::write(dir.path().join("mutable.whirl"), original).expect("flow");
    let shim = format!(
        "require('node:fs').writeFileSync('mutable.whirl', 'changed after parsing');\n{}",
        include_str!("fixtures/fake_shim.js")
    );
    fs::write(dir.path().join("shim.cjs"), shim).expect("test shim");
    let output = Command::new(env!("CARGO_BIN_EXE_whirl"))
        .current_dir(dir.path())
        .env("WHIRL_NODE", "node")
        .env("WHIRL_SHIM_JS", dir.path().join("shim.cjs"))
        .args(["--report-json", "report.json", "mutable.whirl"])
        .output()
        .expect("whirl command");
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let report: Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join("report.json")).expect("report"))
            .expect("JSON");
    assert_eq!(
        fs::read_to_string(dir.path().join("mutable.whirl")).expect("changed source"),
        "changed after parsing"
    );
    assert_eq!(
        report["files"][0]["sourceSha256"],
        "fc8f7285afc39b1cd65232d769a331cd83df8f08576c270eec3fc850bdd85940"
    );
    assert_eq!(
        report["files"][0]["roles"],
        json!({"requested": true, "setup": false})
    );
    assert!(report["files"][0]["startedAt"].is_string());
    assert!(report["files"][0]["finishedAt"].is_string());
}

#[cfg(unix)]
#[test]
fn saved_report_rejects_symlink_escapes_and_protects_symlinked_inputs() {
    use std::os::unix::fs::symlink;

    let dir = TempDir::new().expect("temp directory");
    fs::create_dir_all(dir.path().join("artifacts/gone")).expect("flow directory");
    fs::write(
        dir.path().join("private.png"),
        b"\x89PNG\r\n\x1a\nprivate pixels",
    )
    .expect("private media");
    symlink(
        dir.path().join("private.png"),
        dir.path().join("artifacts/gone/escape.png"),
    )
    .expect("symlink");
    let mut report = legacy_report(dir.path());
    report["files"][0]["entries"][0]["artifacts"] = json!(["artifacts/gone/escape.png"]);
    let source = save(dir.path(), &report);
    symlink(
        dir.path().join("report.json"),
        dir.path().join("alias.html"),
    )
    .expect("symlink");
    let output = render(dir.path(), &["report.json", "--html", "alias.html"]);
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    assert_eq!(
        fs::read_to_string(dir.path().join("report.json")).expect("report"),
        source
    );
    let output = render(dir.path(), &["report.json", "--html", "evidence.html"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let html = fs::read_to_string(dir.path().join("evidence.html")).expect("HTML");
    assert!(html.contains("artifact is outside its flow directory"));
    assert!(!html.contains("data:image/png;base64,"));
}
