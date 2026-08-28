//! The shared run report model (plan doc "JSON report shape"). The
//! runner builds one [`RunReport`] per invocation; the console, JSON,
//! and JUnit reporters all render from it. Every string in the model is
//! already secret-masked by the runner (SPEC 11).

use serde::ser::SerializeMap as _;
use serde::{Serialize, Serializer};

/// The name of the synthetic entry that reports a failure before the
/// first real entry: option resolution, storage loading, or browser
/// launch (SPEC 14).
pub const SETUP_ENTRY: &str = "[setup]";

/// Outcome of a step, an entry, or a file. Files never report
/// `Skipped`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Passed,
    Failed,
    /// A runtime error (SPEC 13, exit 3): shim or browser failure,
    /// missing snapshot baseline, or an internal shim error.
    Error,
    Skipped,
}

/// What kind of SPEC line a step is.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StepKind {
    Action,
    Page,
    Assert,
    Capture,
}

/// A failed step's error detail.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StepError {
    pub message:    String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected:   Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual:     Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidates: Option<Vec<String>>,
}

/// One executed (or skipped) step.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StepReport {
    pub line:        u32,
    pub kind:        StepKind,
    /// The rendered, secret-masked step text.
    pub text:        String,
    pub status:      Status,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error:       Option<StepError>,
}

/// One entry's result: its steps, captures, and artifacts.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryReport {
    /// The display name (SPEC 14): nearest comment above the entry, or
    /// its first action plus line number; `[setup]` for the synthetic
    /// pre-entry failure.
    pub name:        String,
    pub line:        u32,
    pub status:      Status,
    pub duration_ms: u64,
    pub steps:       Vec<StepReport>,
    /// Captured variables, masked, in capture order. Serialized as a
    /// JSON object (plan doc "JSON report shape").
    #[serde(serialize_with = "serialize_captures")]
    pub captures:    Vec<(String, String)>,
    /// Artifact paths recorded for this entry.
    pub artifacts:   Vec<String>,
}

/// Serializes ordered `(name, value)` captures as a JSON object.
fn serialize_captures<S: Serializer>(
    captures: &[(String, String)],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(Some(captures.len()))?;
    for (name, value) in captures {
        map.serialize_entry(name, value)?;
    }
    map.end()
}

/// One file's result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileReport {
    /// The input path as given on the command line.
    pub path:          String,
    pub status:        Status,
    pub duration_ms:   u64,
    pub artifacts_dir: String,
    pub blocked_hosts: Vec<String>,
    /// Screenshot warnings (SPEC 7) and other non-failing notices.
    pub warnings:      Vec<String>,
    /// File-level artifact paths (`video.webm`, `network.har`).
    pub artifacts:     Vec<String>,
    pub entries:       Vec<EntryReport>,
}

/// The whole run.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunReport {
    pub duration_ms: u64,
    pub files:       Vec<FileReport>,
}

impl RunReport {
    /// True when any file hit a runtime error (SPEC 13, exit 3).
    pub fn has_error(&self) -> bool {
        self.files.iter().any(|file| file.status == Status::Error)
    }

    /// True when any file failed (SPEC 13, exit 1).
    pub fn has_failure(&self) -> bool {
        self.files.iter().any(|file| file.status == Status::Failed)
    }
}
