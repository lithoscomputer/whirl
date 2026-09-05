//! The versioned JSON report, shared by live and saved report generation.

use std::path::{Path, PathBuf};
use std::{env, fs};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use crate::report::metadata::ReportMetadata;
use crate::report::model::RunReport;

const VERSION: u32 = 1;

/// The producer's context stays attached when a saved report is rendered later.
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Document {
    version: u32,
    pub(crate) working_directory: PathBuf,
    pub(crate) whirl_version: String,
    pub(crate) platform: String,
    pub(crate) architecture: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) video_requested: Option<bool>,
    #[serde(flatten)]
    pub(crate) report: RunReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) metadata: Option<ReportMetadata>,
}

impl Document {
    pub(crate) fn new(report: RunReport, metadata: Option<ReportMetadata>, video: bool) -> Self {
        Self {
            version: VERSION,
            working_directory: env::current_dir().unwrap_or_default(),
            whirl_version: env!("CARGO_PKG_VERSION").to_owned(),
            platform: env::consts::OS.to_owned(),
            architecture: env::consts::ARCH.to_owned(),
            video_requested: Some(video),
            report,
            metadata,
        }
    }

    pub(crate) fn read(path: &Path) -> anyhow::Result<Self> {
        // Check the version before interpreting its result shape.
        #[derive(Deserialize)]
        struct Version {
            version: u32,
        }
        let source = fs::read_to_string(path)
            .with_context(|| format!("reading report '{}'", path.display()))?;
        let version: Version =
            serde_json::from_str(&source).context("invalid Whirl JSON report")?;
        anyhow::ensure!(
            version.version == VERSION,
            "unsupported report version {}",
            version.version
        );
        let document: Self = serde_json::from_str(&source).context("invalid Whirl JSON report")?;
        anyhow::ensure!(
            document.working_directory.is_absolute(),
            "report workingDirectory must be absolute"
        );
        for file in &document.report.files {
            if let Some(hash) = &file.source_sha256 {
                anyhow::ensure!(
                    hash.len() == 64
                        && hash
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                    "invalid sourceSha256 for '{}'",
                    file.path
                );
            }
            if let Some(roles) = file.roles {
                anyhow::ensure!(
                    roles.requested || roles.setup,
                    "flow '{}' has no role",
                    file.path
                );
            }
        }
        Ok(document)
    }

    /// Pretty-printed JSON with a trailing newline.
    pub(crate) fn render(&self) -> String {
        let mut text = serde_json::to_string_pretty(self)
            .expect("the report model should always serialize to JSON");
        text.push('\n');
        text
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::report::fixture::{SECRET, sample_report};

    fn rendered() -> Value {
        let text = Document::new(sample_report(), None, false).render();
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
                "code": "assert",
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
        let text = Document::new(sample_report(), None, false).render();
        assert!(!text.contains(SECRET), "report:\n{text}");
        assert!(
            text.contains("FILL \\\"Password\\\" ***"),
            "report:\n{text}"
        );
    }
}
