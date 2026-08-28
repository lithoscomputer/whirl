//! The JUnit XML report (SPEC 14): one test suite per file, one test
//! case per entry (named by the entry display name), and the synthetic
//! `[setup]` case for failures before the first entry. Rendered from the
//! shared [`RunReport`] model, whose strings are already secret-masked.

use std::fmt::Write as _;
use std::time::Duration;

use quick_junit::{NonSuccessKind, Report, TestCase, TestCaseStatus, TestSuite};

use crate::report::model::{EntryReport, RunReport, Status, StepReport};

/// Renders the whole run as a JUnit XML document.
pub fn render(report: &RunReport) -> String {
    let mut junit = Report::new("whirl");
    junit.set_time(Duration::from_millis(report.duration_ms));
    for file in &report.files {
        let mut suite = TestSuite::new(file.path.clone());
        suite.set_time(Duration::from_millis(file.duration_ms));
        for entry in &file.entries {
            suite.add_test_case(test_case(entry));
        }
        // Screenshot skip warnings must reach both reports (SPEC 7), so
        // the suite carries them as its <system-out> text.
        if !file.warnings.is_empty() {
            let lines: Vec<String> = file
                .warnings
                .iter()
                .map(|warning| format!("warning: {warning}"))
                .collect();
            suite.set_system_out(lines.join("\n"));
        }
        junit.add_test_suite(suite);
    }
    junit
        .to_string()
        .expect("the report model should always serialize to JUnit XML")
}

/// Maps one entry to its test case.
fn test_case(entry: &EntryReport) -> TestCase {
    let status = match entry.status {
        Status::Passed => TestCaseStatus::success(),
        Status::Skipped => TestCaseStatus::skipped(),
        // A runtime error is an <error>; any other failure — including a
        // non-runtime `[setup]` failure — is a <failure> (SPEC 14).
        Status::Failed => non_success(entry, NonSuccessKind::Failure),
        Status::Error => non_success(entry, NonSuccessKind::Error),
    };
    let mut case = TestCase::new(entry.name.clone(), status);
    case.set_time(Duration::from_millis(entry.duration_ms));
    case
}

/// Builds a `<failure>` or `<error>` status carrying the failing step's
/// text and its expected-versus-actual detail.
fn non_success(entry: &EntryReport, kind: NonSuccessKind) -> TestCaseStatus {
    let mut status = TestCaseStatus::non_success(kind);
    let Some(step) = failing_step(entry) else {
        return status;
    };
    let message = match &step.error {
        Some(error) => format!("{}: {}", step.text, error.message),
        None => step.text.clone(),
    };
    status.set_message(message);
    status.set_description(describe_step(step));
    status
}

/// The failing (or errored) step of an entry, if any.
fn failing_step(entry: &EntryReport) -> Option<&StepReport> {
    entry
        .steps
        .iter()
        .find(|step| step.status == Status::Failed || step.status == Status::Error)
}

/// The failure body: the step, its line, and expected versus actual.
fn describe_step(step: &StepReport) -> String {
    let mut body = format!("step: {} (line {})\n", step.text, step.line);
    if let Some(error) = &step.error {
        let _ = writeln!(body, "error: {}", error.message);
        if let Some(expected) = &error.expected {
            let _ = writeln!(body, "expected: {expected}");
        }
        if let Some(actual) = &error.actual {
            let _ = writeln!(body, "actual: {actual}");
        }
        for candidate in error.candidates.iter().flatten() {
            let _ = writeln!(body, "candidate: {candidate}");
        }
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::fixture::{SECRET, sample_report};

    fn rendered() -> String {
        render(&sample_report())
    }

    #[test]
    fn each_file_is_a_suite_and_each_entry_a_case_with_timing() {
        let xml = rendered();
        assert!(
            xml.contains("<testsuite name=\"flows/pass.whirl\""),
            "xml:\n{xml}"
        );
        assert!(
            xml.contains("<testsuite name=\"flows/fail.whirl\""),
            "xml:\n{xml}"
        );
        assert!(xml.contains("<testcase name=\"Log in.\""), "xml:\n{xml}");
        assert!(xml.contains("time=\"1.200\""), "xml:\n{xml}");
        assert!(xml.contains("time=\"0.900\""), "xml:\n{xml}");
    }

    #[test]
    fn a_failed_entry_reports_a_failure_with_the_step_detail() {
        let xml = rendered();
        assert!(
            xml.contains(
                "<failure message=\"label:Password value == expected: assert: value mismatch\""
            ),
            "xml:\n{xml}"
        );
        assert!(
            xml.contains("step: label:Password value == expected (line 4)"),
            "xml:\n{xml}"
        );
        assert!(xml.contains("expected: expected"), "xml:\n{xml}");
        assert!(xml.contains("actual: ***"), "xml:\n{xml}");
    }

    #[test]
    fn a_skipped_entry_reports_as_skipped() {
        let xml = rendered();
        assert!(
            xml.contains("<testcase name=\"Never reached.\""),
            "xml:\n{xml}"
        );
        assert!(xml.contains("<skipped"), "xml:\n{xml}");
    }

    #[test]
    fn setup_cases_use_failure_or_error_by_status() {
        let xml = rendered();
        assert!(xml.contains("<testcase name=\"[setup]\""), "xml:\n{xml}");
        // The non-runtime setup failure is a <failure>; the runtime
        // error is an <error> carrying its message.
        assert!(xml.contains("<failure/>"), "xml:\n{xml}");
        assert!(
            xml.contains("<error message=\"browser launch: the shim process died\""),
            "xml:\n{xml}"
        );
    }

    #[test]
    fn screenshot_warnings_appear_as_suite_system_out() {
        let xml = rendered();
        assert!(
            xml.contains(
                "<system-out>warning: SCREENSHOT overview skipped: page crashed</system-out>"
            ),
            "xml:\n{xml}"
        );
    }

    #[test]
    fn the_env_sourced_secret_never_appears() {
        let xml = rendered();
        assert!(!xml.contains(SECRET), "xml:\n{xml}");
    }
}
