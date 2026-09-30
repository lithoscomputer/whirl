//! Execution of one parsed flow through one shim process: option
//! resolution (SPEC 5, 11), entry and step execution with timeout
//! budgeting (SPEC 12), failure artifacts, and the per-file report.

use std::collections::HashMap;
use std::path::{self, Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};
use tokio::fs;
use tracing::{Instrument as _, debug, debug_span, info_span};

use crate::check;
use crate::lang::ast::{
    self, BrowserKind, BrowserSimOrigin, DialogPolicy, DurationLit, File, FileOption, OptionSource,
    OptionValue, ReducedMotion, Value, Viewport,
};
use crate::lang::fmt::render_snapshot_target;
use crate::report::model::{
    ActReport, AiReport, BlockedHostRule, CaptureValue, EntryReport, ExtractReport, FileReport,
    GoalReport, JudgeReport, MockReport, ReportViewport, RuntimeMetadata, SETUP_ENTRY,
    SnapshotReport, Status, StepError, StepKind, StepReport, StepWarning, Timing,
};
use crate::run::act::{ActPlanner, Instruction, JudgeAnswer, ModelClient};
use crate::run::artifacts;
use crate::run::cache::{self, CacheMode};
use crate::run::shim::{
    EndFlowParams, ErrorObject, MockHits, ShimClient, ShimError, StartFlowParams, StepCommand,
    StepOutcome, StepRequest, VideoParams, ViewportParams, wire,
};
use crate::run::vars::{VarError, VarStore};

mod act_step;
mod ai_step;
mod check_step;
mod extract_step;
mod goal_step;
mod judge_step;
mod settings;
mod snapshot;
use snapshot::SnapshotSettings;

/// Default per-step timeout (SPEC 5).
pub(crate) const DEFAULT_STEP_TIMEOUT_MS: u64 = 10_000;
/// Default navigation timeout for `VISIT` (SPEC 5).
pub(crate) const DEFAULT_NAV_TIMEOUT_MS: u64 = 30_000;
/// Default viewport (SPEC 5).
pub(crate) const DEFAULT_VIEWPORT: Viewport = Viewport {
    width:  1280,
    height: 720,
};
/// Budget for the best-effort failure screenshot (SPEC 12).
const FAILURE_SCREENSHOT_TIMEOUT_MS: u64 = 5_000;

/// The frame rate of Chromium recordings without `--video-fps` (SPEC 13).
pub(crate) const DEFAULT_VIDEO_FPS: u8 = 60;

/// Run-wide flags the flow needs (SPEC 13).
#[derive(Clone, Debug, Default)]
pub(crate) struct FlowFlags {
    pub(crate) trace:            bool,
    pub(crate) video:            bool,
    /// The `--video-fps` flag; honored on Chromium only.
    pub(crate) video_fps:        Option<u8>,
    pub(crate) har:              bool,
    pub(crate) update_snapshots: bool,
    /// Set only when this flow is the run's single file.
    pub(crate) save_state:       Option<PathBuf>,
    /// The `--cache` mode (SPEC 12.1).
    pub(crate) cache:            CacheMode,
    /// `--headed`: show the browser window.
    pub(crate) headed:           bool,
    /// The `--load-state` file; the CLI rejects it together with a
    /// `storage` or `setup` option (SPEC 13).
    pub(crate) load_state:       Option<PathBuf>,
}

/// The hostname of a URL, textually: scheme and userinfo stripped, cut
/// at the first `/`, `?`, or `#`, port removed (IPv6 brackets kept
/// textual per SPEC 5). `None` when the URL has no host (`data:`).
fn url_host(url: &str) -> Option<String> {
    // Without `://` the URL is opaque (`data:`) and has no host.
    let (_, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = if let Some(end) = host_port.strip_prefix('[') {
        // IPv6 literal: keep the bracketed text without the port.
        end.split_once(']').map_or(host_port, |(ip, _)| ip)
    } else {
        host_port.split(':').next().unwrap_or_default()
    };
    (!host.is_empty()).then(|| host.to_owned())
}

/// A file's options after resolution at file start (SPEC 5, 11). The
/// command line's options are already in the file's option lines.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ResolvedOptions {
    snapshot:          SnapshotSettings,
    base:              Option<String>,
    browser:           BrowserKind,
    viewport:          Viewport,
    step_timeout_ms:   u64,
    entry_timeout_ms:  Option<u64>,
    nav_timeout_ms:    u64,
    /// As the options set it; [`ResolvedOptions::shim_allow_hosts`] adds
    /// the `base` host (SPEC 5).
    allow_hosts:       Option<Vec<String>>,
    /// Hosts blocked even when `allow_hosts` allows them (SPEC 5).
    block_hosts:       Option<Vec<String>>,
    dialogs:           DialogPolicy,
    /// The `prefers-reduced-motion` value the page sees; the engine
    /// default when unset (SPEC 5).
    reduced_motion:    Option<ReducedMotion>,
    /// Resolved relative to the `.whirl` file (SPEC 5).
    storage:           Option<PathBuf>,
    headed:            bool,
    /// Browser user agent string; the engine default when unset (SPEC 5).
    user_agent:        Option<String>,
    /// The `setup` flow, resolved relative to the `.whirl` file (SPEC 5).
    setup:             Option<PathBuf>,
    /// The model `ACT` asks (SPEC 5, 7.4).
    model:             Option<String>,
    /// Validated and inactive: only BrowserSim replay uses it (SPEC 5).
    browsersim_origin: BrowserSimOrigin,
}

/// A failure while resolving options at file start. Reported as the
/// `[setup]` entry of a failed run (SPEC 11, exit 1).
#[derive(Debug, thiserror::Error)]
enum OptionsError {
    #[error("{0}")]
    Var(#[from] VarError),
    #[error("{}: invalid {key} value '{value}'", option_place(*.line))]
    InvalidValue {
        key:   &'static str,
        value: String,
        line:  u32,
    },
}

/// Where an option line is, for messages: its line, or `-O` for an option
/// set on the command line (line 0).
fn option_place(line: u32) -> String {
    if line == 0 {
        "-O".to_owned()
    } else {
        format!("line {line}")
    }
}

/// Resolves one typed option value: a literal passes through; an
/// interpolated value resolves (SPEC 11) then re-parses its shape.
fn resolve_option<T: Clone>(
    value: &OptionValue<T>,
    key: &'static str,
    line: u32,
    vars: &mut VarStore,
    parse: impl Fn(&str) -> Option<T>,
) -> Result<T, OptionsError> {
    match value {
        OptionValue::Literal(typed) => Ok(typed.clone()),
        OptionValue::Interpolated(raw) => {
            let resolved = vars.resolve(raw)?;
            parse(&resolved).ok_or(OptionsError::InvalidValue {
                key,
                value: resolved,
                line,
            })
        }
    }
}

/// Resolves a file's options at file start (SPEC 5, 11): only
/// variables-file entries, `--var` flags, and `{{env.NAME}}` are
/// available; the `base` host is appended to `allow-hosts` when that
/// option is set. `canonical` is the flow's canonical path (SPEC 14): a
/// file's `storage` path resolves relative to it, like `UPLOAD` paths and
/// snapshot baselines.
impl ResolvedOptions {
    fn try_new(
        file: &File,
        canonical: &Path,
        vars: &mut VarStore,
        flags: &FlowFlags,
    ) -> Result<Self, OptionsError> {
        let snapshot = SnapshotSettings::default().with_options(
            file.options.iter().filter_map(|line| {
                if let FileOption::Snapshot(option) = &line.option {
                    Some((option, line.line))
                } else {
                    None
                }
            }),
            vars,
        )?;
        let mut base = None;
        let mut browser = BrowserKind::Chromium;
        let mut viewport = DEFAULT_VIEWPORT;
        let mut step_timeout_ms = DEFAULT_STEP_TIMEOUT_MS;
        let mut entry_timeout_ms = None;
        let mut nav_timeout_ms = DEFAULT_NAV_TIMEOUT_MS;
        let mut allow_hosts: Option<Vec<String>> = None;
        let mut block_hosts: Option<Vec<String>> = None;
        let mut browsersim_origin = BrowserSimOrigin::default();
        let mut dialogs = DialogPolicy::Dismiss;
        let mut reduced_motion = None;
        let mut storage: Option<PathBuf> = None;
        let mut user_agent: Option<String> = None;
        let mut setup: Option<String> = None;
        let mut model: Option<String> = None;

        for option in &file.options {
            let line = option.line;
            match &option.option {
                FileOption::Snapshot(_) => {}
                FileOption::Base(value) => base = Some(vars.resolve(value)?),
                FileOption::Browser(value) => {
                    browser = resolve_option(value, "browser", line, vars, |text| {
                        text.parse::<BrowserKind>().ok()
                    })?;
                }
                FileOption::Viewport(value) => {
                    viewport = resolve_option(value, "viewport", line, vars, |text| {
                        text.parse::<Viewport>().ok()
                    })?;
                }
                FileOption::StepTimeout(value) => {
                    step_timeout_ms = resolve_duration(value, "step-timeout", line, vars)?;
                }
                FileOption::EntryTimeout(value) => {
                    entry_timeout_ms = Some(resolve_duration(value, "entry-timeout", line, vars)?);
                }
                FileOption::NavTimeout(value) => {
                    nav_timeout_ms = resolve_duration(value, "nav-timeout", line, vars)?;
                }
                FileOption::AllowHosts(values) => allow_hosts = Some(resolve_all(values, vars)?),
                FileOption::BlockHosts(values) => block_hosts = Some(resolve_all(values, vars)?),
                FileOption::BrowserSimOrigin(value) => {
                    browsersim_origin =
                        resolve_option(value, "browsersim-origin", line, vars, |text| {
                            text.parse::<BrowserSimOrigin>().ok()
                        })?;
                }
                FileOption::Dialogs(value) => {
                    dialogs = resolve_option(value, "dialogs", line, vars, |text| {
                        text.parse::<DialogPolicy>().ok()
                    })?;
                }
                FileOption::ReducedMotion(value) => {
                    reduced_motion = Some(resolve_option(
                        value,
                        "reduced-motion",
                        line,
                        vars,
                        |text| text.parse::<ReducedMotion>().ok(),
                    )?);
                }
                FileOption::Storage(value) => {
                    let path = vars.resolve(value)?;
                    // A file's path resolves beside the file — its
                    // canonical path, so a symlinked input resolves like
                    // `UPLOAD` paths do; a command-line path resolves
                    // against the working directory (SPEC 5, 13, 14).
                    storage = Some(match option.source {
                        OptionSource::File => resolve_beside_file(canonical, &path),
                        OptionSource::CommandLine => {
                            path::absolute(&path).map_err(|_| OptionsError::InvalidValue {
                                key: "storage",
                                value: path.clone(),
                                line,
                            })?
                        }
                    });
                }
                FileOption::UserAgent(value) => user_agent = Some(vars.resolve(value)?),
                FileOption::Setup(value) => setup = Some(vars.resolve(value)?),
                FileOption::Model(value) => model = Some(vars.resolve(value)?),
            }
        }

        // The CLI rejects `--load-state` with a `storage` option.
        if let Some(path) = &flags.load_state {
            storage = Some(path.clone());
        }

        Ok(Self {
            snapshot,
            base,
            browser,
            viewport,
            step_timeout_ms,
            entry_timeout_ms,
            nav_timeout_ms,
            allow_hosts,
            block_hosts,
            dialogs,
            reduced_motion,
            storage,
            headed: flags.headed,
            user_agent,
            setup: setup.map(|path| resolve_beside_file(canonical, &path)),
            model,
            browsersim_origin,
        })
    }

    /// The `allow-hosts` list the shim enforces: the `base` host is always
    /// allowed (SPEC 5).
    fn shim_allow_hosts(&self) -> Option<Vec<String>> {
        let mut hosts = self.allow_hosts.clone()?;
        if let Some(host) = self.base.as_deref().and_then(url_host) {
            hosts.push(host);
        }
        Some(hosts)
    }

    /// Apply the validated setup handoff before creating a browser context.
    fn use_setup(&mut self, setup: &SetupHandoff) {
        self.storage = Some(setup.storage_path.clone());
    }
}

/// A duration in milliseconds as a Whirl duration: seconds when whole,
/// else milliseconds (SPEC 3.1).
fn render_duration_ms(ms: u64) -> String {
    if ms.is_multiple_of(1000) {
        format!("{}s", ms / 1000)
    } else {
        format!("{ms}ms")
    }
}

/// Resolves every value of a list option.
fn resolve_all(values: &[Value], vars: &mut VarStore) -> Result<Vec<String>, OptionsError> {
    values
        .iter()
        .map(|value| vars.resolve(value).map_err(OptionsError::from))
        .collect()
}

/// Resolves a duration option value.
fn resolve_duration(
    value: &OptionValue<DurationLit>,
    key: &'static str,
    line: u32,
    vars: &mut VarStore,
) -> Result<u64, OptionsError> {
    match value {
        OptionValue::Literal(lit) => Ok(lit.millis()),
        OptionValue::Interpolated(raw) => {
            let resolved = vars.resolve(raw)?;
            resolved
                .parse::<DurationLit>()
                .ok()
                .map(DurationLit::millis)
                .ok_or(OptionsError::InvalidValue {
                    key,
                    value: resolved,
                    line,
                })
        }
    }
}

/// A path from a `.whirl` file resolved relative to that file's
/// directory (SPEC 5, 7). Absolute paths pass through.
fn resolve_beside_file(flow_path: &Path, relative: &str) -> PathBuf {
    let relative = Path::new(relative);
    if relative.is_absolute() {
        return relative.to_path_buf();
    }
    flow_path
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join(relative)
}

/// The path of a file's `setup` flow as written, resolved against the
/// file's own directory (SPEC 5). `None` without a literal `setup`
/// option; lint rejects an interpolated one.
pub(crate) fn setup_path_for(file: &File) -> Option<PathBuf> {
    let line = file.setup_option()?;
    let FileOption::Setup(value) = &line.option else {
        return None;
    };
    let literal = value.as_literal()?;
    let parent = file.path.parent().unwrap_or_else(|| Path::new("."));
    Some(parent.join(literal))
}

/// What a finished `setup` flow hands to the files that depend on it
/// (SPEC 12): its saved storage state, its captures, and the secrets it
/// masked, so dependents mask the same values.
#[derive(Clone, Debug, Default)]
pub(crate) struct SetupHandoff {
    pub(crate) storage_path: PathBuf,
    pub(crate) captures:     Vec<(String, check::Value)>,
    pub(crate) secrets:      Vec<String>,
}

/// One flow's result: its report plus what a dependent file would need
/// from it as a `setup` flow.
#[derive(Debug)]
pub(crate) struct FlowOutcome {
    pub(crate) report:   FileReport,
    /// Every capture, unmasked, in the order taken.
    pub(crate) captures: Vec<(String, check::Value)>,
    /// The secrets the run masked.
    pub(crate) secrets:  Vec<String>,
}

/// One flow's inputs, prepared by the runner.
#[derive(Debug)]
pub(crate) struct FlowRun<'a> {
    pub(crate) file:       &'a File,
    /// The canonical flow path: snapshot baselines and `storage`,
    /// `UPLOAD`, and `DROP` paths resolve relative to it.
    pub(crate) canonical:  &'a Path,
    /// The per-flow artifact directory as reported (possibly relative).
    pub(crate) report_dir: &'a Path,
    /// The same directory, absolute, for shim commands.
    pub(crate) abs_dir:    &'a Path,
    pub(crate) flags:      &'a FlowFlags,
    /// `--variables-file` entries then `--var` flags, in order.
    pub(crate) base_vars:  &'a [(String, String)],
    /// The finished `setup` flow this file starts from, when it has one
    /// (SPEC 12).
    pub(crate) setup:      Option<&'a SetupHandoff>,
    /// Where to save the final storage state when this file is itself a
    /// `setup` flow; only a passed run writes it.
    pub(crate) state_out:  Option<&'a Path>,
    /// The run's `ACT` planner; present when any input uses `ACT`.
    pub(crate) planner:    Option<&'a dyn ActPlanner>,
    /// The run's language model client; present when any input asks a
    /// model (SPEC 6.3, 7.4).
    pub(crate) model:      Option<&'a ModelClient>,
}

/// One step line of an entry, in execution order (SPEC 12).
#[derive(Clone, Copy, Debug)]
enum StepNode<'a> {
    Action(&'a ast::Action),
    Page(&'a ast::Page),
    Assert(&'a ast::Assert),
    Judge(&'a ast::Judge),
    Capture(&'a ast::Capture),
}

impl<'a> StepNode<'a> {
    fn line(self) -> u32 {
        match self {
            Self::Action(step) => step.line,
            Self::Page(step) => step.line,
            Self::Assert(step) => step.line,
            Self::Judge(step) => step.line,
            Self::Capture(step) => step.line,
        }
    }

    /// The step's source text, as written.
    fn raw_text(self) -> &'a str {
        match self {
            Self::Action(step) => match &step.kind {
                ast::ActionKind::Http { source, .. } | ast::ActionKind::Mock { source, .. } => {
                    source
                }
                _ => &step.text,
            },
            Self::Page(step) => &step.text,
            Self::Assert(step) => &step.text,
            Self::Judge(step) => &step.text,
            Self::Capture(step) => &step.text,
        }
    }

    fn kind(self) -> StepKind {
        match self {
            Self::Action(_) => StepKind::Action,
            Self::Page(_) => StepKind::Page,
            Self::Assert(_) => StepKind::Assert,
            Self::Judge(_) => StepKind::Judge,
            Self::Capture(_) => StepKind::Capture,
        }
    }

    /// The `@duration` override on the line, in milliseconds.
    fn timeout_override(self) -> Option<u64> {
        let timeout = match self {
            Self::Action(step) => step.timeout,
            Self::Page(step) => step.timeout,
            Self::Assert(step) => step.timeout,
            Self::Judge(step) => step.timeout,
            Self::Capture(step) => step.timeout,
        };
        timeout.map(DurationLit::millis)
    }
}

/// The step lines of one entry in execution order: actions, `PAGE`,
/// then check lines in the order written (SPEC 12).
fn entry_steps(entry: &ast::Entry) -> Vec<StepNode<'_>> {
    let actions = entry.actions.iter().map(StepNode::Action);
    let page = entry.page.iter().map(StepNode::Page);
    let checks = entry.checks.iter().map(|check| match check {
        ast::CheckStep::Assert(assert) => StepNode::Assert(assert),
        ast::CheckStep::Judge(judge) => StepNode::Judge(judge),
        ast::CheckStep::Capture(capture) => StepNode::Capture(capture),
    });
    actions.chain(page).chain(checks).collect()
}

/// The per-line timeout budget (SPEC 12): `@duration` beats the line's
/// own default; `VISIT` defaults to the navigation timeout; everything
/// else to the step timeout.
fn line_budget_ms(node: StepNode<'_>, options: &ResolvedOptions) -> u64 {
    if let Some(over) = node.timeout_override() {
        return over;
    }
    match node {
        StepNode::Action(action) if matches!(action.kind, ast::ActionKind::Visit { .. }) => {
            options.nav_timeout_ms
        }
        StepNode::Action(action) if matches!(action.kind, ast::ActionKind::Goal { .. }) => {
            goal_step::DEFAULT_TIMEOUT_MS
        }
        _ => options.step_timeout_ms,
    }
}

/// The effective timeout of a step: the smaller of the line budget and
/// the remaining entry budget (SPEC 12). The flag is true when the
/// entry budget is the cap, so an expiry reports as an entry-timeout
/// failure.
fn effective_timeout_ms(line_budget: u64, entry_remaining: Option<u64>) -> (u64, bool) {
    match entry_remaining {
        Some(remaining) if remaining < line_budget => (remaining, true),
        _ => (line_budget, false),
    }
}

/// Renders a step's display text: `{{name}}` and `{{env.NAME}}` are
/// replaced with their resolved values (an unresolved reference stays
/// as written), then every recorded secret is masked (SPEC 11).
fn render_step_text(raw: &str, vars: &mut VarStore) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find("{{") {
        let (before, from_braces) = rest.split_at(start);
        out.push_str(before);
        if before.ends_with('\\') {
            // `\{{` writes a literal `{{` (SPEC 3.1).
            out.push_str("{{");
            rest = &from_braces[2..];
            continue;
        }
        let Some(end) = from_braces.find("}}") else {
            out.push_str(from_braces);
            return vars.mask(&out);
        };
        let name = &from_braces[2..end];
        let resolved = match name.strip_prefix("env.") {
            Some(env_name) => vars.resolve_env(env_name),
            None => vars.get_text(name),
        };
        match resolved {
            Some(value) => out.push_str(&value),
            None => out.push_str(&from_braces[..end + 2]),
        }
        rest = &from_braces[end + 2..];
    }
    out.push_str(rest);
    vars.mask(&out)
}

/// A step command that could not be built: the step fails before any
/// shim call (SPEC 11).
#[derive(Debug, thiserror::Error)]
enum BuildError {
    #[error("{0}")]
    Snapshot(#[from] OptionsError),
    #[error("{0}")]
    Var(#[from] VarError),
    #[error("relative URL '{url}' needs the base option")]
    NoBase { url: String },
    /// A check or capture argument that is invalid after interpolation,
    /// such as a JSONPath query.
    #[error("{0}")]
    Check(String),
}

impl BuildError {
    /// The stable report code of the failure.
    fn code(&self) -> &'static str {
        match self {
            Self::Snapshot(_) | Self::Var(_) | Self::NoBase { .. } => "variable-resolution",
            Self::Check(_) => "filter-error",
        }
    }
}

/// A step ready to run: one shim command, or an `ACT` instruction that
/// plans its own commands (SPEC 7.4).
enum PreparedStep {
    Command(StepCommand),
    Act {
        instruction: Instruction,
        /// The wire locator of the element the snapshot is limited to.
        scope:       Option<Json>,
    },
    /// An `EXTRACT` line (SPEC 7.6).
    Extract(extract_step::ExtractPlan),
    /// A `GOAL` line (SPEC 7.7).
    Goal(goal_step::GoalPlan),
    /// A `JUDGE` line (SPEC 9.8).
    Judge(judge_step::JudgePlan),
    /// A check with a subject, evaluated in Rust (SPEC 9).
    Check(check_step::PreparedCheck),
    Capture(check_step::PreparedCapture),
}

/// Mutable state of one flow run.
struct FlowExec<'a> {
    run:           &'a FlowRun<'a>,
    options:       ResolvedOptions,
    vars:          VarStore,
    warnings:      Vec<String>,
    /// True while the shim has an open flow (startFlow succeeded and no
    /// cancel closed it).
    flow_open:     bool,
    /// Every capture, unmasked, for a dependent file (SPEC 12).
    captures:      Vec<(String, check::Value)>,
    /// Responses read so far; a response never changes (SPEC 9.7).
    responses:     check_step::ResponseCache,
    /// Requests read so far, by `RESPONSE` name (SPEC 9.2).
    requests:      check_step::RequestCache,
    /// Every `MOCK` that ran, in order (SPEC 7.5).
    mocks:         Vec<RegisteredMock>,
    /// The flow's AI cache (SPEC 12.1).
    cache:         cache::FlowCache,
    /// The values `EXTRACT` lines read (SPEC 7.6).
    extracts:      extract_step::ExtractValues,
    /// The entry's `JUDGE` batches: consecutive lines with the same scope
    /// and timeout, by the line of the first (SPEC 9.8).
    judge_batches: HashMap<u32, Vec<ast::Judge>>,
    /// Answers that a batch's first line received for the later lines.
    judge_answers: HashMap<u32, JudgeAnswer>,
}

/// The resolved header pairs and body of a request or a mocked response.
type MessageParts = (Vec<(String, String)>, Option<String>);

/// A `MOCK` line that ran, for the report (SPEC 7.5).
struct RegisteredMock {
    line:   u32,
    method: String,
    /// The resolved URL pattern, masked.
    url:    String,
}

impl FlowExec<'_> {
    /// Resolves a value through the variable store.
    fn resolve(&mut self, value: &Value) -> Result<String, VarError> {
        self.vars.resolve(value)
    }

    /// Resolves an `UPLOAD` or `DROP` path to an absolute path beside the
    /// flow file (SPEC 7).
    fn file_path(&mut self, path: &Value) -> Result<String, VarError> {
        let resolved = self.resolve(path)?;
        Ok(resolve_beside_file(self.run.canonical, &resolved)
            .to_string_lossy()
            .into_owned())
    }

    /// Resolves a locator to wire JSON.
    fn locator(
        &mut self,
        locator: &ast::Locator,
        engine: Option<ast::DefaultEngine>,
    ) -> Result<Json, VarError> {
        let vars = &mut self.vars;
        wire::locator_wire(locator, engine, &mut |value| vars.resolve(value))
    }

    /// Builds the wire command of one step (protocol section 4), or the
    /// resolved instruction of an `ACT` line.
    fn prepare_step(
        &mut self,
        node: StepNode<'_>,
        implicit_response: Option<&str>,
    ) -> Result<PreparedStep, BuildError> {
        match node {
            StepNode::Action(action) => match &action.kind {
                ast::ActionKind::Act { scope, instruction } => Ok(PreparedStep::Act {
                    instruction: Instruction::try_new(instruction, &mut self.vars)?,
                    // A scope has only prefixed segments (SPEC 7.4).
                    scope:       scope
                        .as_ref()
                        .map(|scope| self.locator(scope, None))
                        .transpose()?,
                }),
                ast::ActionKind::Extract {
                    name,
                    scope,
                    instruction,
                    schema,
                } => Ok(PreparedStep::Extract(extract_step::ExtractPlan {
                    name:        name.text.clone(),
                    instruction: Instruction::try_new(instruction, &mut self.vars)?,
                    scope:       scope
                        .as_ref()
                        .map(|scope| self.locator(scope, None))
                        .transpose()?,
                    schema:      schema.as_ref().map(ast::ExtractSchema::json),
                })),
                ast::ActionKind::Goal { goal } => Ok(PreparedStep::Goal(goal_step::GoalPlan {
                    goal: Instruction::try_new(goal, &mut self.vars)?,
                })),
                _ => self.build_action(action).map(PreparedStep::Command),
            },
            StepNode::Page(page) => {
                let vars = &mut self.vars;
                let expect = wire::page_wire(&page.check, &mut |value| vars.resolve(value))?;
                Ok(PreparedStep::Command(StepCommand::Page { expect }))
            }
            StepNode::Assert(assert) => match &assert.body {
                ast::AssertBody::WindowClosed { name } => {
                    Ok(PreparedStep::Command(StepCommand::Assert {
                        spec: wire::tab_closed_wire(name),
                    }))
                }
                ast::AssertBody::ElementState { locator, state } => {
                    let vars = &mut self.vars;
                    let spec =
                        wire::state_assert_wire(locator, *state, &mut |value| vars.resolve(value))?;
                    Ok(PreparedStep::Command(StepCommand::Assert { spec }))
                }
                ast::AssertBody::Check(line) => self
                    .prepare_check(line, implicit_response)
                    .map(PreparedStep::Check),
            },
            StepNode::Capture(capture) => self
                .prepare_capture(capture, implicit_response)
                .map(PreparedStep::Capture),
            StepNode::Judge(judge) => Ok(PreparedStep::Judge(judge_step::JudgePlan {
                claim: Instruction::try_new(&judge.claim, &mut self.vars)?,
                scope: judge
                    .scope
                    .as_ref()
                    .map(|scope| self.locator(scope, None))
                    .transpose()?,
            })),
        }
    }

    /// Resolves the header lines and body of an `HTTP` request or a
    /// `MOCK` response (SPEC 7.3, 7.5). A JSON body without a
    /// `Content-Type` header gets `application/json`.
    fn message_parts(
        &mut self,
        headers: &[ast::HttpHeader],
        body: Option<&ast::HttpBody>,
    ) -> Result<MessageParts, BuildError> {
        let mut resolved = Vec::with_capacity(headers.len() + 1);
        for header in headers {
            resolved.push((header.name.clone(), self.resolve(&header.value)?));
        }
        let has_content_type = headers
            .iter()
            .any(|header| header.name.eq_ignore_ascii_case("content-type"));
        if body.is_some_and(|body| body.kind == ast::HttpBodyKind::Json) && !has_content_type {
            resolved.push(("Content-Type".to_owned(), "application/json".to_owned()));
        }
        let body = body
            .map(|body| match body.kind {
                // Typed variables insert as JSON (SPEC 11).
                ast::HttpBodyKind::Json => self.vars.resolve_json(&body.text, body.value.span),
                ast::HttpBodyKind::Text => self.resolve(&body.value),
            })
            .transpose()?;
        Ok((resolved, body))
    }

    fn resolve_url(&mut self, value: &ast::Value) -> Result<String, BuildError> {
        let resolved = self.resolve(value)?;
        if resolved.starts_with('/') {
            let Some(base) = self.options.base.as_deref() else {
                return Err(BuildError::NoBase { url: resolved });
            };
            Ok(format!("{}{resolved}", base.trim_end_matches('/')))
        } else {
            Ok(resolved)
        }
    }

    /// Builds the wire command of one action line (SPEC 7). `ACT` lines
    /// plan their commands instead; see [`Self::prepare_step`].
    fn build_action(&mut self, action: &ast::Action) -> Result<StepCommand, BuildError> {
        use ast::ActionKind as K;

        let engine = action.kind.default_engine();
        let command = match &action.kind {
            K::Popup { name } => StepCommand::Popup {
                name: name.text.clone(),
            },
            K::Window { name } => StepCommand::Tab {
                name: name.text.clone(),
            },
            K::Close { name } => StepCommand::Close {
                name: name.text.clone(),
            },
            K::Visit { url } => StepCommand::Visit {
                url: self.resolve_url(url)?,
            },
            K::Response { name, method, url } => StepCommand::Response {
                name:   name.text.clone(),
                method: method.clone(),
                url:    self.resolve_url(url)?,
            },
            K::Http {
                method,
                url,
                headers,
                body,
                ..
            } => {
                let (headers, body) = self.message_parts(headers, body.as_ref())?;
                StepCommand::Http {
                    name: wire::independent_http_response(action.line),
                    method: method.clone(),
                    url: self.resolve_url(url)?,
                    headers,
                    body,
                }
            }
            K::Mock {
                method,
                url,
                response,
                ..
            } => {
                let url = self.resolve_url(url)?;
                let pattern = wire::mock_pattern(&url).map_err(BuildError::Check)?;
                let response = match response {
                    ast::MockResponse::Fulfill {
                        status,
                        headers,
                        body,
                    } => {
                        let (headers, body) = self.message_parts(headers, body.as_ref())?;
                        json!({
                            "type": "fulfill",
                            "status": status,
                            "headers": headers,
                            "body": body,
                        })
                    }
                    ast::MockResponse::Failed => json!({"type": "failed"}),
                };
                self.mocks.push(RegisteredMock {
                    line:   action.line,
                    method: method.clone(),
                    url:    self.vars.mask(&url),
                });
                StepCommand::Mock {
                    id: action.line,
                    method: method.clone(),
                    pattern,
                    response,
                }
            }
            K::Click { target, button } => StepCommand::Click {
                locator: self.locator(target, engine)?,
                button:  button.name().to_owned(),
            },
            K::Dblclick { target } => StepCommand::Dblclick {
                locator: self.locator(target, engine)?,
            },
            K::Fill { target, value } => StepCommand::Fill {
                locator: self.locator(target, engine)?,
                value:   self.resolve(value)?,
            },
            K::Type { target, text } => StepCommand::Type {
                locator: self.locator(target, engine)?,
                text:    self.resolve(text)?,
            },
            K::Press { target, key } => StepCommand::Press {
                locator: target
                    .as_ref()
                    .map(|target| self.locator(target, engine))
                    .transpose()?,
                key:     self.resolve(key)?,
            },
            K::Check { target } => StepCommand::Checkbox {
                locator: self.locator(target, engine)?,
                checked: true,
            },
            K::Uncheck { target } => StepCommand::Checkbox {
                locator: self.locator(target, engine)?,
                checked: false,
            },
            K::Select { target, option } => StepCommand::SelectOption {
                locator: self.locator(target, engine)?,
                label:   self.resolve(option)?,
            },
            K::Hover { target } => StepCommand::Hover {
                locator: self.locator(target, engine)?,
            },
            K::Drag { source, target } => StepCommand::Drag {
                locator: self.locator(source, engine)?,
                target:  self.locator(target, engine)?,
            },
            K::ScrollIntoView { target } => StepCommand::Scroll {
                locator: Some(self.locator(target, engine)?),
                motion:  wire::scroll_motion_wire(None),
            },
            K::Scroll { target, motion } => StepCommand::Scroll {
                locator: target
                    .as_ref()
                    .map(|target| self.locator(target, engine))
                    .transpose()?,
                motion:  wire::scroll_motion_wire(Some(motion)),
            },
            K::Upload { target, path } => {
                let path = self.file_path(path)?;
                StepCommand::Upload {
                    locator: self.locator(target, engine)?,
                    path,
                }
            }
            K::Drop { target, path } => {
                let path = self.file_path(path)?;
                StepCommand::Drop {
                    locator: self.locator(target, engine)?,
                    path,
                }
            }
            K::Screenshot { name } => StepCommand::Screenshot {
                path: self
                    .run
                    .abs_dir
                    .join(artifacts::screenshot_file(&name.text))
                    .to_string_lossy()
                    .into_owned(),
            },
            K::Snapshot {
                name,
                target,
                options,
            } => {
                let settings = self.options.snapshot.with_options(
                    options.iter().map(|line| (&line.option, line.line)),
                    &mut self.vars,
                )?;
                // A target has no default engine (SPEC 6.1), so interpolation
                // changes only its values.
                let target_wire = target
                    .as_ref()
                    .map(|target| self.locator(target, None))
                    .transpose()?;
                let target_text = target.as_ref().map(|target| {
                    render_step_text(&render_snapshot_target(target), &mut self.vars)
                });
                let report = settings.report(target_text.as_deref(), &self.vars);
                let baseline = artifacts::snapshot_baseline_path(
                    self.run.canonical,
                    &name.text,
                    self.options.browser.as_str(),
                );
                // The shim's snapshot writer creates the baseline directory.
                StepCommand::Snapshot {
                    baseline_path:   baseline.to_string_lossy().into_owned(),
                    actual_path:     self
                        .run
                        .abs_dir
                        .join(artifacts::snapshot_actual_file(&name.text))
                        .to_string_lossy()
                        .into_owned(),
                    diff_path:       self
                        .run
                        .abs_dir
                        .join(artifacts::snapshot_diff_file(&name.text))
                        .to_string_lossy()
                        .into_owned(),
                    update:          self.run.flags.update_snapshots,
                    target:          target_wire,
                    masks:           settings.masks.clone(),
                    pixel_threshold: settings.threshold.value(),
                    max_diff:        settings.max_diff_wire(),
                    report:          Box::new(report),
                }
            }
            K::Eval { script } => StepCommand::EvalAction {
                script: self.resolve(script)?,
            },
            K::Act { .. } => unreachable!("prepare_step routes ACT to the act runner"),
            K::Extract { .. } => unreachable!("prepare_step routes EXTRACT to its runner"),
            K::Goal { .. } => unreachable!("prepare_step routes GOAL to its runner"),
            K::Store { scope, key, value } => StepCommand::Store {
                scope: scope.keyword().to_owned(),
                key:   self.resolve(key)?,
                value: self.resolve(value)?,
            },
        };
        Ok(command)
    }
}

/// How one executed step ended.
#[derive(Debug)]
enum StepEnd {
    Passed,
    /// A test failure (SPEC 13, exit 1).
    Failed(StepError),
    /// A runtime error (SPEC 13, exit 3).
    Error(StepError),
}

impl StepEnd {
    fn status(&self) -> Status {
        match self {
            Self::Passed => Status::Passed,
            Self::Failed(_) => Status::Failed,
            Self::Error(_) => Status::Error,
        }
    }

    fn into_error(self) -> Option<StepError> {
        match self {
            Self::Passed => None,
            Self::Failed(error) | Self::Error(error) => Some(error),
        }
    }
}

/// True for the protocol error kinds that report as runtime errors
/// (protocol section 7).
fn is_runtime_kind(kind: &str) -> bool {
    matches!(kind, "snapshot-missing-baseline" | "internal")
}

/// True for kinds an expiring timeout produces.
fn is_timeout_kind(kind: &str) -> bool {
    matches!(kind, "timeout" | "cancelled")
}

/// The entry-timeout failure detail (SPEC 12).
fn entry_timeout_error(budget_ms: u64) -> StepError {
    StepError {
        code: "entry-timeout".to_owned(),
        message: format!("entry timeout: the entry's {budget_ms}ms budget expired on this step"),
        ..StepError::default()
    }
}

/// A wire error object as a masked report detail.
fn step_error(vars: &VarStore, error: &ErrorObject) -> StepError {
    let mask = |text: &String| vars.mask(text);
    StepError {
        code:       error.kind.clone(),
        message:    format!(
            "{kind}: {message}",
            kind = error.kind,
            message = mask(&error.message)
        ),
        expected:   error.expected.as_ref().map(mask),
        actual:     error.actual.as_ref().map(mask),
        candidates: error
            .candidates
            .as_ref()
            .map(|candidates| candidates.iter().map(mask).collect()),
    }
}

/// How early a deadline the shim enforces can fire. Node's timers count
/// whole milliseconds of loop time, so a timer can fire up to 1 ms before
/// its delay has passed on Rust's clock.
const SHIM_TIMER_GRANULARITY: Duration = Duration::from_millis(1);

/// True when a step failure means the entry budget expired (SPEC 12):
/// the entry budget capped this step's timeout, and the step either
/// failed with a timeout kind or consumed the whole capped budget. An
/// `EVAL` timeout arrives as kind `eval` (protocol section 4), so the
/// kind alone is not enough. The shim's own deadline can fire within
/// [`SHIM_TIMER_GRANULARITY`] of the budget, so reaching the budget's last
/// millisecond counts as consuming it.
fn entry_budget_expired(
    kind: &str,
    entry_capped: bool,
    elapsed: Duration,
    timeout_ms: u64,
) -> bool {
    entry_capped
        && (is_timeout_kind(kind)
            || elapsed + SHIM_TIMER_GRANULARITY > Duration::from_millis(timeout_ms))
}

/// Classifies a shim error reply (protocol section 7). An expired entry
/// budget turns the failure into an entry-timeout failure (SPEC 12).
fn classify_shim_error(
    vars: &VarStore,
    error: &ErrorObject,
    expired: bool,
    entry_budget_ms: u64,
) -> StepEnd {
    if expired && !is_runtime_kind(&error.kind) {
        return StepEnd::Failed(entry_timeout_error(entry_budget_ms));
    }
    let detail = step_error(vars, error);
    if is_runtime_kind(&error.kind) {
        StepEnd::Error(detail)
    } else {
        StepEnd::Failed(detail)
    }
}

/// The timeout budget one step ran under (SPEC 12), for outcome
/// classification.
#[derive(Clone, Copy, Debug)]
struct StepBudget {
    /// True when the remaining entry budget capped this step's timeout.
    entry_capped:    bool,
    entry_budget_ms: u64,
    /// The effective timeout the step ran with.
    timeout_ms:      u64,
    /// The step's measured wall time, at full precision.
    elapsed:         Duration,
}

/// One finished step line: how it ended, its wall time, its rendered
/// text, and, for `ACT`, what it did.
struct StepRun {
    end:         StepEnd,
    duration_ms: u64,
    text:        String,
    act:         Option<ActReport>,
    snapshot:    Option<SnapshotReport>,
    ai:          Option<AiReport>,
    extract:     Option<ExtractReport>,
    judge:       Option<JudgeReport>,
    goal:        Option<GoalReport>,
    warnings:    Vec<StepWarning>,
}

impl StepRun {
    fn before_start(end: StepEnd, text: String) -> Self {
        Self {
            end,
            duration_ms: 0,
            text,
            snapshot: None,
            act: None,
            ai: None,
            extract: None,
            judge: None,
            goal: None,
            warnings: Vec::new(),
        }
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Everything recorded while one entry runs.
struct EntryState {
    steps:        Vec<StepReport>,
    captures:     Vec<(String, CaptureValue)>,
    artifacts:    Vec<String>,
    /// Remaining entry budget, when `entry-timeout` is set (SPEC 12).
    remaining_ms: Option<u64>,
}

impl FlowExec<'_> {
    /// Runs one step line: builds the command, executes it under the
    /// watchdog, applies capture and artifact effects, and classifies
    /// the outcome. An `ACT` line plans and runs its own commands.
    async fn run_step(
        &mut self,
        node: StepNode<'_>,
        implicit_response: Option<&str>,
        client: &mut ShimClient,
        state: &mut EntryState,
    ) -> StepRun {
        let entry_budget_ms = self.options.entry_timeout_ms.unwrap_or(0);
        let ai_targets = ai_step::ai_locators(node);
        // A line whose locators hold `ai:` targets resolves them before it
        // builds its command (SPEC 6.3). Resolving each description records
        // its env secrets, so the title masks them.
        let prepared = if ai_targets.is_empty() {
            self.prepare_step(node, implicit_response).map(Some)
        } else {
            for (locator, _) in &ai_targets {
                if let Some(description) = locator.ai_description() {
                    let _ = self.vars.resolve(description);
                }
            }
            Ok(None)
        };
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let text = render_step_text(node.raw_text(), &mut self.vars);
                let end = StepEnd::Failed(StepError {
                    code: error.code().to_owned(),
                    message: self.vars.mask(&error.to_string()),
                    ..StepError::default()
                });
                return StepRun::before_start(end, text);
            }
        };
        // Resolving the command recorded any env secrets, so the
        // rendered title masks them (SPEC 11).
        let title = render_step_text(node.raw_text(), &mut self.vars);

        let snapshot = match &prepared {
            Some(PreparedStep::Command(StepCommand::Snapshot { report, .. })) => {
                Some((**report).clone())
            }
            _ => None,
        };
        let line_budget = line_budget_ms(node, &self.options);
        let (timeout_ms, entry_capped) = effective_timeout_ms(line_budget, state.remaining_ms);
        if entry_capped && timeout_ms == 0 {
            let end = StepEnd::Failed(entry_timeout_error(entry_budget_ms));
            return StepRun {
                snapshot,
                ..StepRun::before_start(end, title)
            };
        }

        let started = Instant::now();
        let span = debug_span!("step", line = node.line(), step_kind = ?node.kind());
        let mut extract = None;
        let mut judge = None;
        let mut goal = None;
        let (end, act, mut spend) = match prepared {
            None => {
                let budget = act_step::ActBudget {
                    timeout_ms,
                    entry_capped,
                    entry_budget_ms,
                };
                let run = self
                    .run_ai_line(node, implicit_response, &title, budget, client, state)
                    .instrument(span)
                    .await;
                extract = run.extract;
                judge = run.judge;
                (run.end, run.act, run.spend)
            }
            Some(PreparedStep::Command(command)) => {
                let request = StepRequest {
                    entry_start: state.steps.is_empty(),
                    command,
                    timeout_ms,
                    title: Some(title.clone()),
                };
                let outcome = client.run_step(&request).instrument(span).await;
                let budget = StepBudget {
                    entry_capped,
                    entry_budget_ms,
                    timeout_ms,
                    elapsed: started.elapsed(),
                };
                (
                    self.apply_outcome(node, outcome, state, budget),
                    None,
                    ai_step::AiSpend::default(),
                )
            }
            Some(PreparedStep::Act { instruction, scope }) => {
                let budget = act_step::ActBudget {
                    timeout_ms,
                    entry_capped,
                    entry_budget_ms,
                };
                let (end, act, warnings) = self
                    .run_act(node, &instruction, scope, &title, budget, client, state)
                    .instrument(span)
                    .await;
                let spend = ai_step::AiSpend {
                    warnings,
                    ..ai_step::AiSpend::default()
                };
                (end, act, spend)
            }
            Some(PreparedStep::Extract(plan)) => {
                let budget = act_step::ActBudget {
                    timeout_ms,
                    entry_capped,
                    entry_budget_ms,
                };
                let (end, report) = self
                    .run_extract(node, plan, &title, budget, client, state)
                    .instrument(span)
                    .await;
                extract = report;
                (end, None, ai_step::AiSpend::default())
            }
            Some(PreparedStep::Goal(plan)) => {
                let budget = act_step::ActBudget {
                    timeout_ms,
                    entry_capped,
                    entry_budget_ms,
                };
                let (end, report, warnings) = self
                    .run_goal(node, plan, &title, budget, client, state)
                    .instrument(span)
                    .await;
                goal = report;
                let spend = ai_step::AiSpend {
                    warnings,
                    ..ai_step::AiSpend::default()
                };
                (end, None, spend)
            }
            Some(PreparedStep::Judge(plan)) => {
                let budget = act_step::ActBudget {
                    timeout_ms,
                    entry_capped,
                    entry_budget_ms,
                };
                let (end, report, warnings) = self
                    .run_judge(node, plan, &title, budget, client, state)
                    .instrument(span)
                    .await;
                judge = report;
                let spend = ai_step::AiSpend {
                    warnings,
                    ..ai_step::AiSpend::default()
                };
                (end, None, spend)
            }
            Some(PreparedStep::Check(mut check)) => {
                let budget = check_step::LineBudget {
                    timeout_ms,
                    entry_capped,
                    entry_budget_ms,
                };
                let end = self
                    .run_check(node, &mut check, &title, budget, client, state)
                    .instrument(span)
                    .await;
                let ai = check.take_ai();
                let spend = self.finish_ai_read(node, ai, matches!(end, StepEnd::Passed));
                (end, None, spend)
            }
            Some(PreparedStep::Capture(mut capture)) => {
                let budget = check_step::LineBudget {
                    timeout_ms,
                    entry_capped,
                    entry_budget_ms,
                };
                let end = self
                    .run_capture(node, &mut capture, &title, budget, client, state)
                    .instrument(span)
                    .await;
                let ai = capture.take_ai();
                let spend = self.finish_ai_read(node, ai, matches!(end, StepEnd::Passed));
                (end, None, spend)
            }
        };
        let duration_ms = elapsed_ms(started);
        if let Some(remaining) = state.remaining_ms.as_mut() {
            *remaining = remaining.saturating_sub(duration_ms);
        }
        let ai = spend.report(self.options.model.clone());
        StepRun {
            end,
            duration_ms,
            text: title,
            snapshot,
            act,
            ai,
            extract,
            judge,
            goal,
            warnings: spend.warnings,
        }
    }

    /// Applies a step outcome's effects and classifies it (protocol
    /// section 7, SPEC 7 and 12).
    fn apply_outcome(
        &mut self,
        node: StepNode<'_>,
        outcome: StepOutcome,
        state: &mut EntryState,
        budget: StepBudget,
    ) -> StepEnd {
        let entry_budget_ms = budget.entry_budget_ms;
        match outcome {
            StepOutcome::Ok(result) => self.apply_success(node, &result, state),
            StepOutcome::ShimError(error) => {
                let expired = entry_budget_expired(
                    &error.kind,
                    budget.entry_capped,
                    budget.elapsed,
                    budget.timeout_ms,
                );
                if let StepNode::Action(action) = node {
                    if let ast::ActionKind::Screenshot { name } = &action.kind {
                        // SCREENSHOT never fails the entry, except when
                        // the entry budget itself expired (SPEC 7).
                        if !expired {
                            self.warnings.push(format!(
                                "SCREENSHOT {name} skipped: {message}",
                                name = name.text,
                                message = self.vars.mask(&error.message)
                            ));
                            return StepEnd::Passed;
                        }
                    }
                    if let ast::ActionKind::Snapshot { name, .. } = &action.kind
                        && error.kind == "snapshot-mismatch"
                    {
                        state.artifacts.push(
                            self.report_artifact(&artifacts::snapshot_actual_file(&name.text)),
                        );
                        state
                            .artifacts
                            .push(self.report_artifact(&artifacts::snapshot_diff_file(&name.text)));
                    }
                }
                classify_shim_error(&self.vars, &error, expired, entry_budget_ms)
            }
            StepOutcome::StepTimeout { process_killed } => {
                self.flow_open = false;
                if budget.entry_capped {
                    return StepEnd::Failed(entry_timeout_error(entry_budget_ms));
                }
                let detail = if process_killed {
                    "the step timed out and the unresponsive shim process was killed"
                } else {
                    "the step timed out and could not be cancelled cleanly; \
                     the browser context was closed"
                };
                StepEnd::Failed(StepError {
                    code: "timeout".to_owned(),
                    message: format!("timeout: {detail}"),
                    ..StepError::default()
                })
            }
            StepOutcome::ProcessDied { stderr_tail } => {
                self.flow_open = false;
                StepEnd::Error(StepError {
                    code: "shim-crash".to_owned(),
                    message: self.vars.mask(&format!(
                        "the shim process died while running this step; stderr:\n{stderr_tail}"
                    )),
                    ..StepError::default()
                })
            }
        }
    }

    /// Applies the effects of a successful step: captured variables and
    /// screenshot artifacts.
    fn apply_success(
        &mut self,
        node: StepNode<'_>,
        result: &Json,
        state: &mut EntryState,
    ) -> StepEnd {
        let _ = result;
        match node {
            StepNode::Action(action) => {
                if let ast::ActionKind::Screenshot { name } = &action.kind {
                    state
                        .artifacts
                        .push(self.report_artifact(&artifacts::screenshot_file(&name.text)));
                }
                StepEnd::Passed
            }
            _ => StepEnd::Passed,
        }
    }

    /// An artifact path as reported: under the flow's report directory.
    fn report_artifact(&self, file_name: &str) -> String {
        self.run
            .report_dir
            .join(file_name)
            .to_string_lossy()
            .into_owned()
    }
}

impl FlowExec<'_> {
    /// Runs one entry: its steps in order, first failure stopping the
    /// rest (SPEC 12), then the best-effort failure screenshot.
    async fn run_entry(&mut self, entry: &ast::Entry, client: &mut ShimClient) -> EntryReport {
        let started = Instant::now();
        let mut state = EntryState {
            steps:        Vec::new(),
            captures:     Vec::new(),
            artifacts:    Vec::new(),
            remaining_ms: self.options.entry_timeout_ms,
        };
        let mut entry_status = Status::Passed;
        self.judge_batches = judge_step::batches(entry);
        self.judge_answers.clear();
        let implicit_response = entry.actions.first().and_then(|action| {
            matches!(action.kind, ast::ActionKind::Http { .. })
                .then(|| wire::independent_http_response(action.line))
        });
        for node in entry_steps(entry) {
            if entry_status != Status::Passed {
                state.steps.push(StepReport {
                    line:        node.line(),
                    kind:        node.kind(),
                    text:        self.vars.mask(node.raw_text()),
                    status:      Status::Skipped,
                    duration_ms: 0,
                    error:       None,
                    snapshot:    None,
                    act:         None,
                    warnings:    Vec::new(),
                    ai:          None,
                    extract:     None,
                    judge:       None,
                    goal:        None,
                });
                continue;
            }
            let run = self
                .run_step(node, implicit_response.as_deref(), client, &mut state)
                .await;
            let status = run.end.status();
            state.steps.push(StepReport {
                line: node.line(),
                kind: node.kind(),
                text: run.text,
                status,
                duration_ms: run.duration_ms,
                error: run.end.into_error(),
                act: run.act,
                snapshot: run.snapshot,
                warnings: run.warnings,
                ai: run.ai,
                extract: run.extract,
                judge: run.judge,
                goal: run.goal,
            });
            if status != Status::Passed {
                entry_status = status;
            }
        }
        if entry_status != Status::Passed {
            self.failure_screenshot(client, &mut state).await;
        }
        EntryReport {
            name:        self.vars.mask(&self.run.file.entry_display_name(entry)),
            line:        entry.line(),
            status:      entry_status,
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            steps:       state.steps,
            captures:    state.captures,
            artifacts:   state.artifacts,
        }
    }

    /// The best-effort full-page screenshot after a failure (SPEC 12).
    async fn failure_screenshot(&mut self, client: &mut ShimClient, state: &mut EntryState) {
        if !self.flow_open || !client.is_alive() {
            return;
        }
        let request = StepRequest {
            entry_start: false,
            command:     StepCommand::Screenshot {
                path: self
                    .run
                    .abs_dir
                    .join(artifacts::FAILURE_PNG)
                    .to_string_lossy()
                    .into_owned(),
            },
            timeout_ms:  FAILURE_SCREENSHOT_TIMEOUT_MS,
            title:       Some("failure screenshot".to_owned()),
        };
        match client.run_step(&request).await {
            StepOutcome::Ok(_) => {
                state
                    .artifacts
                    .push(self.report_artifact(artifacts::FAILURE_PNG));
            }
            StepOutcome::ShimError(_) => {}
            StepOutcome::StepTimeout { .. } | StepOutcome::ProcessDied { .. } => {
                self.flow_open = false;
            }
        }
    }
}

/// A skipped entry's report: every step skipped, no duration.
fn skipped_entry(file: &File, entry: &ast::Entry, vars: &VarStore) -> EntryReport {
    let steps = entry_steps(entry)
        .into_iter()
        .map(|node| StepReport {
            line:        node.line(),
            kind:        node.kind(),
            text:        vars.mask(node.raw_text()),
            status:      Status::Skipped,
            duration_ms: 0,
            error:       None,
            snapshot:    None,
            act:         None,
            warnings:    Vec::new(),
            ai:          None,
            extract:     None,
            judge:       None,
            goal:        None,
        })
        .collect();
    EntryReport {
        name: vars.mask(&file.entry_display_name(entry)),
        line: entry.line(),
        status: Status::Skipped,
        duration_ms: 0,
        steps,
        captures: Vec::new(),
        artifacts: Vec::new(),
    }
}

/// The synthetic `[setup]` entry for a failure before the first entry
/// (SPEC 14).
fn setup_entry(status: Status, message: String) -> EntryReport {
    EntryReport {
        name: SETUP_ENTRY.to_owned(),
        line: 0,
        status,
        duration_ms: 0,
        steps: Vec::new(),
        captures: Vec::new(),
        artifacts: Vec::new(),
    }
    .with_setup_step(message)
}

impl EntryReport {
    /// Attaches the single synthetic step that carries a setup
    /// failure's message.
    fn with_setup_step(mut self, message: String) -> Self {
        self.steps.push(StepReport {
            line:        0,
            kind:        StepKind::Action,
            text:        SETUP_ENTRY.to_owned(),
            status:      self.status,
            duration_ms: 0,
            error:       Some(StepError {
                code: "setup-failed".to_owned(),
                message,
                ..StepError::default()
            }),
            snapshot:    None,
            act:         None,
            warnings:    Vec::new(),
            ai:          None,
            extract:     None,
            judge:       None,
            goal:        None,
        });
        self
    }
}

/// Flow paths are canonical; CLI paths are made absolute during run
/// preparation. Rendering a wire path performs no filesystem access.
fn wire_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// The `startFlow` params of one flow (protocol section 3).
fn start_flow_params(run: &FlowRun<'_>, options: &ResolvedOptions) -> StartFlowParams {
    StartFlowParams {
        browser:            options.browser.as_str().to_owned(),
        headed:             options.headed,
        viewport:           ViewportParams {
            width:  options.viewport.width,
            height: options.viewport.height,
        },
        storage_state_path: options.storage.as_deref().map(wire_path),
        dialogs:            options.dialogs.as_str().to_owned(),
        allow_hosts:        options.shim_allow_hosts(),
        block_hosts:        options.block_hosts.clone(),
        nav_timeout_ms:     options.nav_timeout_ms,
        user_agent:         options.user_agent.clone(),
        reduced_motion:     options
            .reduced_motion
            .map(|motion| motion.as_str().to_owned()),
        video:              run.flags.video.then(|| VideoParams {
            temp_dir:   wire_path(&run.abs_dir.join("video-temp")),
            final_path: wire_path(&run.abs_dir.join(artifacts::VIDEO_WEBM)),
            // Only Chromium has the screencast recorder; the other engines
            // keep Playwright's recorder at its fixed rate (SPEC 13).
            fps:        (options.browser == BrowserKind::Chromium)
                .then(|| run.flags.video_fps.unwrap_or(DEFAULT_VIDEO_FPS)),
        }),
        har_path:           run
            .flags
            .har
            .then(|| wire_path(&run.abs_dir.join(artifacts::NETWORK_HAR))),
        trace:              run.flags.trace,
        open_shadow_roots:  run.file.uses_act() || run.file.uses_goal(),
        mocks:              run.file.uses_mock(),
    }
}

/// Lists each mock with the requests it served (SPEC 7.5). When the
/// file passed, a mock that served none gets the `unused-mock` warning
/// on its line.
fn report_mocks(report: &mut FileReport, mocks: &[RegisteredMock], hits: &[MockHits]) {
    for mock in mocks {
        let served = hits
            .iter()
            .find(|entry| entry.id == mock.line)
            .map_or(0, |entry| entry.hits);
        report.mocks.push(MockReport {
            line:   mock.line,
            method: mock.method.clone(),
            url:    mock.url.clone(),
            hits:   served,
        });
        if served > 0 || report.status != Status::Passed {
            continue;
        }
        let step = report
            .entries
            .iter_mut()
            .flat_map(|entry| &mut entry.steps)
            .find(|step| step.line == mock.line);
        if let Some(step) = step {
            step.warnings.push(StepWarning {
                code:    "unused-mock".to_owned(),
                message: format!("MOCK {} {} served no request", mock.method, mock.url),
            });
        }
    }
}

/// The status of a whole file from its entries.
fn file_status(entries: &[EntryReport]) -> Status {
    if entries.iter().any(|entry| entry.status == Status::Error) {
        Status::Error
    } else if entries.iter().any(|entry| entry.status == Status::Failed) {
        Status::Failed
    } else {
        Status::Passed
    }
}

/// Runs one flow through the given live shim client and returns its
/// report. The caller (the worker) respawns the client when
/// [`ShimClient::is_alive`] turns false afterwards.
pub(crate) async fn run_flow(run: &FlowRun<'_>, client: &mut ShimClient) -> FlowOutcome {
    async {
        let started = Instant::now();
        let mut report = FileReport {
            timing:             Timing::default(),
            source_sha256:      None,
            roles:              None,
            runtime:            None,
            path:               run.file.path.to_string_lossy().into_owned(),
            status:             Status::Passed,
            duration_ms:        0,
            artifacts_dir:      run.report_dir.to_string_lossy().into_owned(),
            blocked_hosts:      Vec::new(),
            blocked_host_rules: Vec::new(),
            settings:           Vec::new(),
            warnings:           Vec::new(),
            artifacts:          Vec::new(),
            mocks:              Vec::new(),
            entries:            Vec::new(),
        };
        let finish = |mut report: FileReport,
                      vars: &VarStore,
                      captures: Vec<(String, check::Value)>| {
            report.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            debug!(status = ?report.status, duration_ms = report.duration_ms, "flow finished");
            FlowOutcome {
                report,
                captures,
                secrets: vars.masker().secrets().to_vec(),
            }
        };

        let mut vars = VarStore::new();
        for (name, value) in run.base_vars {
            vars.set_input(name.clone(), value);
        }
        if let Some(setup) = run.setup {
            for secret in &setup.secrets {
                vars.record_secret(secret);
            }
            for (name, value) in &setup.captures {
                vars.set_setup(name, value.clone());
            }
        }

        // Option resolution (SPEC 11): a failure here fails the file before
        // any entry, as the `[setup]` entry of a failed run (exit 1).
        let options = match ResolvedOptions::try_new(run.file, run.canonical, &mut vars, run.flags)
        {
            Ok(mut options) => {
                // A dependent file starts from its setup flow's saved state
                // (SPEC 12); lint rejects `setup` together with `storage`.
                if let Some(setup) = run.setup {
                    options.use_setup(setup);
                }
                report.settings = settings::report(run.file, &options, &vars);
                options
            }
            Err(error) => {
                let message = vars.mask(&error.to_string());
                report.entries.push(setup_entry(Status::Failed, message));
                report.status = Status::Failed;
                return finish(report, &vars, Vec::new());
            }
        };

        if let Err(error) = fs::create_dir_all(run.abs_dir).await {
            let message = format!(
                "cannot create artifact directory '{dir}': {error}",
                dir = run.abs_dir.display()
            );
            report.entries.push(setup_entry(Status::Error, message));
            report.status = Status::Error;
            return finish(report, &vars, Vec::new());
        }

        // Browser context launch; storage loading happens here too. A shim
        // `internal` error is a runtime error; other errors are setup
        // failures (SPEC 14).
        let params = start_flow_params(run, &options);
        let runtime_result = match client.start_flow(&params).await {
            Ok(result) => result,
            Err(error) => {
                let (status, message) = match &error {
                    ShimError::Shim(object) if !is_runtime_kind(&object.kind) => {
                        (Status::Failed, vars.mask(&object.message))
                    }
                    _ => (Status::Error, vars.mask(&error.to_string())),
                };
                report.entries.push(setup_entry(status, message));
                report.status = status;
                return finish(report, &vars, Vec::new());
            }
        };
        let version = |key: &str| {
            runtime_result
                .get(key)
                .and_then(Json::as_str)
                .map(str::to_owned)
        };
        report.runtime = Some(RuntimeMetadata {
            browser:            params.browser.clone(),
            viewport:           ReportViewport {
                width:  params.viewport.width,
                height: params.viewport.height,
            },
            browser_version:    version("browserVersion"),
            user_agent:         runtime_result
                .get("userAgent")
                .and_then(Json::as_str)
                .map(|value| vars.mask(value)),
            node_version:       version("nodeVersion"),
            playwright_version: version("playwrightVersion"),
            video_fps:          runtime_result.get("videoFps").and_then(Json::as_u64),
        });

        let (flow_cache, cache_error) =
            cache::FlowCache::load(run.flags.cache, run.canonical, run.file);
        let mut exec = FlowExec {
            run,
            options,
            vars,
            warnings: Vec::new(),
            flow_open: true,
            captures: Vec::new(),
            responses: check_step::ResponseCache::new(),
            requests: check_step::RequestCache::new(),
            mocks: Vec::new(),
            cache: flow_cache,
            extracts: extract_step::ExtractValues::new(),
            judge_batches: HashMap::new(),
            judge_answers: HashMap::new(),
        };
        if let Some(error) = cache_error {
            exec.warnings.push(format!(
                "the AI cache is ignored: {}",
                exec.vars.mask(&error.to_string())
            ));
        }
        // An explicit rate that the engine cannot honor is a warning, not a
        // failure: the recording still exists at the engine's rate (SPEC 13).
        if let (Some(fps), Some(video)) = (run.flags.video_fps, &params.video)
            && video.fps.is_none()
        {
            let actual = report
                .runtime
                .as_ref()
                .and_then(|runtime| runtime.video_fps)
                .map_or_else(
                    || "the engine's fixed rate".to_owned(),
                    |actual| format!("{actual} fps"),
                );
            exec.warnings.push(format!(
                "--video-fps {fps} is not supported on {browser}; recording at {actual}",
                browser = params.browser
            ));
        }
        let mut failed = false;
        for entry in &run.file.entries {
            if failed {
                report
                    .entries
                    .push(skipped_entry(run.file, entry, &exec.vars));
                continue;
            }
            let entry_report = exec.run_entry(entry, client).await;
            failed = entry_report.status != Status::Passed;
            report.entries.push(entry_report);
        }
        report.status = file_status(&report.entries);

        // endFlow (protocol section 3): always, unless the flow was already
        // torn down by a cancel or the process died.
        if exec.flow_open && client.is_alive() {
            let trace_path = (report.status != Status::Passed && run.flags.trace)
                .then(|| run.abs_dir.join(artifacts::TRACE_ZIP));
            let end = EndFlowParams {
                save_storage_path: (report.status == Status::Passed)
                    .then(|| {
                        run.flags
                            .save_state
                            .as_deref()
                            .or(run.state_out)
                            .map(wire_path)
                    })
                    .flatten(),
                trace_path:        trace_path.as_deref().map(wire_path),
            };
            match client.end_flow(&end).await {
                Ok(result) => {
                    report.blocked_hosts = result
                        .blocked_hosts
                        .iter()
                        .map(|blocked| blocked.host.clone())
                        .collect();
                    report.blocked_host_rules = result
                        .blocked_hosts
                        .into_iter()
                        .map(|blocked| BlockedHostRule {
                            host:   blocked.host,
                            option: blocked.option,
                            glob:   blocked.glob,
                        })
                        .collect();
                    report_mocks(&mut report, &exec.mocks, &result.mocks);
                    if trace_path.is_some()
                        && let Some(entry) = report.entries.iter_mut().find(|entry| {
                            entry.status != Status::Passed && entry.status != Status::Skipped
                        })
                    {
                        entry.artifacts.push(
                            run.report_dir
                                .join(artifacts::TRACE_ZIP)
                                .to_string_lossy()
                                .into_owned(),
                        );
                    }
                    if result.video_path.is_some() {
                        report.artifacts.push(
                            run.report_dir
                                .join(artifacts::VIDEO_WEBM)
                                .to_string_lossy()
                                .into_owned(),
                        );
                    }
                    // A recording is evidence, not a result: a skipped one
                    // never changes the file's status (SPEC 13).
                    // The report then has no frame rate, as for any flow
                    // without a recording.
                    if let Some(reason) = result.video_skipped {
                        exec.warnings.push(format!(
                            "video recording skipped: {}",
                            exec.vars.mask(&reason)
                        ));
                        if let Some(runtime) = report.runtime.as_mut() {
                            runtime.video_fps = None;
                        }
                    }
                    // A blank recording stays listed, with its frame rate.
                    // The warning explains its white frame, and the file's
                    // status does not change.
                    if let Some(reason) = result.video_blank {
                        exec.warnings.push(format!(
                            "video recording is blank: {}",
                            exec.vars.mask(&reason)
                        ));
                    }
                    if run.flags.har {
                        report.artifacts.push(
                            run.report_dir
                                .join(artifacts::NETWORK_HAR)
                                .to_string_lossy()
                                .into_owned(),
                        );
                    }
                }
                Err(error) => {
                    exec.warnings.push(format!(
                        "endFlow failed: {}",
                        exec.vars.mask(&error.to_string())
                    ));
                    if report.status == Status::Passed {
                        // A clean run whose teardown (storage save, video
                        // move) failed is a runtime error, not a silent pass.
                        report.status = Status::Error;
                    }
                }
            }
        }
        match exec.cache.finish(report.status == Status::Passed) {
            Ok(_) => {}
            Err(error) => exec
                .warnings
                .push(format!("the AI cache could not be written: {error}")),
        }
        report.warnings = exec.warnings;
        finish(report, &exec.vars, exec.captures)
    }
    .instrument(info_span!("flow", entry_count = run.file.entries.len()))
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::ast::{Span, ValueSegment};
    use crate::lang::parse::parse_file;

    fn parse(source: &str) -> File {
        parse_file(Path::new("test.whirl"), source)
            .unwrap_or_else(|error| panic!("fixture should parse:\n{error}"))
    }

    /// A `{{env.NAME}}` value literal for masking tests.
    fn env_value(name: &str) -> Value {
        Value {
            segments: vec![ValueSegment::EnvVar(name.to_owned())],
            span:     Span {
                line:   1,
                column: 1,
                len:    1,
            },
            quoted:   false,
        }
    }

    #[test]
    fn visit_gets_the_nav_timeout_and_others_the_step_timeout() {
        let file = parse("VISIT /a\nCLICK \"Go\"\nASSERT title == x\n");
        let entry = &file.entries[0];
        let mut vars = VarStore::new();
        let options = ResolvedOptions::try_new(&file, &file.path, &mut vars, &FlowFlags::default())
            .expect("options resolve");
        let steps = entry_steps(entry);
        assert_eq!(line_budget_ms(steps[0], &options), DEFAULT_NAV_TIMEOUT_MS);
        assert_eq!(line_budget_ms(steps[1], &options), DEFAULT_STEP_TIMEOUT_MS);
        assert_eq!(line_budget_ms(steps[2], &options), DEFAULT_STEP_TIMEOUT_MS);
    }

    #[test]
    fn storage_resolves_beside_the_canonical_flow_path() {
        // A symlinked input's `storage` path must resolve beside the
        // real file, not beside the symlink (SPEC 5, 14).
        let file = parse("[Options]\nstorage: st.json\nVISIT /a\n");
        let mut vars = VarStore::new();
        let canonical = Path::new("/real/dir/flow.whirl");
        let options = ResolvedOptions::try_new(&file, canonical, &mut vars, &FlowFlags::default())
            .expect("options resolve");
        assert_eq!(options.storage, Some(PathBuf::from("/real/dir/st.json")));
    }

    #[test]
    fn a_duration_suffix_overrides_the_line_budget() {
        let file = parse("VISIT /a @2s\nCLICK \"Go\" @500ms\n");
        let mut vars = VarStore::new();
        let options = ResolvedOptions::try_new(&file, &file.path, &mut vars, &FlowFlags::default())
            .expect("options resolve");
        let steps = entry_steps(&file.entries[0]);
        assert_eq!(line_budget_ms(steps[0], &options), 2_000);
        assert_eq!(line_budget_ms(steps[1], &options), 500);
    }

    #[test]
    fn the_entry_budget_caps_the_effective_timeout() {
        assert_eq!(effective_timeout_ms(10_000, None), (10_000, false));
        assert_eq!(effective_timeout_ms(10_000, Some(30_000)), (10_000, false));
        assert_eq!(effective_timeout_ms(10_000, Some(700)), (700, true));
        assert_eq!(effective_timeout_ms(10_000, Some(0)), (0, true));
    }

    #[test]
    fn step_text_interpolates_variables_and_masks_env_secrets() {
        let mut vars = VarStore::new();
        vars.set_input("user", "alice");
        vars.resolve(&env_value("PATH"))
            .expect("PATH is set for tests");
        let rendered = render_step_text("FILL label:Email {{user}}:{{env.PATH}}", &mut vars);
        assert_eq!(rendered, "FILL label:Email alice:***");
    }

    #[test]
    fn an_unresolved_reference_stays_as_written_in_step_text() {
        let mut vars = VarStore::new();
        let rendered = render_step_text("VISIT {{missing}}", &mut vars);
        assert_eq!(rendered, "VISIT {{missing}}");
    }

    fn error_object(kind: &str) -> ErrorObject {
        ErrorObject {
            kind:       kind.to_owned(),
            message:    "detail".to_owned(),
            expected:   Some("a".to_owned()),
            actual:     Some("b".to_owned()),
            candidates: None,
        }
    }

    #[test]
    fn failure_kinds_map_to_entry_failures() {
        let vars = VarStore::new();
        for kind in [
            "assert",
            "timeout",
            "strictness",
            "snapshot-mismatch",
            "eval",
            "eval-result",
            "capture",
            "action",
            "cancelled",
        ] {
            let end = classify_shim_error(&vars, &error_object(kind), false, 0);
            assert!(matches!(end, StepEnd::Failed(_)), "kind {kind}");
        }
    }

    #[test]
    fn runtime_kinds_map_to_runtime_errors() {
        let vars = VarStore::new();
        for kind in ["snapshot-missing-baseline", "internal"] {
            let end = classify_shim_error(&vars, &error_object(kind), false, 0);
            assert!(matches!(end, StepEnd::Error(_)), "kind {kind}");
        }
    }

    #[test]
    fn an_expired_entry_budget_reports_as_an_entry_timeout_failure() {
        let vars = VarStore::new();
        for kind in ["timeout", "cancelled", "eval", "assert"] {
            let end = classify_shim_error(&vars, &error_object(kind), true, 2_000);
            let StepEnd::Failed(error) = end else {
                panic!("kind {kind}: expected Failed");
            };
            assert!(error.message.contains("entry timeout"), "kind {kind}");
        }
        // A failure before the budget expired keeps its own detail.
        let end = classify_shim_error(&vars, &error_object("assert"), false, 2_000);
        let StepEnd::Failed(error) = end else {
            panic!("expected Failed");
        };
        assert!(error.message.starts_with("assert:"));
    }

    #[test]
    fn budget_expiry_needs_the_cap_and_a_timeout_kind_or_a_consumed_budget() {
        let ms = Duration::from_millis;
        // A timeout kind under the cap expires whatever the timing says.
        assert!(entry_budget_expired("timeout", true, ms(0), 1_000));
        assert!(entry_budget_expired("cancelled", true, ms(0), 1_000));
        // Another kind expires only when the step consumed the whole
        // capped budget (an EVAL timeout arrives as kind `eval`).
        assert!(entry_budget_expired("eval", true, ms(1_000), 1_000));
        assert!(!entry_budget_expired("eval", true, ms(300), 1_000));
        // Without the cap the entry budget cannot expire.
        assert!(!entry_budget_expired("timeout", false, ms(5_000), 1_000));
    }

    #[test]
    fn a_shim_deadline_that_fires_in_the_last_millisecond_expires_the_budget() {
        // The shim's 995 ms EVAL timer fired early, as Node's timers can,
        // and Rust measured 994.4 ms for the whole step.
        assert!(entry_budget_expired(
            "eval",
            true,
            Duration::from_micros(994_400),
            995
        ));
        // A failure more than a millisecond before the budget is the
        // step's own failure.
        assert!(!entry_budget_expired(
            "eval",
            true,
            Duration::from_micros(993_900),
            995
        ));
    }

    #[test]
    fn shim_error_details_are_masked() {
        let mut vars = VarStore::new();
        let path = vars
            .resolve(&env_value("PATH"))
            .expect("PATH is set for tests");
        let error = ErrorObject {
            kind:       "assert".to_owned(),
            message:    format!("saw {path}"),
            expected:   Some(path.clone()),
            actual:     Some(format!("not {path}")),
            candidates: Some(vec![path.clone()]),
        };
        let detail = step_error(&vars, &error);
        assert_eq!(detail.message, "assert: saw ***");
        assert_eq!(detail.expected.as_deref(), Some("***"));
        assert_eq!(detail.actual.as_deref(), Some("not ***"));
        assert_eq!(detail.candidates, Some(vec!["***".to_owned()]));
    }
}
