//! The shared run report model (plan doc "JSON report shape"). The
//! runner builds one [`RunReport`] per invocation; the console, JSON,
//! and JUnit reporters all render from it. Every string in the model is
//! already secret-masked by the runner (SPEC 11).

use std::fmt;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeMap as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// UTC wall-clock boundaries. Durations are measured separately with Instant.
/// Absent fields belong to reports written before timestamps were recorded.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Timing {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) started_at:  Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) finished_at: Option<DateTime<Utc>>,
}

impl Timing {
    pub(crate) fn start() -> Self {
        Self {
            started_at:  Some(SystemTime::now().into()),
            finished_at: None,
        }
    }

    pub(crate) fn finish(&mut self) {
        self.finished_at = Some(SystemTime::now().into());
    }
}

/// A requested flow can also supply setup state, while still running once.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct FlowRoles {
    pub(crate) requested: bool,
    pub(crate) setup:     bool,
}

/// The name of the synthetic entry that reports a failure before the
/// first real entry: option resolution, storage loading, or browser
/// launch (SPEC 14).
pub(crate) const SETUP_ENTRY: &str = "[setup]";

/// Outcome of a step, an entry, or a file. Files never report
/// `Skipped`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Status {
    Passed,
    Failed,
    /// A runtime error (SPEC 13, exit 3): shim or browser failure,
    /// missing snapshot baseline, or an internal shim error.
    Error,
    Skipped,
}

/// What kind of SPEC line a step is.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum StepKind {
    Action,
    Page,
    Assert,
    Capture,
}

/// A failed step's error detail.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StepError {
    pub(crate) code:       String,
    pub(crate) message:    String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) expected:   Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) actual:     Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) candidates: Option<Vec<String>>,
}

impl Default for StepError {
    fn default() -> Self {
        Self {
            code:       "internal".to_owned(),
            message:    String::new(),
            expected:   None,
            actual:     None,
            candidates: None,
        }
    }
}

/// The viewport recorded in a report, in CSS pixels.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ReportViewport {
    pub(crate) width:  u64,
    pub(crate) height: u64,
}

/// The selected browser environment, reported only after a context starts.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RuntimeMetadata {
    pub(crate) browser:            String,
    pub(crate) viewport:           ReportViewport,
    pub(crate) user_agent:         Option<String>,
    pub(crate) browser_version:    Option<String>,
    pub(crate) node_version:       Option<String>,
    pub(crate) playwright_version: Option<String>,
    /// Frames per second of the file's recording; absent without `--video`
    /// and in reports written before the field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) video_fps:          Option<u64>,
}

/// One executed (or skipped) step.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StepReport {
    pub(crate) line:        u32,
    pub(crate) kind:        StepKind,
    /// The rendered, secret-masked step text.
    pub(crate) text:        String,
    pub(crate) status:      Status,
    pub(crate) duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error:       Option<StepError>,
}

/// One entry's result: its steps, captures, and artifacts.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EntryReport {
    /// The display name (SPEC 14): nearest comment above the entry, or
    /// its first action plus line number; `[setup]` for the synthetic
    /// pre-entry failure.
    pub(crate) name:        String,
    pub(crate) line:        u32,
    pub(crate) status:      Status,
    pub(crate) duration_ms: u64,
    pub(crate) steps:       Vec<StepReport>,
    /// Captured variables, masked, in capture order. Serialized as a
    /// JSON object (plan doc "JSON report shape").
    #[serde(
        serialize_with = "serialize_captures",
        deserialize_with = "deserialize_captures"
    )]
    pub(crate) captures:    Vec<(String, String)>,
    /// Artifact paths recorded for this entry.
    pub(crate) artifacts:   Vec<String>,
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

/// Preserve capture order, including repeated names in reports from older runs.
fn deserialize_captures<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<(String, String)>, D::Error> {
    struct CapturesVisitor;
    impl<'de> Visitor<'de> for CapturesVisitor {
        type Value = Vec<(String, String)>;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("an object of captured string values")
        }
        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
            let mut captures = Vec::new();
            while let Some(capture) = map.next_entry()? {
                captures.push(capture);
            }
            Ok(captures)
        }
    }
    deserializer.deserialize_map(CapturesVisitor)
}

/// One file's result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FileReport {
    #[serde(flatten)]
    pub(crate) timing:        Timing,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) roles:         Option<FlowRoles>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) runtime:       Option<RuntimeMetadata>,
    /// The input path as given on the command line.
    pub(crate) path:          String,
    pub(crate) status:        Status,
    pub(crate) duration_ms:   u64,
    pub(crate) artifacts_dir: String,
    pub(crate) blocked_hosts: Vec<String>,
    /// Screenshot warnings (SPEC 7) and other non-failing notices.
    pub(crate) warnings:      Vec<String>,
    /// File-level artifact paths (`video.webm`, `network.har`).
    pub(crate) artifacts:     Vec<String>,
    pub(crate) entries:       Vec<EntryReport>,
}

/// The whole run.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RunReport {
    #[serde(flatten)]
    pub(crate) timing:      Timing,
    pub(crate) duration_ms: u64,
    pub(crate) files:       Vec<FileReport>,
}

impl RunReport {
    /// True when any file hit a runtime error (SPEC 13, exit 3).
    pub(crate) fn has_error(&self) -> bool {
        self.files.iter().any(|file| file.status == Status::Error)
    }

    /// True when any file failed (SPEC 13, exit 1).
    pub(crate) fn has_failure(&self) -> bool {
        self.files.iter().any(|file| file.status == Status::Failed)
    }
}
