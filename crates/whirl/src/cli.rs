//! Command-line surface (SPEC section 13): argument parsing, path
//! expansion, and central exit-code handling.
//!
//! [`run`] is the binary's whole entry point. Exit codes follow SPEC 13:
//! a usage error (4) preempts everything, and within an invocation a
//! runtime error (3) outranks parse or lint errors (2), which outrank the
//! command's negative result (1), which outranks success (0).

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::{fs, io};

use anyhow::Context as _;
use clap::error::ErrorKind;
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::runtime::Runtime;

use crate::lang::lint::{Lint, Severity, lint_file_with, lint_setup_refs, setup_capture_uses};
use crate::lang::parse::{ParseError, parse_file};
use crate::lang::{ast, fmt};
use crate::report::metadata::ReportMetadata;
use crate::report::model::Status;
use crate::report::{console, html, json, junit};
use crate::run::{artifacts, flow, runner, vars};
use crate::{doctor, install, telemetry};

/// Outcome of one invocation, ordered by SPEC 13 precedence: `max` of two
/// outcomes is the one that wins the process exit code.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Exit {
    /// 0: all files passed (or nothing to do).
    Success,
    /// 1: the command's negative result — a failed entry (run) or
    /// formatting drift (`fmt --check`).
    Negative,
    /// 2: parse or lint error.
    ParseLint,
    /// 3: runtime error (browser, shim, or environment failure).
    Runtime,
    /// 4: usage error.
    Usage,
}

impl Exit {
    fn code(self) -> u8 {
        match self {
            Self::Success => 0,
            Self::Negative => 1,
            Self::ParseLint => 2,
            Self::Runtime => 3,
            Self::Usage => 4,
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "whirl",
    version,
    about = "Run web UI tests written in plain-text .whirl files",
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    run: RunArgs,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Generate HTML from saved results without running browsers.
    Report(ReportArgs),
    /// Parse and lint files; nothing runs.
    Check {
        /// Write versioned JSON diagnostics to stdout.
        #[arg(long)]
        json:  bool,
        /// Files to check; directories recurse to *.whirl.
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<PathBuf>,
    },
    /// Rewrite files to the canonical form.
    Fmt {
        /// Write nothing; exit 1 when any file would change.
        #[arg(long)]
        check: bool,
        /// Files to format; directories recurse to *.whirl.
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<PathBuf>,
    },
    /// Provision the shim bundle and browsers.
    Install {
        /// Browser engines to install; defaults to all three.
        #[arg(value_name = "BROWSER", value_parser = ["chromium", "firefox", "webkit"])]
        browsers: Vec<String>,
    },
    /// Check the runtime and launch a browser; print repair commands on
    /// failure.
    Doctor {
        /// Browser engine to check.
        #[arg(long, default_value = "chromium", value_parser = ["chromium", "firefox", "webkit"])]
        browser: String,
    },
    /// Open a Playwright trace with Whirl's private runtime.
    ShowTrace {
        /// Trace archive to open.
        path: PathBuf,
    },
}

#[derive(Debug, Args)]
struct ReportArgs {
    /// Version 1 Whirl JSON reports. Select the latest attempt for each flow.
    #[arg(required = true, value_name = "REPORT")]
    reports:           Vec<PathBuf>,
    /// Write a standalone HTML report.
    #[arg(long, value_name = "PATH")]
    html:              PathBuf,
    /// Replace saved author context; flow keys match recorded paths exactly.
    #[arg(long, value_name = "PATH")]
    metadata:          Option<PathBuf>,
    /// JSON array of expected flow paths, in scenario order. Missing flows show
    /// Not run.
    #[arg(long, value_name = "PATH")]
    expected:          Option<PathBuf>,
    /// Resolve relative artifact paths here instead of the recorded directory.
    #[arg(long, value_name = "DIR")]
    working_directory: Option<PathBuf>,
}

/// Flags for the default run command (SPEC 13). The runner is a later
/// phase; the flags are accepted now so the surface is stable.
#[derive(Clone, Debug, Args)]
#[command(group(clap::ArgGroup::new("report_context_output").args(["report_json", "report_html"]).multiple(true)))]
struct RunArgs {
    /// Files to run; directories recurse to *.whirl.
    #[arg(required_unless_present = "rerun_failed", value_name = "PATH")]
    paths: Vec<PathBuf>,

    /// Rerun whole failed or errored files from a JSON report.
    #[arg(long, value_name = "REPORT", conflicts_with = "paths")]
    rerun_failed: Option<PathBuf>,

    /// Override the `base` option.
    #[arg(long, value_name = "URL")]
    base: Option<String>,

    /// Override the `browser` option.
    #[arg(long, value_name = "NAME")]
    browser: Option<String>,

    /// Override the `step-timeout` option.
    #[arg(long, value_name = "DURATION")]
    step_timeout: Option<String>,

    /// Run with a visible browser window.
    #[arg(long)]
    headed: bool,

    /// Worker slots for parallel files.
    #[arg(long, value_name = "N")]
    jobs: Option<usize>,

    /// Define a variable (repeatable).
    #[arg(long, value_name = "k=v")]
    var: Vec<String>,

    /// Load variables from a file.
    #[arg(long, value_name = "PATH")]
    variables_file: Option<PathBuf>,

    /// Artifact output directory.
    #[arg(long, value_name = "DIR", default_value = "whirl-artifacts")]
    artifacts: PathBuf,

    /// Record a Playwright trace per file; saved only when the file fails.
    #[arg(long)]
    trace: bool,

    /// Write a JUnit XML report.
    #[arg(long, value_name = "PATH")]
    report_junit: Option<PathBuf>,

    /// Write a JSON report.
    #[arg(long, value_name = "PATH")]
    report_json: Option<PathBuf>,

    /// Write a standalone HTML report with embedded recordings and screenshots.
    #[arg(long, value_name = "PATH")]
    report_html: Option<PathBuf>,

    /// Read author-written report and flow descriptions from a JSON file.
    #[arg(long, value_name = "PATH", requires = "report_context_output")]
    report_metadata: Option<PathBuf>,

    /// Stop scheduling new files after the first failure.
    #[arg(long)]
    fail_fast: bool,

    /// Write or refresh SNAPSHOT baselines instead of comparing.
    #[arg(long)]
    update_snapshots: bool,

    /// Record a .webm video of each file's run.
    #[arg(long)]
    video: bool,

    /// Record a .har network log per file.
    #[arg(long)]
    har: bool,

    /// Override the `storage` option.
    #[arg(long, value_name = "PATH")]
    storage: Option<PathBuf>,

    /// Write the final storage state after a successful run.
    #[arg(long, value_name = "PATH")]
    save_storage: Option<PathBuf>,

    /// Override the `entry-timeout` option.
    #[arg(long, value_name = "DURATION")]
    entry_timeout: Option<String>,

    /// Override the user agent: chrome, firefox, safari, or a literal string.
    #[arg(long, value_name = "UA")]
    user_agent: Option<String>,
}

/// Runs the CLI for the given argv (including the program name) and
/// returns the process exit code.
pub fn run(argv: impl IntoIterator<Item = OsString>) -> ExitCode {
    telemetry::init();
    ExitCode::from(execute(argv))
}

fn execute(argv: impl IntoIterator<Item = OsString>) -> u8 {
    let cli = match Cli::try_parse_from(argv) {
        Ok(cli) => cli,
        Err(error) => return exit_for_clap_error(&error),
    };
    let exit = match cli.command {
        Some(Command::Report(args)) => report_command(&args),
        Some(Command::Check { paths, json }) => check_command(&paths, json),
        Some(Command::Fmt { check, paths }) => fmt_command(check, &paths),
        Some(Command::Install { browsers }) => install_command(&browsers),
        Some(Command::Doctor { browser }) => {
            match doctor::run(&browser, &mut |line| print_out(line)) {
                Ok(()) => Exit::Success,
                Err(error) => {
                    print_err(&format!("whirl: error: {error:#}"));
                    Exit::Runtime
                }
            }
        }
        Some(Command::ShowTrace { path }) => {
            if !path.is_file() {
                print_err(&format!(
                    "whirl: error: trace '{}' is not a file",
                    path.display()
                ));
                Exit::Usage
            } else if let Err(error) = install::show_trace(&path) {
                print_err(&format!("whirl: error: {error:#}"));
                Exit::Runtime
            } else {
                Exit::Success
            }
        }
        None => run_command(&cli.run),
    };
    exit.code()
}

/// Prints a clap error and maps it to an exit code: help and version are
/// success; everything else is a usage error (SPEC 13, exit 4).
fn exit_for_clap_error(error: &clap::Error) -> u8 {
    let _ = error.print();
    match error.kind() {
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => Exit::Success.code(),
        _ => Exit::Usage.code(),
    }
}

#[expect(clippy::print_stdout, reason = "the CLI's stdout boundary")]
fn print_out(text: &str) {
    println!("{text}");
}

#[expect(clippy::print_stderr, reason = "the CLI's stderr boundary")]
fn print_err(text: &str) {
    eprintln!("{text}");
}

/// A usage error: a bad path argument (SPEC 13, exit 4).
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct UsageError {
    message: String,
}

/// Expands each PATH argument: a file is taken as-is, a directory
/// recurses to `*.whirl` files (sorted), and a nonexistent path is a
/// usage error.
fn expand_paths(paths: &[PathBuf]) -> Result<Vec<PathBuf>, UsageError> {
    let mut files = Vec::new();
    for path in paths {
        if path.is_dir() {
            let mut found = Vec::new();
            collect_whirl_files(path, &mut found).map_err(|error| UsageError {
                message: format!("cannot read directory '{}': {error}", path.display()),
            })?;
            found.sort();
            files.extend(found);
        } else if path.is_file() {
            files.push(path.clone());
        } else {
            return Err(UsageError {
                message: format!("path '{}' does not exist", path.display()),
            });
        }
    }
    if files.is_empty() {
        return Err(UsageError {
            message: "no .whirl files found in the input paths".to_owned(),
        });
    }
    Ok(files)
}

/// Recursively collects `*.whirl` files under `dir`.
fn collect_whirl_files(dir: &Path, found: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_whirl_files(&path, found)?;
        } else if path.extension().is_some_and(|ext| ext == "whirl") && path.is_file() {
            found.push(path);
        }
    }
    Ok(())
}

/// Reads every input file. An unreadable file that exists is an
/// environmental failure (SPEC 16, exit 3), not a usage error.
fn load_sources(files: &[PathBuf]) -> anyhow::Result<Vec<(PathBuf, String)>> {
    files
        .iter()
        .map(|path| {
            let source = fs::read_to_string(path)
                .with_context(|| format!("cannot read '{}'", path.display()))?;
            Ok((path.clone(), source))
        })
        .collect()
}

/// One input file's parse outcome, with the source kept for diagnostics.
struct ParsedInput {
    file:   ast::File,
    source: String,
}

/// Parses every input. Files that parse are returned even when others
/// fail, so `check` can lint and report everything in one pass.
fn parse_inputs(sources: Vec<(PathBuf, String)>) -> (Vec<ParsedInput>, Vec<ParseError>) {
    let mut parsed = Vec::new();
    let mut errors = Vec::new();
    for (path, source) in sources {
        match parse_file(&path, &source) {
            Ok(file) => parsed.push(ParsedInput { file, source }),
            Err(error) => errors.push(error),
        }
    }
    (parsed, errors)
}

/// Renders a lint diagnostic in the parse-error style (SPEC 16): file,
/// line, column, the source line, and a caret under the offending token.
fn render_lint(lint: &Lint, source: &str) -> String {
    let severity = match lint.severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
    };
    let location = format!("{}:{}:{}", lint.path.display(), lint.line, lint.column);
    let line_index = usize::try_from(lint.line.saturating_sub(1)).unwrap_or(0);
    let source_line = source
        .lines()
        .nth(line_index)
        .unwrap_or_default()
        .trim_end_matches('\r');
    let pad = " ".repeat(usize::try_from(lint.column.saturating_sub(1)).unwrap_or(0));
    let caret = "^".repeat(usize::try_from(lint.len.max(1)).unwrap_or(1));
    format!(
        "{location}: {severity}: {message}\n  {source_line}\n  {pad}{caret}",
        message = lint.message
    )
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Diagnostic {
    code:     &'static str,
    severity: &'static str,
    path:     Option<PathBuf>,
    line:     Option<u32>,
    column:   Option<u32>,
    length:   Option<u32>,
    message:  String,
    expected: Vec<String>,
    #[serde(skip)]
    rendered: String,
}

impl Diagnostic {
    fn parse(error: &ParseError) -> Self {
        Self {
            code:     "parse-error",
            severity: "error",
            path:     Some(error.path.clone()),
            line:     Some(error.line),
            column:   Some(error.column),
            length:   Some(error.len),
            message:  error.message.clone(),
            expected: error.expected.clone(),
            rendered: error.render(),
        }
    }

    fn lint(lint: &Lint, source: &str) -> Self {
        Self {
            code:     lint.code,
            severity: if lint.severity == Severity::Warning {
                "warning"
            } else {
                "error"
            },
            path:     Some(lint.path.clone()),
            line:     Some(lint.line),
            column:   Some(lint.column),
            length:   Some(lint.len),
            message:  lint.message.clone(),
            expected: Vec::new(),
            rendered: render_lint(lint, source),
        }
    }

    fn environment(code: &'static str, message: String) -> Self {
        Self {
            code,
            severity: "error",
            path: None,
            line: None,
            column: None,
            length: None,
            rendered: message.clone(),
            message,
            expected: Vec::new(),
        }
    }
}

fn print_diagnostics(diagnostics: &[Diagnostic], json: bool, exit: Exit) {
    if json {
        print_out(
            &serde_json::to_string_pretty(&serde_json::json!({
                "version": 1, "exitCode": exit.code(), "diagnostics": diagnostics
            }))
            .expect("diagnostics serialize"),
        );
    } else {
        for diagnostic in diagnostics {
            print_err(&diagnostic.rendered);
        }
    }
}

/// The parsed inputs plus the `setup` flows they name that are not
/// inputs themselves (SPEC 12).
struct CheckedInputs {
    inputs: Vec<ParsedInput>,
    setups: Vec<ParsedInput>,
}

/// Parses and lints every input and every `setup` flow they name,
/// printing all diagnostics. Returns the parsed files and the worst
/// outcome: [`Exit::ParseLint`] when any parse error or lint error was
/// found, [`Exit::Runtime`] when a setup file cannot be read, and
/// [`Exit::Success`] otherwise (lint warnings never change the exit
/// code, SPEC 16).
fn check_inputs(
    sources: Vec<(PathBuf, String)>,
    diagnostics: &mut Vec<Diagnostic>,
) -> (CheckedInputs, Exit) {
    let (inputs, parse_errors) = parse_inputs(sources);
    for error in &parse_errors {
        diagnostics.push(Diagnostic::parse(error));
    }
    let mut exit = if parse_errors.is_empty() {
        Exit::Success
    } else {
        Exit::ParseLint
    };

    // Setup flows: those named by an input that are not inputs themselves
    // are read and parsed here (SPEC 12).
    let mut setups: Vec<ParsedInput> = Vec::new();
    let mut setup_of: Vec<(usize, PathBuf)> = Vec::new();
    for (index, input) in inputs.iter().enumerate() {
        let Some(path) = flow::setup_path_for(&input.file) else {
            continue;
        };
        let Some(canonical) = path.canonicalize().ok() else {
            diagnostics.push(Diagnostic::environment(
                "setup-io",
                format!(
                    "whirl: error: {}: setup flow '{}' does not exist",
                    input.file.path.display(),
                    path.display()
                ),
            ));
            exit = exit.max(Exit::Runtime);
            continue;
        };
        setup_of.push((index, canonical.clone()));
        let already_known = inputs
            .iter()
            .chain(setups.iter())
            .any(|known| known.file.path.canonicalize().ok().as_ref() == Some(&canonical));
        if already_known {
            continue;
        }
        match fs::read_to_string(&path) {
            Ok(source) => match parse_file(&path, &source) {
                Ok(file) => setups.push(ParsedInput { file, source }),
                Err(error) => {
                    diagnostics.push(Diagnostic::parse(&error));
                    exit = exit.max(Exit::ParseLint);
                }
            },
            Err(error) => {
                diagnostics.push(Diagnostic::environment(
                    "setup-io",
                    format!(
                        "whirl: error: cannot read setup flow '{}': {error}",
                        path.display()
                    ),
                ));
                exit = exit.max(Exit::Runtime);
            }
        }
    }

    // A setup flow's captures count as used when a dependent reads them
    // as {{setup.name}} (SPEC 16).
    let find = |canonical: &PathBuf| -> Option<&ParsedInput> {
        inputs
            .iter()
            .chain(setups.iter())
            .find(|known| known.file.path.canonicalize().ok().as_ref() == Some(canonical))
    };
    for input in inputs.iter().chain(setups.iter()) {
        let mut external = HashSet::new();
        if let Ok(canonical) = input.file.path.canonicalize() {
            for (dependent, setup_canonical) in &setup_of {
                if *setup_canonical == canonical {
                    external.extend(setup_capture_uses(&inputs[*dependent].file));
                }
            }
        }
        for lint in lint_file_with(&input.file, &external) {
            diagnostics.push(Diagnostic::lint(&lint, &input.source));
            if lint.severity == Severity::Error {
                exit = exit.max(Exit::ParseLint);
            }
        }
    }
    for (dependent, setup_canonical) in &setup_of {
        let Some(setup) = find(setup_canonical) else {
            continue;
        };
        let input = &inputs[*dependent];
        for lint in lint_setup_refs(&input.file, &setup.file) {
            diagnostics.push(Diagnostic::lint(&lint, &input.source));
            if lint.severity == Severity::Error {
                exit = exit.max(Exit::ParseLint);
            }
        }
    }
    (CheckedInputs { inputs, setups }, exit)
}

/// `whirl check`: parse and lint only; nothing runs (SPEC 13).
fn check_command(paths: &[PathBuf], json: bool) -> Exit {
    let mut diagnostics = Vec::new();
    let exit = match expand_paths(paths) {
        Err(error) => {
            diagnostics.push(Diagnostic::environment(
                "input-selection",
                format!("whirl: error: {error}"),
            ));
            Exit::Usage
        }
        Ok(files) => match load_sources(&files) {
            Err(error) => {
                diagnostics.push(Diagnostic::environment(
                    "input-io",
                    format!("whirl: error: {error:#}"),
                ));
                Exit::Runtime
            }
            Ok(sources) => check_inputs(sources, &mut diagnostics).1,
        },
    };
    print_diagnostics(&diagnostics, json, exit);
    exit
}

/// Expands path arguments and reads every file, printing any error.
fn prepare_sources(paths: &[PathBuf]) -> Result<Vec<(PathBuf, String)>, Exit> {
    let files = expand_paths(paths).map_err(|error| {
        print_err(&format!("whirl: error: {error}"));
        Exit::Usage
    })?;
    load_sources(&files).map_err(|error| {
        print_err(&format!("whirl: error: {error:#}"));
        Exit::Runtime
    })
}

/// `whirl fmt [--check]`: rewrite files to the canonical form (SPEC 13).
fn fmt_command(check: bool, paths: &[PathBuf]) -> Exit {
    let sources = match prepare_sources(paths) {
        Ok(sources) => sources,
        Err(exit) => return exit,
    };
    let (parsed, parse_errors) = parse_inputs(sources);
    for error in &parse_errors {
        print_err(&error.render());
    }
    if !parse_errors.is_empty() {
        return Exit::ParseLint;
    }
    let mut exit = Exit::Success;
    for input in &parsed {
        let formatted = fmt::format_file(&input.file);
        if formatted == input.source.replace("\r\n", "\n") {
            continue;
        }
        let path = &input.file.path;
        if check {
            print_out(&format!("would reformat {}", path.display()));
            exit = exit.max(Exit::Negative);
        } else if let Err(error) = fs::write(path, &formatted) {
            print_err(&format!(
                "whirl: error: cannot write '{}': {error}",
                path.display()
            ));
            exit = exit.max(Exit::Runtime);
        } else {
            print_out(&format!("reformatted {}", path.display()));
        }
    }
    exit
}

/// Builds the command-line option overrides (SPEC 5, 13). A malformed
/// flag value is a usage error.
fn build_overrides(args: &RunArgs) -> Result<flow::Overrides, UsageError> {
    let duration = |flag: &'static str, value: &Option<String>| {
        value
            .as_deref()
            .map(|text| {
                text.parse::<ast::DurationLit>()
                    .map(ast::DurationLit::millis)
                    .map_err(|_| UsageError {
                        message: format!(
                            "invalid {flag} value '{text}': expected e.g. 500ms or 10s"
                        ),
                    })
            })
            .transpose()
    };
    let browser = args
        .browser
        .as_deref()
        .map(|text| {
            text.parse::<ast::BrowserKind>().map_err(|_| UsageError {
                message: format!(
                    "invalid --browser value '{text}': expected chromium, firefox, or webkit"
                ),
            })
        })
        .transpose()?;
    Ok(flow::Overrides {
        base: args.base.clone(),
        browser,
        step_timeout_ms: duration("--step-timeout", &args.step_timeout)?,
        entry_timeout_ms: duration("--entry-timeout", &args.entry_timeout)?,
        headed: args.headed,
        storage: args.storage.clone(),
        user_agent: args.user_agent.clone(),
    })
}

/// Loads `--variables-file` entries then `--var` flags, in order
/// (SPEC 11). Malformed entries are usage errors.
fn build_base_vars(args: &RunArgs) -> Result<Vec<(String, String)>, UsageError> {
    let mut entries = Vec::new();
    if let Some(path) = &args.variables_file {
        let source = fs::read_to_string(path).map_err(|error| UsageError {
            message: format!("cannot read variables file '{}': {error}", path.display()),
        })?;
        entries.extend(
            vars::parse_variables_file(&source).map_err(|error| UsageError {
                message: format!("variables file '{}': {error}", path.display()),
            })?,
        );
    }
    for flag in &args.var {
        entries.push(vars::parse_var_flag(flag).map_err(|error| UsageError {
            message: format!("--var {flag}: {error}"),
        })?);
    }
    Ok(entries)
}

/// Rejects `--save-storage` with more than one input flow before parsing,
/// so the usage error preempts parse errors (SPEC 13). Flows are counted
/// after canonical-path dedup (SPEC 14); a canonicalization failure is
/// left for the runner to report as a runtime error.
fn check_save_storage_inputs(
    args: &RunArgs,
    sources: &[(PathBuf, String)],
) -> Result<(), UsageError> {
    if args.save_storage.is_none() {
        return Ok(());
    }
    let inputs: Vec<PathBuf> = sources.iter().map(|(path, _)| path.clone()).collect();
    let Ok(flows) = artifacts::dedup_flows(&inputs) else {
        return Ok(());
    };
    if flows.len() > 1 {
        return Err(UsageError {
            message: format!(
                "--save-storage requires a single input file, got {}",
                flows.len()
            ),
        });
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RerunReport {
    version:           u32,
    working_directory: PathBuf,
    files:             Vec<RerunFile>,
}

#[derive(Deserialize)]
struct RerunFile {
    path:   PathBuf,
    status: Status,
}

fn failed_paths(path: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let report: RerunReport = serde_json::from_str(
        &fs::read_to_string(path)
            .with_context(|| format!("reading rerun report '{}'", path.display()))?,
    )
    .context("invalid rerun report; use a Whirl JSON report with workingDirectory metadata")?;
    anyhow::ensure!(
        report.version == 1,
        "unsupported report version {}",
        report.version
    );
    anyhow::ensure!(
        report.working_directory.is_absolute(),
        "report workingDirectory must be absolute"
    );
    Ok(report
        .files
        .into_iter()
        .filter(|file| matches!(file.status, Status::Failed | Status::Error))
        .map(|file| report.working_directory.join(file.path))
        .collect())
}

/// The default run command (SPEC 13): parse and lint everything, then
/// run the files through the worker pool and print the console report.
fn run_command(args: &RunArgs) -> Exit {
    // Usage errors are detected before parsing and preempt everything
    // (SPEC 13).
    let (overrides, base_vars) = match build_overrides(args)
        .and_then(|overrides| build_base_vars(args).map(|base_vars| (overrides, base_vars)))
    {
        Ok(prepared) => prepared,
        Err(error) => {
            print_err(&format!("whirl: error: {error}"));
            return Exit::Usage;
        }
    };
    let mut metadata = match args
        .report_metadata
        .as_deref()
        .map(ReportMetadata::load)
        .transpose()
    {
        Ok(metadata) => metadata,
        Err(error) => {
            print_err(&format!(
                "whirl: error: cannot load report metadata: {error:#}"
            ));
            return Exit::Usage;
        }
    };
    let mut selected = args.clone();
    if let Some(path) = &args.rerun_failed {
        match failed_paths(path) {
            Ok(paths) if paths.is_empty() => {
                print_out("No failed files to rerun.");
                return Exit::Success;
            }
            Ok(paths) => selected.paths = paths,
            Err(error) => {
                print_err(&format!("whirl: error: {error:#}"));
                return Exit::Usage;
            }
        }
    }
    let args = &selected;
    let sources = match prepare_sources(&args.paths) {
        Ok(sources) => sources,
        Err(exit) => return exit,
    };
    if let Err(error) = check_save_storage_inputs(args, &sources) {
        print_err(&format!("whirl: error: {error}"));
        return Exit::Usage;
    }
    if let Some(html_path) = &args.report_html {
        if let Err(error) = check_html_path(
            args,
            html_path,
            sources.iter().map(|(path, _)| path.as_path()),
        ) {
            print_err(&format!("whirl: error: {error:#}"));
            return Exit::Usage;
        }
    }
    let mut diagnostics = Vec::new();
    let (checked, exit) = check_inputs(sources, &mut diagnostics);
    print_diagnostics(&diagnostics, false, exit);
    if exit != Exit::Success {
        return exit;
    }
    let paths = || {
        checked
            .inputs
            .iter()
            .chain(&checked.setups)
            .map(|input| input.file.path.as_path())
    };
    if let Some(html_path) = &args.report_html {
        if let Err(error) = check_html_path(args, html_path, paths()) {
            print_err(&format!("whirl: error: {error:#}"));
            return Exit::Usage;
        }
    }
    if let Some(metadata) = &mut metadata {
        metadata.select(paths());
    }
    let settings = runner::RunSettings {
        source_hashes: checked
            .inputs
            .iter()
            .chain(&checked.setups)
            .map(|input| {
                (
                    input.file.path.clone(),
                    format!("{:x}", Sha256::digest(input.source.as_bytes())),
                )
            })
            .collect(),
        jobs: args.jobs,
        fail_fast: args.fail_fast,
        artifacts_dir: args.artifacts.clone(),
        flags: flow::FlowFlags {
            trace:            args.trace,
            video:            args.video,
            har:              args.har,
            update_snapshots: args.update_snapshots,
            save_storage:     args.save_storage.clone(),
        },
        overrides,
        base_vars,
    };
    let files: Vec<ast::File> = checked.inputs.into_iter().map(|input| input.file).collect();
    let setups: Vec<ast::File> = checked.setups.into_iter().map(|input| input.file).collect();
    let runtime = match Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            print_err(&format!("whirl: error: cannot start the runtime: {error}"));
            return Exit::Runtime;
        }
    };
    match runtime.block_on(runner::run_files(&files, &setups, &settings)) {
        Ok(report) => {
            print_out(console::render(&report).trim_end());
            let run_exit = if report.has_error() {
                Exit::Runtime
            } else if report.has_failure() {
                Exit::Negative
            } else {
                Exit::Success
            };
            let document = json::Document::new(report, metadata, args.video);
            let report_exit = write_reports(args, &document);
            run_exit.max(report_exit)
        }
        Err(error @ runner::RunnerError::SaveStorageManyFiles { .. }) => {
            print_err(&format!("whirl: error: {error}"));
            Exit::Usage
        }
        Err(error) => {
            print_err(&format!("whirl: error: {error}"));
            Exit::Runtime
        }
    }
}

/// Writes the requested JSON, JUnit, and HTML files
/// (SPEC 13, 14). Reports are written whenever a run happened, whatever
/// its outcome; a report that cannot be written is an environmental
/// failure (exit 3).
fn write_reports(args: &RunArgs, document: &json::Document) -> Exit {
    let mut exit = Exit::Success;
    if let Some(path) = &args.report_json {
        exit = exit.max(write_report_file(path, &document.render()));
    }
    if let Some(path) = &args.report_junit {
        exit = exit.max(write_report_file(path, &junit::render(&document.report)));
    }
    if let Some(path) = &args.report_html {
        if let Err(error) = html::write(path, document, &document.working_directory) {
            print_err(&format!(
                "whirl: error: cannot write HTML report '{}': {error:#}",
                path.display()
            ));
            exit = exit.max(Exit::Runtime);
        }
    }
    exit
}

/// Prevent the new report destination from replacing inputs or another output.
fn check_html_path<'a>(
    args: &'a RunArgs,
    html: &Path,
    inputs: impl Iterator<Item = &'a Path>,
) -> anyhow::Result<()> {
    check_report_destination(
        html,
        inputs.chain(
            [
                args.report_json.as_deref(),
                args.report_junit.as_deref(),
                args.report_metadata.as_deref(),
                args.variables_file.as_deref(),
                args.save_storage.as_deref(),
                args.storage.as_deref(),
                args.rerun_failed.as_deref(),
            ]
            .into_iter()
            .flatten(),
        ),
    )
}

fn check_report_destination<'a>(
    html: &Path,
    inputs: impl Iterator<Item = &'a Path>,
) -> anyhow::Result<()> {
    let identity = |path: &Path| {
        path.canonicalize().or_else(|_| {
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            Ok::<_, io::Error>(
                parent
                    .canonicalize()?
                    .join(path.file_name().unwrap_or_default()),
            )
        })
    };
    let Ok(target) = identity(html) else {
        return Ok(());
    };
    for path in inputs {
        anyhow::ensure!(
            identity(path).ok().as_ref() != Some(&target),
            "HTML report destination conflicts with '{}'",
            path.display()
        );
    }
    Ok(())
}

/// Saved results are data, never executable configuration.
fn report_command(args: &ReportArgs) -> Exit {
    use crate::report::aggregate;

    let prepared = (|| -> anyhow::Result<_> {
        check_report_destination(
            &args.html,
            args.reports
                .iter()
                .map(PathBuf::as_path)
                .chain(args.metadata.as_deref())
                .chain(args.expected.as_deref()),
        )?;
        let metadata = args
            .metadata
            .as_deref()
            .map(ReportMetadata::read_author)
            .transpose()?;
        let expected = args
            .expected
            .as_deref()
            .map(aggregate::read_expected)
            .transpose()?;
        let override_base = args
            .working_directory
            .as_deref()
            .map(Path::canonicalize)
            .transpose()
            .context("resolving artifact working directory")?;
        if let Some(base) = &override_base {
            anyhow::ensure!(
                base.is_dir(),
                "artifact working directory must be a directory"
            );
        }
        let mut inputs = Vec::new();
        for path in &args.reports {
            let document = json::Document::read(path)?;
            let base = override_base
                .as_ref()
                .unwrap_or(&document.working_directory)
                .clone();
            let sources: Vec<PathBuf> = document
                .report
                .files
                .iter()
                .flat_map(|file| {
                    [
                        document.working_directory.join(&file.path),
                        base.join(&file.path),
                    ]
                })
                .collect();
            check_report_destination(&args.html, sources.iter().map(PathBuf::as_path))?;
            inputs.push(aggregate::Input {
                path: path.clone(),
                document,
                base,
            });
        }
        Ok((inputs, expected, metadata))
    })();
    let (mut inputs, expected, metadata) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            print_err(&format!("whirl: error: {error:#}"));
            return Exit::Usage;
        }
    };
    let written = if inputs.len() == 1 && expected.is_none() {
        let input = &mut inputs[0];
        if let Some(mut metadata) = metadata {
            metadata.files.retain(|path, _| {
                input
                    .document
                    .report
                    .files
                    .iter()
                    .any(|file| file.path == *path)
            });
            input.document.metadata = Some(metadata);
        }
        html::write(&args.html, &input.document, &input.base)
    } else {
        let report = match aggregate::Report::new(inputs, expected, metadata) {
            Ok(report) => report,
            Err(error) => {
                print_err(&format!("whirl: error: {error:#}"));
                return Exit::Usage;
            }
        };
        html::aggregate::write(&args.html, &report)
    };
    match written {
        Ok(()) => Exit::Success,
        Err(error) => {
            print_err(&format!(
                "whirl: error: cannot write HTML report '{}': {error:#}",
                args.html.display()
            ));
            Exit::Runtime
        }
    }
}

/// Writes one report file, printing any error.
fn write_report_file(path: &Path, content: &str) -> Exit {
    match fs::write(path, content) {
        Ok(()) => Exit::Success,
        Err(error) => {
            print_err(&format!(
                "whirl: error: cannot write report '{}': {error}",
                path.display()
            ));
            Exit::Runtime
        }
    }
}

/// `whirl install`: provision the shim bundle and browsers (SPEC 13).
/// A failure is a runtime error (exit 3) naming the failing step.
fn install_command(browsers: &[String]) -> Exit {
    match install::run(browsers, &mut |line| print_out(line)) {
        Ok(()) => Exit::Success,
        Err(error) => {
            print_err(&format!("whirl: error: {error:#}"));
            Exit::Runtime
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::{env, iter, process, slice};

    use super::*;

    /// A unique temporary directory removed on drop.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!("whirl-cli-test-{}-{id}", process::id()));
            fs::create_dir_all(&path).expect("temp dir should be creatable");
            Self { path }
        }

        fn file(&self, name: &str, content: &str) -> PathBuf {
            let path = self.path.join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("parent dir should be creatable");
            }
            fs::write(&path, content).expect("temp file should be writable");
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn run_cli(args: &[&str]) -> u8 {
        let argv = iter::once("whirl")
            .chain(args.iter().copied())
            .map(OsString::from);
        execute(argv)
    }

    #[test]
    fn exit_precedence_orders_runtime_over_parse_over_negative() {
        assert_eq!(Exit::Success.max(Exit::Negative), Exit::Negative);
        assert_eq!(Exit::Negative.max(Exit::ParseLint), Exit::ParseLint);
        assert_eq!(Exit::ParseLint.max(Exit::Runtime), Exit::Runtime);
        assert_eq!(Exit::Runtime.max(Exit::Usage), Exit::Usage);
        assert_eq!(Exit::Usage.max(Exit::Success), Exit::Usage);
    }

    #[test]
    fn exit_codes_match_spec_13() {
        assert_eq!(Exit::Success.code(), 0);
        assert_eq!(Exit::Negative.code(), 1);
        assert_eq!(Exit::ParseLint.code(), 2);
        assert_eq!(Exit::Runtime.code(), 3);
        assert_eq!(Exit::Usage.code(), 4);
    }

    #[test]
    fn expands_a_file_argument_as_is() {
        let dir = TempDir::new();
        let file = dir.file("notes.txt", "not a whirl file");
        let files = expand_paths(slice::from_ref(&file)).expect("a plain file expands");
        assert_eq!(files, vec![file]);
    }

    #[test]
    fn expands_a_directory_to_sorted_whirl_files() {
        let dir = TempDir::new();
        let b = dir.file("b.whirl", "VISIT /\n");
        let a = dir.file("sub/a.whirl", "VISIT /\n");
        dir.file("ignored.txt", "not whirl");
        let files = expand_paths(slice::from_ref(&dir.path)).expect("a directory expands");
        assert_eq!(files, vec![b, a]);
    }

    #[test]
    fn rejects_a_nonexistent_path() {
        let dir = TempDir::new();
        let missing = dir.path.join("missing.whirl");
        let error = expand_paths(&[missing]).expect_err("a missing path is a usage error");
        assert!(
            error.message.contains("does not exist"),
            "message: {}",
            error.message
        );
    }

    #[test]
    fn check_passes_a_clean_file() {
        let dir = TempDir::new();
        let file = dir.file("clean.whirl", "VISIT /login\n");
        assert_eq!(run_cli(&["check", file.to_str().expect("utf-8 path")]), 0);
    }

    #[test]
    fn check_reports_a_parse_error_with_exit_2() {
        let dir = TempDir::new();
        let file = dir.file("broken.whirl", "BOGUS line\n");
        assert_eq!(run_cli(&["check", file.to_str().expect("utf-8 path")]), 2);
    }

    #[test]
    fn check_keeps_exit_0_for_a_lint_warning() {
        let dir = TempDir::new();
        let file = dir.file("warn.whirl", "VISIT /login\n[Captures]\nunused: url\n");
        assert_eq!(run_cli(&["check", file.to_str().expect("utf-8 path")]), 0);
    }

    #[test]
    fn check_maps_a_lint_error_to_exit_2() {
        let dir = TempDir::new();
        let file = dir.file(
            "dupe.whirl",
            "VISIT /login\nSCREENSHOT shot\nSCREENSHOT shot\n",
        );
        assert_eq!(run_cli(&["check", file.to_str().expect("utf-8 path")]), 2);
    }

    #[test]
    fn fmt_check_reports_drift_with_exit_1_and_writes_nothing() {
        let dir = TempDir::new();
        let file = dir.file("drift.whirl", "VISIT   /login\n");
        assert_eq!(
            run_cli(&["fmt", "--check", file.to_str().expect("utf-8 path")]),
            1
        );
        let content = fs::read_to_string(&file).expect("file should still be readable");
        assert_eq!(content, "VISIT   /login\n");
    }

    #[test]
    fn fmt_rewrites_a_drifted_file_and_exits_0() {
        let dir = TempDir::new();
        let file = dir.file("drift.whirl", "VISIT   /login\n");
        assert_eq!(run_cli(&["fmt", file.to_str().expect("utf-8 path")]), 0);
        let content = fs::read_to_string(&file).expect("file should still be readable");
        assert_eq!(content, "VISIT /login\n");
    }

    #[test]
    fn fmt_check_passes_a_canonical_file() {
        let dir = TempDir::new();
        let file = dir.file("clean.whirl", "VISIT /login\n");
        assert_eq!(
            run_cli(&["fmt", "--check", file.to_str().expect("utf-8 path")]),
            0
        );
    }

    #[test]
    fn a_missing_path_is_a_usage_error() {
        let dir = TempDir::new();
        let missing = dir.path.join("missing.whirl");
        assert_eq!(
            run_cli(&["check", missing.to_str().expect("utf-8 path")]),
            4
        );
    }

    #[test]
    fn no_arguments_is_a_usage_error() {
        assert_eq!(run_cli(&[]), 4);
    }

    #[test]
    fn an_unknown_flag_is_a_usage_error() {
        assert_eq!(run_cli(&["--frobnicate", "x.whirl"]), 4);
    }

    #[test]
    fn run_stops_on_parse_errors_before_the_runner() {
        let dir = TempDir::new();
        let file = dir.file("broken.whirl", "BOGUS line\n");
        assert_eq!(run_cli(&[file.to_str().expect("utf-8 path")]), 2);
    }

    #[test]
    fn an_invalid_browser_flag_is_a_usage_error() {
        let dir = TempDir::new();
        let file = dir.file("clean.whirl", "VISIT /login\n");
        assert_eq!(
            run_cli(&["--browser", "netscape", file.to_str().expect("utf-8 path")]),
            4
        );
    }

    #[test]
    fn an_invalid_step_timeout_flag_is_a_usage_error() {
        let dir = TempDir::new();
        let file = dir.file("clean.whirl", "VISIT /login\n");
        assert_eq!(
            run_cli(&["--step-timeout", "soon", file.to_str().expect("utf-8 path")]),
            4
        );
    }

    #[test]
    fn a_malformed_var_flag_is_a_usage_error() {
        let dir = TempDir::new();
        let file = dir.file("broken.whirl", "BOGUS line\n");
        // The usage error preempts the parse error (SPEC 13).
        assert_eq!(
            run_cli(&["--var", "novalue", file.to_str().expect("utf-8 path")]),
            4
        );
    }
}
