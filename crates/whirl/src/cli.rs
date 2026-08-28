//! Command-line surface (SPEC section 13): argument parsing, path
//! expansion, and central exit-code handling.
//!
//! [`run`] is the binary's whole entry point. Exit codes follow SPEC 13:
//! a usage error (4) preempts everything, and within an invocation a
//! runtime error (3) outranks parse or lint errors (2), which outrank the
//! command's negative result (1), which outranks success (0).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::{fs, io};

use anyhow::Context as _;
use clap::error::ErrorKind;
use clap::{Args, Parser, Subcommand};

use crate::lang::lint::{Lint, Severity, lint_file};
use crate::lang::parse::{ParseError, parse_file};
use crate::lang::{ast, fmt};

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
    /// Parse and lint files; nothing runs.
    Check {
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
    Install,
}

/// Flags for the default run command (SPEC 13). The runner is a later
/// phase; the flags are accepted now so the surface is stable.
#[derive(Debug, Args)]
struct RunArgs {
    /// Files to run; directories recurse to *.whirl.
    #[arg(required = true, value_name = "PATH")]
    paths: Vec<PathBuf>,

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
}

/// Runs the CLI for the given argv (including the program name) and
/// returns the process exit code.
pub fn run(argv: impl IntoIterator<Item = OsString>) -> ExitCode {
    ExitCode::from(execute(argv))
}

fn execute(argv: impl IntoIterator<Item = OsString>) -> u8 {
    let cli = match Cli::try_parse_from(argv) {
        Ok(cli) => cli,
        Err(error) => return exit_for_clap_error(&error),
    };
    let exit = match cli.command {
        Some(Command::Check { paths }) => check_command(&paths),
        Some(Command::Fmt { check, paths }) => fmt_command(check, &paths),
        Some(Command::Install) => install_command(),
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

/// Parses and lints every input, printing all diagnostics. Returns the
/// parsed files and the worst outcome: [`Exit::ParseLint`] when any parse
/// error or lint error was found, [`Exit::Success`] otherwise (lint
/// warnings never change the exit code, SPEC 16).
fn check_inputs(sources: Vec<(PathBuf, String)>) -> (Vec<ParsedInput>, Exit) {
    let (parsed, parse_errors) = parse_inputs(sources);
    for error in &parse_errors {
        print_err(&error.render());
    }
    let mut exit = if parse_errors.is_empty() {
        Exit::Success
    } else {
        Exit::ParseLint
    };
    for input in &parsed {
        for lint in lint_file(&input.file) {
            print_err(&render_lint(&lint, &input.source));
            if lint.severity == Severity::Error {
                exit = exit.max(Exit::ParseLint);
            }
        }
    }
    (parsed, exit)
}

/// `whirl check`: parse and lint only; nothing runs (SPEC 13).
fn check_command(paths: &[PathBuf]) -> Exit {
    match prepare_sources(paths) {
        Ok(sources) => check_inputs(sources).1,
        Err(exit) => exit,
    }
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

/// The default run command. This phase parses and lints everything (SPEC
/// 13: nothing runs when any file has a parse or lint error); the runner
/// itself is a later phase.
fn run_command(args: &RunArgs) -> Exit {
    let sources = match prepare_sources(&args.paths) {
        Ok(sources) => sources,
        Err(exit) => return exit,
    };
    let (_parsed, exit) = check_inputs(sources);
    if exit != Exit::Success {
        return exit;
    }
    print_err("whirl: error: the runner is not implemented yet");
    Exit::Runtime
}

/// `whirl install`: provisioning is a later phase.
fn install_command() -> Exit {
    print_err("whirl: error: install is not implemented yet");
    Exit::Runtime
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
    fn run_reports_the_unimplemented_runner_with_exit_3() {
        let dir = TempDir::new();
        let file = dir.file("clean.whirl", "VISIT /login\n");
        assert_eq!(run_cli(&["--trace", file.to_str().expect("utf-8 path")]), 3);
    }

    #[test]
    fn install_is_not_implemented_yet() {
        assert_eq!(run_cli(&["install"]), 3);
    }
}
