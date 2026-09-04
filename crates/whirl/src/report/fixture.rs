//! A shared [`RunReport`] fixture for reporter tests: passed, failed,
//! skipped, setup-failure, and runtime-error cases, with an env-sourced
//! secret masked the way the runner masks it.

use crate::report::model::{
    EntryReport, FileReport, RunReport, SETUP_ENTRY, Status, StepError, StepKind, StepReport,
};
use crate::run::vars::Masker;

/// The raw secret; it must never appear in any rendered report.
pub(crate) const SECRET: &str = "hunter2";

fn step(
    line: u32,
    kind: StepKind,
    text: &str,
    status: Status,
    error: Option<StepError>,
) -> StepReport {
    StepReport {
        line,
        kind,
        text: text.to_owned(),
        status,
        duration_ms: 5,
        error,
    }
}

/// Builds the fixture report. Strings that carried the secret are
/// pre-masked, as the runner would deliver them.
pub(crate) fn sample_report() -> RunReport {
    let mut masker = Masker::default();
    masker.record(SECRET);
    let fill_text = masker.mask(&format!("FILL \"Password\" {SECRET}"));
    let actual_value = masker.mask(SECRET);
    RunReport {
        duration_ms: 3_210,
        files:       vec![
            passed_file(),
            failed_file(&fill_text, &actual_value),
            setup_failed_file(),
            error_file(),
        ],
    }
}

fn passed_file() -> FileReport {
    FileReport {
        runtime:       None,
        path:          "flows/pass.whirl".to_owned(),
        status:        Status::Passed,
        duration_ms:   1_200,
        artifacts_dir: "whirl-artifacts/flows/pass".to_owned(),
        blocked_hosts: vec!["cdn.example.com".to_owned()],
        warnings:      vec!["SCREENSHOT overview skipped: page crashed".to_owned()],
        artifacts:     vec!["whirl-artifacts/flows/pass/video.webm".to_owned()],
        entries:       vec![EntryReport {
            name:        "Log in.".to_owned(),
            line:        2,
            status:      Status::Passed,
            duration_ms: 900,
            steps:       vec![
                step(2, StepKind::Action, "VISIT /login", Status::Passed, None),
                step(3, StepKind::Page, "PAGE /dashboard", Status::Passed, None),
            ],
            captures:    vec![("next_url".to_owned(), "/dashboard".to_owned())],
            artifacts:   Vec::new(),
        }],
    }
}

fn failed_file(fill_text: &str, actual_value: &str) -> FileReport {
    FileReport {
        runtime:       None,
        path:          "flows/fail.whirl".to_owned(),
        status:        Status::Failed,
        duration_ms:   800,
        artifacts_dir: "whirl-artifacts/flows/fail".to_owned(),
        blocked_hosts: Vec::new(),
        warnings:      Vec::new(),
        artifacts:     Vec::new(),
        entries:       vec![
            EntryReport {
                name:        "Fill the form.".to_owned(),
                line:        2,
                status:      Status::Failed,
                duration_ms: 700,
                steps:       vec![
                    step(2, StepKind::Action, fill_text, Status::Passed, None),
                    step(
                        4,
                        StepKind::Assert,
                        "label:Password value == expected",
                        Status::Failed,
                        Some(StepError {
                            code:       "assert".to_owned(),
                            message:    "assert: value mismatch".to_owned(),
                            expected:   Some("expected".to_owned()),
                            actual:     Some(actual_value.to_owned()),
                            candidates: None,
                        }),
                    ),
                ],
                captures:    Vec::new(),
                artifacts:   vec!["whirl-artifacts/flows/fail/failure.png".to_owned()],
            },
            EntryReport {
                name:        "Never reached.".to_owned(),
                line:        7,
                status:      Status::Skipped,
                duration_ms: 0,
                steps:       vec![step(
                    7,
                    StepKind::Action,
                    "SCREENSHOT after",
                    Status::Skipped,
                    None,
                )],
                captures:    Vec::new(),
                artifacts:   Vec::new(),
            },
        ],
    }
}

fn setup_failed_file() -> FileReport {
    FileReport {
        runtime:       None,
        path:          "flows/setup.whirl".to_owned(),
        status:        Status::Failed,
        duration_ms:   10,
        artifacts_dir: "whirl-artifacts/flows/setup".to_owned(),
        blocked_hosts: Vec::new(),
        warnings:      Vec::new(),
        artifacts:     Vec::new(),
        entries:       vec![EntryReport {
            name:        SETUP_ENTRY.to_owned(),
            line:        0,
            status:      Status::Failed,
            duration_ms: 10,
            steps:       Vec::new(),
            captures:    Vec::new(),
            artifacts:   Vec::new(),
        }],
    }
}

fn error_file() -> FileReport {
    FileReport {
        runtime:       None,
        path:          "flows/crash.whirl".to_owned(),
        status:        Status::Error,
        duration_ms:   50,
        artifacts_dir: "whirl-artifacts/flows/crash".to_owned(),
        blocked_hosts: Vec::new(),
        warnings:      Vec::new(),
        artifacts:     Vec::new(),
        entries:       vec![EntryReport {
            name:        SETUP_ENTRY.to_owned(),
            line:        0,
            status:      Status::Error,
            duration_ms: 50,
            steps:       vec![step(
                0,
                StepKind::Action,
                "browser launch",
                Status::Error,
                Some(StepError {
                    code:       "shim-crash".to_owned(),
                    message:    "the shim process died".to_owned(),
                    expected:   None,
                    actual:     None,
                    candidates: None,
                }),
            )],
            captures:    Vec::new(),
            artifacts:   Vec::new(),
        }],
    }
}
