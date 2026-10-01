//! The shared run report model (plan doc "JSON report shape"). The
//! runner builds one [`RunReport`] per invocation; the console, JSON,
//! and JUnit reporters all render from it. Every string in the model is
//! already secret-masked by the runner (SPEC 11).

use std::fmt;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde::de::{Error as DeError, MapAccess, Visitor};
use serde::ser::SerializeMap as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;

/// UTC wall-clock boundaries. Durations are measured separately with Instant.
/// Absent fields belong to reports written before timestamps were recorded.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Timing {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at:  Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
}

impl Timing {
    pub fn start() -> Self {
        Self {
            started_at:  Some(SystemTime::now().into()),
            finished_at: None,
        }
    }

    pub fn finish(&mut self) {
        self.finished_at = Some(SystemTime::now().into());
    }
}

/// A requested flow can also supply setup state, while still running once.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FlowRoles {
    pub requested: bool,
    pub setup:     bool,
}

/// The name of the synthetic entry that reports a failure before the
/// first real entry: option resolution, storage loading, or browser
/// launch (SPEC 14).
pub const SETUP_ENTRY: &str = "[setup]";

/// Outcome of a step, an entry, or a file. Files never report
/// `Skipped`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StepKind {
    Action,
    Page,
    Assert,
    Judge,
    Capture,
}

/// A failed step's error detail.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StepError {
    pub code:       String,
    pub message:    String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected:   Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual:     Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidates: Option<Vec<String>>,
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
pub struct ReportViewport {
    pub width:  u64,
    pub height: u64,
}

/// The selected browser environment, reported only after a context starts.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeMetadata {
    pub browser:            String,
    pub viewport:           ReportViewport,
    pub user_agent:         Option<String>,
    pub browser_version:    Option<String>,
    pub node_version:       Option<String>,
    pub playwright_version: Option<String>,
    /// Frames per second of the file's recording; absent without `--video`
    /// and in reports written before the field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_fps:          Option<u64>,
}

/// One executed (or skipped) step.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
    /// What an `ACT` step asked and did (SPEC 7.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub act:         Option<ActReport>,
    /// What the step's `ai:` targets resolved to (SPEC 6.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai:          Option<AiReport>,
    /// What an `EXTRACT` step read (SPEC 7.6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extract:     Option<ExtractReport>,
    /// What a `JUDGE` step's model answered (SPEC 9.8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge:       Option<JudgeReport>,
    /// What a `GOAL` step ran (SPEC 7.7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal:        Option<GoalReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot:    Option<SnapshotReport>,
    /// Notices that do not fail the step, each with a stable code.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings:    Vec<StepWarning>,
}

/// A notice about a step that does not change its status.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StepWarning {
    /// A stable code, such as `unused-mock`.
    pub code:    String,
    /// Human-readable detail, secret-masked.
    pub message: String,
}

/// The rule that blocked a host (SPEC 5).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockedHostRule {
    pub host:   String,
    /// `allow-hosts` or `block-hosts`.
    pub option: String,
    /// The `block-hosts` glob that matched; none when no `allow-hosts`
    /// glob matched.
    pub glob:   Option<String>,
}

impl BlockedHostRule {
    /// The rule as the console and reports write it, such as
    /// `block-hosts *.analytics.example.com`.
    pub(crate) fn describe(&self) -> String {
        match &self.glob {
            Some(glob) => format!("{} {glob}", self.option),
            None => format!("not in {}", self.option),
        }
    }
}

/// Where a setting's value came from (SPEC 13).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SettingSource {
    Default,
    File,
    CommandLine,
}

/// A setting's value: text, a list, or none when unset.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum SettingValue {
    Text(String),
    List(Vec<String>),
}

/// One effective setting of a file (SPEC 14).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingReport {
    pub key:    String,
    /// The value the file ran with, masked; null when unset.
    pub value:  Option<SettingValue>,
    pub source: SettingSource,
    /// False for a setting that this engine validates but does not apply,
    /// such as `browsersim-origin`.
    pub active: bool,
}

impl SettingReport {
    /// The value as one line of text.
    pub(crate) fn value_text(&self) -> String {
        match &self.value {
            None => "none".to_owned(),
            Some(SettingValue::Text(text)) => text.clone(),
            Some(SettingValue::List(items)) => items.join(" "),
        }
    }
}

/// A mock a flow registered and how many requests it served (SPEC 7.5).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MockReport {
    pub line:   u32,
    pub method: String,
    /// The resolved URL pattern, secret-masked.
    pub url:    String,
    pub hits:   u64,
}

/// Effective snapshot options. Numeric text retains units and permits secret
/// masking.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotReport {
    /// The element target's locator text; absent for a full-page capture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target:          Option<String>,
    pub masks:           Vec<String>,
    pub max_diff:        String,
    pub pixel_threshold: String,
}

/// An `ACT` step's model, the actions it ran, and what the model calls
/// used. Arguments keep their `%name%` placeholders, so no secret appears.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActReport {
    pub model:   String,
    /// The planner that chose the actions: `llm`, or `jev` with `--jev`.
    #[serde(default = "llm_planner")]
    pub planner: String,
    pub actions: Vec<ActActionReport>,
    pub usage:   ActUsage,
    /// The line's AI cache status (SPEC 12.1): `hit`, `miss`, or `healed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache:   Option<String>,
    /// For a healed line, the lines the cache held.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached:  Option<Vec<String>>,
}

/// A step's `ai:` targets: what each resolved to, and what the model calls
/// used (SPEC 6.3).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiReport {
    pub model:   String,
    pub targets: Vec<AiTargetReport>,
    pub usage:   ActUsage,
}

/// One `ai:` target of a step.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiTargetReport {
    /// The authored locator, with its `ai:` segment; masked.
    pub target:      String,
    /// The locator of the element it resolved to, when it found one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locator:     Option<String>,
    /// The model's description of the element.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The AI cache status (SPEC 12.1): `hit`, `miss`, `healed`, or
    /// `uncached`.
    pub cache:       String,
    /// For a healed target, the locator the cache held.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached:      Option<String>,
}

/// A `JUDGE` step's verdict, the model's reason, and what the call used
/// (SPEC 9.8).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JudgeReport {
    pub model:   String,
    /// `yes`, `no`, or `unsure`; absent when the call failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
    /// The model's reason, masked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason:  Option<String>,
    pub usage:   ActUsage,
}

/// What an `EXTRACT` step read and what its model call used (SPEC 7.6).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtractReport {
    pub model: String,
    /// The value with its type, masked as a capture is; absent when the
    /// model found no value.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_capture",
        deserialize_with = "deserialize_optional_capture"
    )]
    pub value: Option<CaptureValue>,
    pub usage: ActUsage,
}

/// Writes a value as the `{type, value}` of a capture.
#[expect(
    clippy::ref_option,
    reason = "serde's serialize_with passes the field by reference"
)]
fn serialize_optional_capture<S: Serializer>(
    value: &Option<CaptureValue>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(value) => CaptureOut {
            value_type: &value.value_type,
            value:      &value.value,
        }
        .serialize(serializer),
        None => serializer.serialize_none(),
    }
}

/// Reads a `{type, value}` capture shape.
fn deserialize_optional_capture<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<CaptureValue>, D::Error> {
    let shape = Option::<serde_json::Value>::deserialize(deserializer)?;
    let Some(mut shape) = shape else {
        return Ok(None);
    };
    let value_type = shape
        .get("type")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| DeError::custom("a value needs a type"))?;
    let value = shape
        .get_mut("value")
        .map(serde_json::Value::take)
        .ok_or_else(|| DeError::custom("a value needs a value"))?;
    Ok(Some(CaptureValue::new(&value_type, value.to_string())))
}

/// Reports written before planners existed used the language model.
fn llm_planner() -> String {
    "llm".to_owned()
}

/// One action an `ACT` or `GOAL` step ran.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActActionReport {
    /// The action as a Whirl line, such as `CLICK button:"Sign in"`.
    pub line:        String,
    /// The model's description of the element.
    pub description: String,
    /// Which planner chose this action: `llm`, `jev`, or `cache`.
    #[serde(default = "llm_planner")]
    pub planned_by:  String,
    /// Why the action failed, for a `GOAL` action that the model planned
    /// past (SPEC 7.7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error:       Option<String>,
}

/// A `GOAL` step's model, the actions it ran, how it ended, and what the
/// model calls used (SPEC 7.7).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalReport {
    pub model:   String,
    pub actions: Vec<ActActionReport>,
    /// `done` or `impossible`; absent when the model did not say, as for
    /// a cache hit or a step that ran out of actions or time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end:     Option<String>,
    /// The model's reason for its last answer, masked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason:  Option<String>,
    pub usage:   ActUsage,
    /// The line's AI cache status (SPEC 12.1): `hit`, `miss`, or `healed`.
    pub cache:   String,
    /// For a healed line, the lines the cache held.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached:  Option<Vec<String>>,
}

/// Token usage summed over an `ACT` step's model calls.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActUsage {
    pub model_calls:     u32,
    /// Prompt tokens, cached or not.
    pub input_tokens:    u64,
    /// Completion tokens, reasoning included.
    pub output_tokens:   u64,
    /// Present only when every call was priced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd_micros: Option<u64>,
    /// Jev's requests, tokens, and cost; present only with `--jev`.
    /// `costUsdMicros` above includes Jev's cost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jev:             Option<ActJevUsage>,
}

/// What an `ACT` step's Jev requests used.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActJevUsage {
    pub requests:        u32,
    pub input_tokens:    u64,
    pub output_tokens:   u64,
    /// Present only when every answered request was priced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd_micros: Option<u64>,
}

/// One entry's result: its steps, captures, and artifacts.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
    /// JSON object of `{type, value}` (SPEC 14; report version 2).
    #[serde(
        serialize_with = "serialize_captures",
        deserialize_with = "deserialize_captures"
    )]
    pub captures:    Vec<(String, CaptureValue)>,
    /// Artifact paths recorded for this entry.
    pub artifacts:   Vec<String>,
}

/// One capture as the JSON report writes it: its type and its value. The
/// value keeps its exact JSON text, so a large integer survives a report
/// round trip.
#[derive(Clone, Debug)]
pub struct CaptureValue {
    /// The SPEC 9.3 type name, such as `string` or `number`.
    pub value_type: String,
    pub value:      Box<RawValue>,
}

impl CaptureValue {
    /// A capture with its type and its value's JSON text.
    pub fn new(value_type: &str, json: String) -> Self {
        Self {
            value_type: value_type.to_owned(),
            value:      RawValue::from_string(json)
                .expect("a capture's JSON text is always valid JSON"),
        }
    }

    /// A string capture.
    pub(crate) fn string(text: &str) -> Self {
        Self::new(
            "string",
            serde_json::to_string(text).expect("a string always serializes"),
        )
    }

    /// A masked capture: its type stays, and its value is `***` (SPEC 14).
    pub fn masked(value_type: &str, mask: &str) -> Self {
        Self::new(
            value_type,
            serde_json::to_string(mask).expect("a string always serializes"),
        )
    }

    /// The value as a reader sees it: a string's text, else its JSON.
    pub(crate) fn display(&self) -> String {
        serde_json::from_str::<String>(self.value.get())
            .unwrap_or_else(|_| self.value.get().to_owned())
    }
}

impl PartialEq for CaptureValue {
    fn eq(&self, other: &Self) -> bool {
        self.value_type == other.value_type && self.value.get() == other.value.get()
    }
}

impl Eq for CaptureValue {}

/// The version 2 capture shape, as written.
#[derive(Serialize)]
struct CaptureOut<'a> {
    #[serde(rename = "type")]
    value_type: &'a str,
    value:      &'a RawValue,
}

/// Serializes ordered `(name, value)` captures as a JSON object.
fn serialize_captures<S: Serializer>(
    captures: &[(String, CaptureValue)],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(Some(captures.len()))?;
    for (name, value) in captures {
        map.serialize_entry(name, &CaptureOut {
            value_type: &value.value_type,
            value:      &value.value,
        })?;
    }
    map.end()
}

/// Reads captures in capture order, including repeated names. A version
/// 1 report holds plain strings; version 2 holds `{type, value}`.
fn deserialize_captures<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<(String, CaptureValue)>, D::Error> {
    struct CapturesVisitor;
    impl<'de> Visitor<'de> for CapturesVisitor {
        type Value = Vec<(String, CaptureValue)>;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("an object of captured values")
        }
        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
            let mut captures = Vec::new();
            // The report deserializes through a buffered path, where a
            // RawValue cannot come back; integers within 64 bits stay exact.
            while let Some((name, value)) = map.next_entry::<String, serde_json::Value>()? {
                let capture = match value {
                    serde_json::Value::String(text) => CaptureValue::string(&text),
                    serde_json::Value::Object(mut shape) => {
                        let value_type = shape
                            .remove("type")
                            .and_then(|value_type| value_type.as_str().map(str::to_owned));
                        let value = shape.remove("value");
                        match (value_type, value) {
                            (Some(value_type), Some(value)) => {
                                CaptureValue::new(&value_type, value.to_string())
                            }
                            _ => {
                                return Err(DeError::custom("a capture needs a type and a value"));
                            }
                        }
                    }
                    _ => {
                        return Err(DeError::custom(
                            "a capture is a string or a {type, value} object",
                        ));
                    }
                };
                captures.push((name, capture));
            }
            Ok(captures)
        }
    }
    deserializer.deserialize_map(CapturesVisitor)
}

/// One file's result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileReport {
    #[serde(flatten)]
    pub timing:             Timing,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_sha256:      Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub roles:              Option<FlowRoles>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime:            Option<RuntimeMetadata>,
    /// The input path as given on the command line.
    pub path:               String,
    pub status:             Status,
    pub duration_ms:        u64,
    pub artifacts_dir:      String,
    pub blocked_hosts:      Vec<String>,
    /// The rule that blocked each host of `blocked_hosts` (SPEC 5). Older
    /// reports omit it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_host_rules: Vec<BlockedHostRule>,
    /// The settings the file ran with, masked (SPEC 14). A file whose
    /// options failed to resolve, and older reports, have none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub settings:           Vec<SettingReport>,
    /// Screenshot warnings (SPEC 7) and other non-failing notices.
    pub warnings:           Vec<String>,
    /// File-level artifact paths (`video.webm`, `network.har`).
    pub artifacts:          Vec<String>,
    /// Every `MOCK` that ran, in order (SPEC 7.5).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mocks:              Vec<MockReport>,
    pub entries:            Vec<EntryReport>,
}

impl FileReport {
    /// Each blocked host with the rule that blocked it, as the console and
    /// reports show it (SPEC 5).
    pub(crate) fn blocked_host_lines(&self) -> Vec<String> {
        self.blocked_hosts
            .iter()
            .map(|host| {
                match self
                    .blocked_host_rules
                    .iter()
                    .find(|rule| rule.host == *host)
                {
                    Some(rule) => format!("{host} ({})", rule.describe()),
                    None => host.clone(),
                }
            })
            .collect()
    }

    /// Each setting that this engine did not apply and that the file or
    /// the command line set, such as `browsersim-origin: recorded`
    /// (SPEC 5).
    pub(crate) fn inactive_setting_lines(&self) -> Vec<String> {
        self.settings
            .iter()
            .filter(|setting| !setting.active && setting.source != SettingSource::Default)
            .map(|setting| format!("{}: {}", setting.key, setting.value_text()))
            .collect()
    }

    /// Every warning to show a reader: the file's own, then each step's
    /// with its line and code.
    pub(crate) fn warning_lines(&self) -> Vec<String> {
        let steps = self.entries.iter().flat_map(|entry| &entry.steps);
        let step_warnings = steps.flat_map(|step| {
            step.warnings.iter().map(move |warning| {
                format!("line {}: {}: {}", step.line, warning.code, warning.message)
            })
        });
        self.warnings.iter().cloned().chain(step_warnings).collect()
    }
}

/// The whole run.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunReport {
    #[serde(flatten)]
    pub timing:      Timing,
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
