//! The JSON report (SPEC 14): the machine-readable superset, rendered
//! from the shared [`RunReport`] model in the plan doc's stable shape
//! (version 1). Every string in the model is already secret-masked.

use serde::Serialize;

use crate::report::model::{FileReport, RunReport};

/// The stable JSON report shape version.
const VERSION: u32 = 1;

/// The top-level JSON document: the run report under a `version` tag.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonReport<'a> {
    version:     u32,
    duration_ms: u64,
    files:       &'a [FileReport],
}

/// Renders the report as pretty-printed JSON with a trailing newline.
pub fn render(report: &RunReport) -> String {
    let document = JsonReport {
        version:     VERSION,
        duration_ms: report.duration_ms,
        files:       &report.files,
    };
    let mut text = serde_json::to_string_pretty(&document)
        .expect("the report model should always serialize to JSON");
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::report::fixture::{SECRET, sample_report};

    fn rendered() -> Value {
        let text = render(&sample_report());
        serde_json::from_str(&text).expect("the rendered report should parse as JSON")
    }

    #[test]
    fn the_document_has_the_stable_version_and_run_shape() {
        let document = rendered();
        assert_eq!(document["version"], json!(1));
        assert_eq!(document["durationMs"], json!(3_210));
        assert_eq!(
            document["files"]
                .as_array()
                .expect("files should be an array")
                .len(),
            4
        );
    }

    #[test]
    fn a_passed_file_carries_the_full_per_file_shape() {
        let document = rendered();
        assert_eq!(
            document["files"][0],
            json!({
                "path": "flows/pass.whirl",
                "status": "passed",
                "durationMs": 1_200,
                "artifactsDir": "whirl-artifacts/flows/pass",
                "blockedHosts": ["cdn.example.com"],
                "warnings": ["SCREENSHOT overview skipped: page crashed"],
                "artifacts": ["whirl-artifacts/flows/pass/video.webm"],
                "entries": [{
                    "name": "Log in.",
                    "line": 2,
                    "status": "passed",
                    "durationMs": 900,
                    "steps": [
                        {"line": 2, "kind": "action", "text": "VISIT /login",
                         "status": "passed", "durationMs": 5},
                        {"line": 3, "kind": "page", "text": "PAGE /dashboard",
                         "status": "passed", "durationMs": 5},
                    ],
                    "captures": {"next_url": "/dashboard"},
                    "artifacts": [],
                }],
            })
        );
    }

    #[test]
    fn a_failed_step_carries_its_error_and_later_entries_are_skipped() {
        let document = rendered();
        let failed = &document["files"][1];
        assert_eq!(failed["status"], json!("failed"));
        let step = &failed["entries"][0]["steps"][1];
        assert_eq!(
            step["error"],
            json!({
                "message": "assert: value mismatch",
                "expected": "expected",
                "actual": "***",
            })
        );
        assert_eq!(failed["entries"][1]["status"], json!("skipped"));
    }

    #[test]
    fn setup_failures_report_as_the_synthetic_entry() {
        let document = rendered();
        assert_eq!(document["files"][2]["entries"][0]["name"], json!("[setup]"));
        assert_eq!(
            document["files"][2]["entries"][0]["status"],
            json!("failed")
        );
        assert_eq!(document["files"][3]["entries"][0]["status"], json!("error"));
    }

    #[test]
    fn the_env_sourced_secret_never_appears() {
        let text = render(&sample_report());
        assert!(!text.contains(SECRET), "report:\n{text}");
        assert!(
            text.contains("FILL \\\"Password\\\" ***"),
            "report:\n{text}"
        );
    }
}
