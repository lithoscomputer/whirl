//! Execution of one parsed flow through one shim process: option
//! resolution (SPEC 5, 11), entry and step execution with timeout
//! budgeting (SPEC 12), failure artifacts, and the per-file report.

use std::path::{Path, PathBuf};
use std::time::Instant;
use std::{env, fs};

use serde_json::Value as Json;

use crate::lang::ast::{
    self, BrowserKind, DialogPolicy, DurationLit, File, FileOption, OptionValue, ReducedMotion,
    Value, Viewport,
};
use crate::lang::wire;
use crate::report::model::{
    EntryReport, FileReport, RuntimeMetadata, SETUP_ENTRY, Status, StepError, StepKind, StepReport,
};
use crate::run::artifacts;
use crate::run::shim::{
    CaptureResult, EndFlowParams, ErrorObject, ShimClient, ShimError, StartFlowParams, StepCommand,
    StepOutcome, StepRequest, VideoParams, ViewportParams,
};
use crate::run::vars::{VarError, VarStore};

/// Default per-step timeout (SPEC 5).
pub const DEFAULT_STEP_TIMEOUT_MS: u64 = 10_000;
/// Default navigation timeout for `VISIT` (SPEC 5).
pub const DEFAULT_NAV_TIMEOUT_MS: u64 = 30_000;
/// Default viewport (SPEC 5).
pub const DEFAULT_VIEWPORT: Viewport = Viewport {
    width:  1280,
    height: 720,
};
/// Budget for the best-effort failure screenshot (SPEC 12).
const FAILURE_SCREENSHOT_TIMEOUT_MS: u64 = 5_000;

/// Command-line overrides of file options (SPEC 5: the flag beats the
/// file option). Durations are already parsed to milliseconds.
#[derive(Clone, Debug, Default)]
pub struct Overrides {
    pub base:             Option<String>,
    pub browser:          Option<BrowserKind>,
    pub step_timeout_ms:  Option<u64>,
    pub entry_timeout_ms: Option<u64>,
    pub headed:           bool,
    pub storage:          Option<PathBuf>,
    pub user_agent:       Option<String>,
}

/// Run-wide flags the flow needs (SPEC 13).
#[derive(Clone, Debug, Default)]
pub struct FlowFlags {
    pub trace:            bool,
    pub video:            bool,
    pub har:              bool,
    pub update_snapshots: bool,
    /// Set only when this flow is the run's single file.
    pub save_storage:     Option<PathBuf>,
}

/// A `--step-timeout` / `--entry-timeout` flag value: `500ms` or `10s`.
pub fn parse_duration_flag(text: &str) -> Option<u64> {
    let (amount, factor) = if let Some(amount) = text.strip_suffix("ms") {
        (amount, 1)
    } else {
        let amount = text.strip_suffix('s')?;
        (amount, 1000)
    };
    if amount.is_empty() || !amount.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let amount: u64 = amount.parse().ok()?;
    Some(amount.saturating_mul(factor))
}

/// A `--browser` flag value: `chromium`, `firefox`, or `webkit`.
pub fn parse_browser_flag(text: &str) -> Option<BrowserKind> {
    match text {
        "chromium" => Some(BrowserKind::Chromium),
        "firefox" => Some(BrowserKind::Firefox),
        "webkit" => Some(BrowserKind::Webkit),
        _ => None,
    }
}

/// The wire name of a browser engine.
pub fn browser_name(browser: BrowserKind) -> &'static str {
    match browser {
        BrowserKind::Chromium => "chromium",
        BrowserKind::Firefox => "firefox",
        BrowserKind::Webkit => "webkit",
    }
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

/// A file's options after resolution at file start (SPEC 5, 11), with
/// command-line overrides applied.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedOptions {
    pub base:             Option<String>,
    pub browser:          BrowserKind,
    pub viewport:         Viewport,
    pub step_timeout_ms:  u64,
    pub entry_timeout_ms: Option<u64>,
    pub nav_timeout_ms:   u64,
    /// With the `base` host already appended when set (SPEC 5).
    pub allow_hosts:      Option<Vec<String>>,
    pub dialogs:          DialogPolicy,
    /// The `prefers-reduced-motion` value the page sees; the engine
    /// default when unset (SPEC 5).
    pub reduced_motion:   Option<ReducedMotion>,
    /// Resolved relative to the `.whirl` file (SPEC 5).
    pub storage:          Option<PathBuf>,
    pub headed:           bool,
    /// Browser user agent string; the engine default when unset (SPEC 5).
    pub user_agent:       Option<String>,
    /// The `setup` flow, resolved relative to the `.whirl` file (SPEC 5).
    pub setup:            Option<PathBuf>,
}

/// A failure while resolving options at file start. Reported as the
/// `[setup]` entry of a failed run (SPEC 11, exit 1).
#[derive(Debug, thiserror::Error)]
pub enum OptionsError {
    #[error("{0}")]
    Var(#[from] VarError),
    #[error("line {line}: invalid {key} value '{value}'")]
    InvalidValue {
        key:   &'static str,
        value: String,
        line:  u32,
    },
}

/// Resolves one typed option value: a literal passes through; an
/// interpolated value resolves (SPEC 11) then re-parses its shape.
fn resolve_option<T: Copy>(
    value: &OptionValue<T>,
    key: &'static str,
    line: u32,
    vars: &mut VarStore,
    parse: impl Fn(&str) -> Option<T>,
) -> Result<T, OptionsError> {
    match value {
        OptionValue::Literal(typed) => Ok(*typed),
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

/// Parses a resolved duration option value: `500ms` or `10s`.
fn parse_duration_value(text: &str) -> Option<u64> {
    parse_duration_flag(text)
}

/// Parses a resolved viewport option value: `WIDTHxHEIGHT`.
fn parse_viewport_value(text: &str) -> Option<Viewport> {
    let (width, height) = text.split_once('x')?;
    Some(Viewport {
        width:  width.parse().ok()?,
        height: height.parse().ok()?,
    })
}

/// Parses a resolved dialogs option value.
fn parse_reduced_motion_value(text: &str) -> Option<ReducedMotion> {
    match text {
        "reduce" => Some(ReducedMotion::Reduce),
        "no-preference" => Some(ReducedMotion::NoPreference),
        _ => None,
    }
}

fn parse_dialogs_value(text: &str) -> Option<DialogPolicy> {
    match text {
        "dismiss" => Some(DialogPolicy::Dismiss),
        "accept" => Some(DialogPolicy::Accept),
        _ => None,
    }
}

/// Resolves a file's options at file start (SPEC 5, 11): only
/// variables-file entries, `--var` flags, and `{{env.NAME}}` are
/// available; command-line flags override file options; the `base` host
/// is appended to `allow-hosts` when that option is set. `canonical` is
/// the flow's canonical path (SPEC 14): the `storage` path resolves
/// relative to it, like `UPLOAD` paths and snapshot baselines.
pub fn resolve_options(
    file: &File,
    canonical: &Path,
    vars: &mut VarStore,
    overrides: &Overrides,
) -> Result<ResolvedOptions, OptionsError> {
    let mut base = None;
    let mut browser = BrowserKind::Chromium;
    let mut viewport = DEFAULT_VIEWPORT;
    let mut step_timeout_ms = DEFAULT_STEP_TIMEOUT_MS;
    let mut entry_timeout_ms = None;
    let mut nav_timeout_ms = DEFAULT_NAV_TIMEOUT_MS;
    let mut allow_hosts: Option<Vec<String>> = None;
    let mut dialogs = DialogPolicy::Dismiss;
    let mut reduced_motion = None;
    let mut storage: Option<String> = None;
    let mut user_agent: Option<String> = None;
    let mut setup: Option<String> = None;

    for option in &file.options {
        let line = option.line;
        match &option.option {
            FileOption::Base(value) => base = Some(vars.resolve(value)?),
            FileOption::Browser(value) => {
                browser = resolve_option(value, "browser", line, vars, parse_browser_flag)?;
            }
            FileOption::Viewport(value) => {
                viewport = resolve_option(value, "viewport", line, vars, parse_viewport_value)?;
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
            FileOption::AllowHosts(values) => {
                let mut hosts = Vec::with_capacity(values.len());
                for value in values {
                    hosts.push(vars.resolve(value)?);
                }
                allow_hosts = Some(hosts);
            }
            FileOption::Dialogs(value) => {
                dialogs = resolve_option(value, "dialogs", line, vars, parse_dialogs_value)?;
            }
            FileOption::ReducedMotion(value) => {
                reduced_motion = Some(resolve_option(
                    value,
                    "reduced-motion",
                    line,
                    vars,
                    parse_reduced_motion_value,
                )?);
            }
            FileOption::Storage(value) => storage = Some(vars.resolve(value)?),
            FileOption::UserAgent(value) => user_agent = Some(vars.resolve(value)?),
            FileOption::Setup(value) => setup = Some(vars.resolve(value)?),
        }
    }

    // Command-line overrides (SPEC 5, 13).
    if let Some(flag) = &overrides.base {
        base = Some(flag.clone());
    }
    if let Some(flag) = overrides.browser {
        browser = flag;
    }
    if let Some(flag) = overrides.step_timeout_ms {
        step_timeout_ms = flag;
    }
    if let Some(flag) = overrides.entry_timeout_ms {
        entry_timeout_ms = Some(flag);
    }
    if let Some(flag) = &overrides.user_agent {
        user_agent = Some(flag.clone());
    }

    // The base host is always allowed (SPEC 5).
    if let (Some(hosts), Some(base)) = (allow_hosts.as_mut(), base.as_deref()) {
        if let Some(host) = url_host(base) {
            hosts.push(host);
        }
    }

    // `storage` resolves relative to the `.whirl` file — its canonical
    // path, so a symlinked input resolves like `UPLOAD` paths do
    // (SPEC 5, 14); the `--storage` flag overrides and resolves like any
    // CLI path.
    let storage = match &overrides.storage {
        Some(flag) => Some(flag.clone()),
        None => storage.map(|path| resolve_beside_file(canonical, &path)),
    };

    Ok(ResolvedOptions {
        base,
        browser,
        viewport,
        step_timeout_ms,
        entry_timeout_ms,
        nav_timeout_ms,
        allow_hosts,
        dialogs,
        reduced_motion,
        storage,
        headed: overrides.headed,
        user_agent,
        setup: setup.map(|path| resolve_beside_file(canonical, &path)),
    })
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
            parse_duration_value(&resolved).ok_or(OptionsError::InvalidValue {
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
pub fn setup_path_for(file: &File) -> Option<PathBuf> {
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
pub struct SetupHandoff {
    pub storage_path: PathBuf,
    pub captures:     Vec<(String, String)>,
    pub secrets:      Vec<String>,
}

/// One flow's result: its report plus what a dependent file would need
/// from it as a `setup` flow.
#[derive(Debug)]
pub struct FlowOutcome {
    pub report:   FileReport,
    /// Every capture, unmasked, in the order taken.
    pub captures: Vec<(String, String)>,
    /// The secrets the run masked.
    pub secrets:  Vec<String>,
}

/// One flow's inputs, prepared by the runner.
#[derive(Debug)]
pub struct FlowRun<'a> {
    pub file:       &'a File,
    /// The canonical flow path: snapshot baselines and `storage` and
    /// `UPLOAD` paths resolve relative to it.
    pub canonical:  &'a Path,
    /// The per-flow artifact directory as reported (possibly relative).
    pub report_dir: &'a Path,
    /// The same directory, absolute, for shim commands.
    pub abs_dir:    &'a Path,
    pub flags:      &'a FlowFlags,
    pub overrides:  &'a Overrides,
    /// `--variables-file` entries then `--var` flags, in order.
    pub base_vars:  &'a [(String, String)],
    /// The finished `setup` flow this file starts from, when it has one
    /// (SPEC 12).
    pub setup:      Option<&'a SetupHandoff>,
    /// Where to save the final storage state when this file is itself a
    /// `setup` flow; only a passed run writes it.
    pub state_out:  Option<&'a Path>,
}

/// One step line of an entry, in execution order (SPEC 12).
#[derive(Clone, Copy, Debug)]
enum StepNode<'a> {
    Action(&'a ast::Action),
    Page(&'a ast::Page),
    Assert(&'a ast::Assert),
    Capture(&'a ast::Capture),
}

impl<'a> StepNode<'a> {
    fn line(self) -> u32 {
        match self {
            Self::Action(step) => step.line,
            Self::Page(step) => step.line,
            Self::Assert(step) => step.line,
            Self::Capture(step) => step.line,
        }
    }

    /// The step's source text, as written.
    fn raw_text(self) -> &'a str {
        match self {
            Self::Action(step) => &step.text,
            Self::Page(step) => &step.text,
            Self::Assert(step) => &step.text,
            Self::Capture(step) => &step.text,
        }
    }

    fn kind(self) -> StepKind {
        match self {
            Self::Action(_) => StepKind::Action,
            Self::Page(_) => StepKind::Page,
            Self::Assert(_) => StepKind::Assert,
            Self::Capture(_) => StepKind::Capture,
        }
    }

    /// The `@duration` override on the line, in milliseconds.
    fn timeout_override(self) -> Option<u64> {
        let timeout = match self {
            Self::Action(step) => step.timeout,
            Self::Page(step) => step.timeout,
            Self::Assert(step) => step.timeout,
            Self::Capture(step) => step.timeout,
        };
        timeout.map(DurationLit::millis)
    }
}

/// The step lines of one entry in execution order: actions, `PAGE`,
/// asserts, captures (SPEC 12).
fn entry_steps(entry: &ast::Entry) -> Vec<StepNode<'_>> {
    let actions = entry.actions.iter().map(StepNode::Action);
    let page = entry.page.iter().map(StepNode::Page);
    let asserts = entry.asserts.iter().map(StepNode::Assert);
    let captures = entry.captures.iter().map(StepNode::Capture);
    actions.chain(page).chain(asserts).chain(captures).collect()
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
            None => vars.get(name).map(str::to_owned),
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
    Var(#[from] VarError),
    #[error("relative URL '{url}' needs the base option")]
    NoBase { url: String },
}

/// Mutable state of one flow run.
struct FlowExec<'a> {
    run:       &'a FlowRun<'a>,
    options:   ResolvedOptions,
    vars:      VarStore,
    warnings:  Vec<String>,
    /// True while the shim has an open flow (startFlow succeeded and no
    /// cancel closed it).
    flow_open: bool,
    /// Every capture, unmasked, for a dependent file (SPEC 12).
    captures:  Vec<(String, String)>,
}

impl FlowExec<'_> {
    /// Resolves a value through the variable store.
    fn resolve(&mut self, value: &Value) -> Result<String, VarError> {
        self.vars.resolve(value)
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

    /// Builds the wire command of one step (protocol section 4).
    fn build_command(&mut self, node: StepNode<'_>) -> Result<StepCommand, BuildError> {
        match node {
            StepNode::Action(action) => self.build_action(action),
            StepNode::Page(page) => {
                let vars = &mut self.vars;
                let expect = wire::page_wire(&page.check, &mut |value| vars.resolve(value))?;
                Ok(StepCommand::Page { expect })
            }
            StepNode::Assert(assert) => {
                let vars = &mut self.vars;
                let spec = wire::assert_wire(&assert.body, &mut |value| vars.resolve(value))?;
                Ok(StepCommand::Assert { spec })
            }
            StepNode::Capture(capture) => {
                let vars = &mut self.vars;
                let source =
                    wire::capture_source_wire(&capture.source, &mut |value| vars.resolve(value))?;
                Ok(StepCommand::Capture {
                    source,
                    filter: wire::filter_wire(capture.filter.as_ref()),
                })
            }
        }
    }

    /// Builds the wire command of one action line (SPEC 7).
    fn build_action(&mut self, action: &ast::Action) -> Result<StepCommand, BuildError> {
        use ast::ActionKind as K;

        let engine = action.kind.default_engine();
        let command = match &action.kind {
            K::Visit { url } => {
                let resolved = self.resolve(url)?;
                let url = if resolved.starts_with('/') {
                    let Some(base) = self.options.base.as_deref() else {
                        return Err(BuildError::NoBase { url: resolved });
                    };
                    format!("{}{resolved}", base.trim_end_matches('/'))
                } else {
                    resolved
                };
                StepCommand::Visit { url }
            }
            K::Click { target } => StepCommand::Click {
                locator: self.locator(target, engine)?,
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
            K::Upload { target, path } => {
                let resolved = self.resolve(path)?;
                let path = resolve_beside_file(self.run.canonical, &resolved);
                StepCommand::Upload {
                    locator: self.locator(target, engine)?,
                    path:    path.to_string_lossy().into_owned(),
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
            K::Snapshot { name } => {
                let baseline = artifacts::snapshot_baseline_path(
                    self.run.canonical,
                    &name.text,
                    browser_name(self.options.browser),
                );
                if self.run.flags.update_snapshots {
                    if let Some(parent) = baseline.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                }
                StepCommand::Snapshot {
                    baseline_path: baseline.to_string_lossy().into_owned(),
                    actual_path:   self
                        .run
                        .abs_dir
                        .join(artifacts::snapshot_actual_file(&name.text))
                        .to_string_lossy()
                        .into_owned(),
                    diff_path:     self
                        .run
                        .abs_dir
                        .join(artifacts::snapshot_diff_file(&name.text))
                        .to_string_lossy()
                        .into_owned(),
                    update:        self.run.flags.update_snapshots,
                }
            }
            K::Eval { script } => StepCommand::EvalAction {
                script: self.resolve(script)?,
            },
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

/// True when a step failure means the entry budget expired (SPEC 12):
/// the entry budget capped this step's timeout, and the step either
/// failed with a timeout kind or consumed the whole capped budget. An
/// `EVAL` timeout arrives as kind `eval` (protocol section 4), so the
/// kind alone is not enough.
fn entry_budget_expired(kind: &str, entry_capped: bool, elapsed_ms: u64, timeout_ms: u64) -> bool {
    entry_capped && (is_timeout_kind(kind) || elapsed_ms >= timeout_ms)
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
    /// The step's measured wall time.
    elapsed_ms:      u64,
}

/// Everything recorded while one entry runs.
struct EntryState {
    steps:        Vec<StepReport>,
    captures:     Vec<(String, String)>,
    artifacts:    Vec<String>,
    /// Remaining entry budget, when `entry-timeout` is set (SPEC 12).
    remaining_ms: Option<u64>,
}

impl FlowExec<'_> {
    /// Runs one step line: builds the command, executes it under the
    /// watchdog, applies capture and artifact effects, and classifies
    /// the outcome.
    async fn run_step(
        &mut self,
        node: StepNode<'_>,
        client: &mut ShimClient,
        state: &mut EntryState,
    ) -> (StepEnd, u64, String) {
        let entry_budget_ms = self.options.entry_timeout_ms.unwrap_or(0);
        let command = match self.build_command(node) {
            Ok(command) => command,
            Err(error) => {
                let text = render_step_text(node.raw_text(), &mut self.vars);
                let end = StepEnd::Failed(StepError {
                    code: "variable-resolution".to_owned(),
                    message: self.vars.mask(&error.to_string()),
                    ..StepError::default()
                });
                return (end, 0, text);
            }
        };
        // Resolving the command recorded any env secrets, so the
        // rendered title masks them (SPEC 11).
        let title = render_step_text(node.raw_text(), &mut self.vars);

        let line_budget = line_budget_ms(node, &self.options);
        let (timeout_ms, entry_capped) = effective_timeout_ms(line_budget, state.remaining_ms);
        if entry_capped && timeout_ms == 0 {
            let end = StepEnd::Failed(entry_timeout_error(entry_budget_ms));
            return (end, 0, title);
        }

        let request = StepRequest {
            command,
            timeout_ms,
            title: title.clone(),
        };
        let started = Instant::now();
        let outcome = client.run_step(&request).await;
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if let Some(remaining) = state.remaining_ms.as_mut() {
            *remaining = remaining.saturating_sub(elapsed_ms);
        }

        let budget = StepBudget {
            entry_capped,
            entry_budget_ms,
            timeout_ms,
            elapsed_ms,
        };
        let end = self.apply_outcome(node, outcome, state, budget);
        (end, elapsed_ms, title)
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
                    budget.elapsed_ms,
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
                    if let ast::ActionKind::Snapshot { name } = &action.kind {
                        if error.kind == "snapshot-mismatch" {
                            state.artifacts.push(
                                self.report_artifact(&artifacts::snapshot_actual_file(&name.text)),
                            );
                            state.artifacts.push(
                                self.report_artifact(&artifacts::snapshot_diff_file(&name.text)),
                            );
                        }
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
        match node {
            StepNode::Capture(capture) => {
                let Ok(CaptureResult { value }) =
                    serde_json::from_value::<CaptureResult>(result.clone())
                else {
                    return StepEnd::Error(StepError {
                        message: "internal: malformed capture result from the shim".to_owned(),
                        ..StepError::default()
                    });
                };
                state
                    .captures
                    .push((capture.name.text.clone(), self.vars.mask(&value)));
                self.captures
                    .push((capture.name.text.clone(), value.clone()));
                self.vars.set(capture.name.text.clone(), value);
                StepEnd::Passed
            }
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
        for node in entry_steps(entry) {
            if entry_status != Status::Passed {
                state.steps.push(StepReport {
                    line:        node.line(),
                    kind:        node.kind(),
                    text:        self.vars.mask(node.raw_text()),
                    status:      Status::Skipped,
                    duration_ms: 0,
                    error:       None,
                });
                continue;
            }
            let (end, duration_ms, text) = self.run_step(node, client, &mut state).await;
            let status = end.status();
            state.steps.push(StepReport {
                line: node.line(),
                kind: node.kind(),
                text,
                status,
                duration_ms,
                error: end.into_error(),
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
            command:    StepCommand::Screenshot {
                path: self
                    .run
                    .abs_dir
                    .join(artifacts::FAILURE_PNG)
                    .to_string_lossy()
                    .into_owned(),
            },
            timeout_ms: FAILURE_SCREENSHOT_TIMEOUT_MS,
            title:      "failure screenshot".to_owned(),
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
        });
        self
    }
}

/// A path rendered for the wire. The shim protocol declares every path
/// param as absolute, so a relative CLI path (`--storage`,
/// `--save-storage`) resolves against the current working directory
/// first. When the working directory is unreadable, the path is sent as
/// given — the shim inherits the same directory either way.
fn wire_path(path: &Path) -> String {
    if path.is_absolute() {
        return path.to_string_lossy().into_owned();
    }
    match env::current_dir() {
        Ok(cwd) => cwd.join(path).to_string_lossy().into_owned(),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

/// The `startFlow` params of one flow (protocol section 3).
fn start_flow_params(run: &FlowRun<'_>, options: &ResolvedOptions) -> StartFlowParams {
    StartFlowParams {
        browser:            browser_name(options.browser).to_owned(),
        headed:             options.headed,
        viewport:           ViewportParams {
            width:  options.viewport.width,
            height: options.viewport.height,
        },
        storage_state_path: options.storage.as_deref().map(wire_path),
        dialogs:            match options.dialogs {
            DialogPolicy::Dismiss => "dismiss".to_owned(),
            DialogPolicy::Accept => "accept".to_owned(),
        },
        allow_hosts:        options.allow_hosts.clone(),
        nav_timeout_ms:     options.nav_timeout_ms,
        user_agent:         options.user_agent.clone(),
        reduced_motion:     options.reduced_motion.map(|motion| {
            match motion {
                ReducedMotion::Reduce => "reduce",
                ReducedMotion::NoPreference => "no-preference",
            }
            .to_owned()
        }),
        video:              run.flags.video.then(|| VideoParams {
            temp_dir:   wire_path(&run.abs_dir.join("video-temp")),
            final_path: wire_path(&run.abs_dir.join(artifacts::VIDEO_WEBM)),
        }),
        har_path:           run
            .flags
            .har
            .then(|| wire_path(&run.abs_dir.join(artifacts::NETWORK_HAR))),
        trace:              run.flags.trace,
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
pub async fn run_flow(run: &FlowRun<'_>, client: &mut ShimClient) -> FlowOutcome {
    let started = Instant::now();
    let mut report = FileReport {
        runtime:       None,
        path:          run.file.path.to_string_lossy().into_owned(),
        status:        Status::Passed,
        duration_ms:   0,
        artifacts_dir: run.report_dir.to_string_lossy().into_owned(),
        blocked_hosts: Vec::new(),
        warnings:      Vec::new(),
        artifacts:     Vec::new(),
        entries:       Vec::new(),
    };
    let finish = |mut report: FileReport, vars: &VarStore, captures: Vec<(String, String)>| {
        report.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        FlowOutcome {
            report,
            captures,
            secrets: vars.masker().secrets().to_vec(),
        }
    };

    let mut vars = VarStore::new();
    for (name, value) in run.base_vars {
        vars.set(name.clone(), value.clone());
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
    let options = match resolve_options(run.file, run.canonical, &mut vars, run.overrides) {
        Ok(mut options) => {
            // A dependent file starts from its setup flow's saved state
            // (SPEC 12); lint rejects `setup` together with `storage`.
            if let Some(setup) = run.setup {
                options.storage = Some(setup.storage_path.clone());
            }
            options
        }
        Err(error) => {
            let message = vars.mask(&error.to_string());
            report.entries.push(setup_entry(Status::Failed, message));
            report.status = Status::Failed;
            return finish(report, &vars, Vec::new());
        }
    };

    if let Err(error) = fs::create_dir_all(run.abs_dir) {
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
        viewport:           params.viewport,
        browser_version:    version("browserVersion"),
        node_version:       version("nodeVersion"),
        playwright_version: version("playwrightVersion"),
    });

    let mut exec = FlowExec {
        run,
        options,
        vars,
        warnings: Vec::new(),
        flow_open: true,
        captures: Vec::new(),
    };
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
                        .save_storage
                        .as_deref()
                        .or(run.state_out)
                        .map(wire_path)
                })
                .flatten(),
            trace_path:        trace_path.as_deref().map(wire_path),
        };
        match client.end_flow(&end).await {
            Ok(result) => {
                report.blocked_hosts = result.blocked_hosts;
                if trace_path.is_some() {
                    if let Some(entry) = report.entries.iter_mut().find(|entry| {
                        entry.status != Status::Passed && entry.status != Status::Skipped
                    }) {
                        entry.artifacts.push(
                            run.report_dir
                                .join(artifacts::TRACE_ZIP)
                                .to_string_lossy()
                                .into_owned(),
                        );
                    }
                }
                if result.video_path.is_some() {
                    report.artifacts.push(
                        run.report_dir
                            .join(artifacts::VIDEO_WEBM)
                            .to_string_lossy()
                            .into_owned(),
                    );
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
    report.warnings = exec.warnings;
    finish(report, &exec.vars, exec.captures)
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
    fn duration_flags_parse_ms_and_s() {
        assert_eq!(parse_duration_flag("500ms"), Some(500));
        assert_eq!(parse_duration_flag("10s"), Some(10_000));
        assert_eq!(parse_duration_flag("0ms"), Some(0));
        assert_eq!(parse_duration_flag("10"), None);
        assert_eq!(parse_duration_flag("s"), None);
        assert_eq!(parse_duration_flag("1.5s"), None);
    }

    #[test]
    fn visit_gets_the_nav_timeout_and_others_the_step_timeout() {
        let file = parse("VISIT /a\nCLICK \"Go\"\n[Asserts]\ntitle == x\n");
        let entry = &file.entries[0];
        let mut vars = VarStore::new();
        let options = resolve_options(&file, &file.path, &mut vars, &Overrides::default())
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
        let options = resolve_options(&file, canonical, &mut vars, &Overrides::default())
            .expect("options resolve");
        assert_eq!(options.storage, Some(PathBuf::from("/real/dir/st.json")));
    }

    #[test]
    fn wire_paths_are_absolute() {
        // The shim protocol declares every path param as absolute, so a
        // relative `--storage` / `--save-storage` path resolves against
        // the current working directory before crossing the wire.
        let cwd = env::current_dir().expect("the working directory is readable");
        assert_eq!(
            wire_path(Path::new("nested/state.json")),
            cwd.join("nested/state.json").to_string_lossy()
        );
        assert_eq!(wire_path(Path::new("/abs/state.json")), "/abs/state.json");
    }

    #[test]
    fn a_duration_suffix_overrides_the_line_budget() {
        let file = parse("VISIT /a @2s\nCLICK \"Go\" @500ms\n");
        let mut vars = VarStore::new();
        let options = resolve_options(&file, &file.path, &mut vars, &Overrides::default())
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
        vars.set("user", "alice");
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
        // A timeout kind under the cap expires whatever the timing says.
        assert!(entry_budget_expired("timeout", true, 0, 1_000));
        assert!(entry_budget_expired("cancelled", true, 0, 1_000));
        // Another kind expires only when the step consumed the whole
        // capped budget (an EVAL timeout arrives as kind `eval`).
        assert!(entry_budget_expired("eval", true, 1_000, 1_000));
        assert!(!entry_budget_expired("eval", true, 300, 1_000));
        // Without the cap the entry budget cannot expire.
        assert!(!entry_budget_expired("timeout", false, 5_000, 1_000));
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
