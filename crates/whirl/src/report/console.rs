//! Console output (SPEC 14): one line per file with pass/fail and
//! duration, then a failure detail block per failed entry. All model
//! strings are already secret-masked by the runner.

use std::fmt::Write as _;

use crate::report::model::{EntryReport, FileReport, RunReport, Status, StepReport};

/// A duration rendered for people: `0.3s`, `12.4s`.
fn duration_text(ms: u64) -> String {
    #[expect(clippy::cast_precision_loss, reason = "display only")]
    let seconds = ms as f64 / 1000.0;
    format!("{seconds:.1}s")
}

fn status_text(status: Status) -> &'static str {
    match status {
        Status::Passed => "passed",
        Status::Failed => "FAILED",
        Status::Error => "ERROR",
        Status::Skipped => "skipped",
    }
}

/// Renders the whole console report: the per-file summary lines,
/// warnings, and the failure detail blocks.
pub(crate) fn render(report: &RunReport) -> String {
    let mut out = String::new();
    for file in &report.files {
        let _ = writeln!(
            out,
            "{path} {status} ({duration})",
            path = file.path,
            status = status_text(file.status),
            duration = duration_text(file.duration_ms)
        );
        for warning in &file.warnings {
            let _ = writeln!(out, "  warning: {warning}");
        }
        // The reports list every blocked host (SPEC 5).
        for host in &file.blocked_hosts {
            let _ = writeln!(out, "  blocked host: {host}");
        }
    }
    for file in &report.files {
        for entry in &file.entries {
            if entry.status != Status::Failed && entry.status != Status::Error {
                continue;
            }
            out.push('\n');
            render_entry_failure(&mut out, file, entry);
        }
    }
    out
}

/// The failing (or errored) step of an entry, if any.
fn failing_step(entry: &EntryReport) -> Option<&StepReport> {
    entry
        .steps
        .iter()
        .find(|step| step.status == Status::Failed || step.status == Status::Error)
}

/// Renders one failure detail block (SPEC 14): file, line, the failing
/// step, expected versus actual, and the artifact paths.
fn render_entry_failure(out: &mut String, file: &FileReport, entry: &EntryReport) {
    let step = failing_step(entry);
    let line = step.map_or(entry.line, |step| step.line);
    let _ = writeln!(
        out,
        "{status}: {path}:{line}: {name}",
        status = status_text(entry.status),
        path = file.path,
        name = entry.name
    );
    if let Some(step) = step {
        let _ = writeln!(out, "  step: {}", step.text.replace('\n', "\n        "));
        if let Some(error) = &step.error {
            let _ = writeln!(out, "  error: {}", error.message.replace('\n', "\n    "));
            if let Some(expected) = &error.expected {
                let _ = writeln!(out, "  expected: {expected}");
            }
            if let Some(actual) = &error.actual {
                let _ = writeln!(out, "  actual: {actual}");
            }
            for candidate in error.candidates.iter().flatten() {
                let _ = writeln!(out, "  candidate: {candidate}");
            }
        }
    }
    for artifact in &entry.artifacts {
        let _ = writeln!(out, "  artifact: {artifact}");
        if artifact.ends_with("/trace.zip") {
            let quoted = artifact.replace('\'', "'\"'\"'");
            let _ = writeln!(out, "  open trace: whirl show-trace -- '{quoted}'");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::model::{StepError, StepKind, Timing};

    fn step(line: u32, text: &str, status: Status, error: Option<StepError>) -> StepReport {
        StepReport {
            line,
            kind: StepKind::Assert,
            text: text.to_owned(),
            status,
            duration_ms: 5,
            error,
        }
    }

    fn sample_report() -> RunReport {
        RunReport {
            timing:      Timing::default(),
            duration_ms: 2_000,
            files:       vec![
                FileReport {
                    timing:        Timing::default(),
                    source_sha256: None,
                    roles:         None,
                    runtime:       None,
                    path:          "flows/pass.whirl".to_owned(),
                    status:        Status::Passed,
                    duration_ms:   1_234,
                    artifacts_dir: "whirl-artifacts/flows/pass".to_owned(),
                    blocked_hosts: vec!["cdn.example.com".to_owned()],
                    warnings:      vec!["SCREENSHOT shot skipped: page crashed".to_owned()],
                    artifacts:     Vec::new(),
                    entries:       Vec::new(),
                },
                FileReport {
                    timing:        Timing::default(),
                    source_sha256: None,
                    roles:         None,
                    runtime:       None,
                    path:          "flows/fail.whirl".to_owned(),
                    status:        Status::Failed,
                    duration_ms:   700,
                    artifacts_dir: "whirl-artifacts/flows/fail".to_owned(),
                    blocked_hosts: Vec::new(),
                    warnings:      Vec::new(),
                    artifacts:     Vec::new(),
                    entries:       vec![EntryReport {
                        name:        "Log in.".to_owned(),
                        line:        3,
                        status:      Status::Failed,
                        duration_ms: 700,
                        steps:       vec![
                            step(3, "VISIT /login", Status::Passed, None),
                            step(
                                5,
                                "title == Welcome",
                                Status::Failed,
                                Some(StepError {
                                    message: "assert: title mismatch".to_owned(),
                                    expected: Some("Welcome".to_owned()),
                                    actual: Some("Login".to_owned()),
                                    ..StepError::default()
                                }),
                            ),
                            step(6, "url == /home", Status::Skipped, None),
                        ],
                        captures:    Vec::new(),
                        artifacts:   vec!["whirl-artifacts/flows/fail/failure.png".to_owned()],
                    }],
                },
            ],
        }
    }

    #[test]
    fn renders_a_summary_line_per_file_with_warnings() {
        let out = render(&sample_report());
        assert!(
            out.contains("flows/pass.whirl passed (1.2s)"),
            "out:\n{out}"
        );
        assert!(
            out.contains("flows/fail.whirl FAILED (0.7s)"),
            "out:\n{out}"
        );
        assert!(
            out.contains("warning: SCREENSHOT shot skipped: page crashed"),
            "out:\n{out}"
        );
        assert!(out.contains("blocked host: cdn.example.com"), "out:\n{out}");
    }

    #[test]
    fn renders_the_failure_detail_block_from_the_failing_step() {
        let out = render(&sample_report());
        assert!(
            out.contains("FAILED: flows/fail.whirl:5: Log in."),
            "out:\n{out}"
        );
        assert!(out.contains("step: title == Welcome"), "out:\n{out}");
        assert!(out.contains("expected: Welcome"), "out:\n{out}");
        assert!(out.contains("actual: Login"), "out:\n{out}");
        assert!(
            out.contains("artifact: whirl-artifacts/flows/fail/failure.png"),
            "out:\n{out}"
        );
    }

    #[test]
    fn indents_multiline_step_text() {
        let mut report = sample_report();
        report.files[1].entries[0].steps[1].text =
            "HTTP POST /fixtures\nContent-Type: application/json\n{\n  \"name\": \"Ada\"\n}"
                .to_owned();

        let out = render(&report);

        assert!(
            out.contains(
                "step: HTTP POST /fixtures\n        Content-Type: application/json\n        {\n          \"name\": \"Ada\"\n        }"
            ),
            "out:\n{out}"
        );
    }

    #[test]
    fn the_report_rolls_up_failures_and_errors() {
        let report = sample_report();
        assert!(report.has_failure());
        assert!(!report.has_error());
    }
}
