//! A coverage view whose selected attempts remain attached to their source
//! runs.

use std::io;
use std::path::Path;

use super::{badge, escape, render_details, render_file, render_head, render_timing, status_text};
use crate::report::aggregate::{Report, Selection};
use crate::report::metadata::FileMetadata;
use crate::report::model::Status;

pub(crate) fn write(path: &Path, report: &Report) -> anyhow::Result<()> {
    for input in &report.inputs {
        super::check_artifact_destination(path, &input.document, &input.base)?;
    }
    super::write_atomic(path, |output| render(output, report))
}

fn render(output: &mut impl io::Write, report: &Report) -> io::Result<()> {
    let title = report
        .metadata
        .title
        .as_deref()
        .unwrap_or("Browser test evidence");
    render_head(output, title)?;
    write!(
        output,
        "<header><p class=\"eyebrow\">Whirl / Browser verification</p><h1>{}</h1>",
        escape(title)
    )?;
    if report.inputs.len() > 1 {
        write!(
            output,
            "<p>Combined evidence from {} saved reports.</p><p>Results from multiple runs do not establish one complete suite pass.</p>",
            report.inputs.len()
        )?;
    } else {
        write!(output, "<p>Scenario coverage from one saved report.</p>")?;
    }
    write!(
        output,
        "<p>Each scenario shows its latest recorded attempt. Setup and other flows are listed separately.</p></header>"
    )?;
    if let Some(description) = &report.metadata.description {
        write!(
            output,
            "<section class=\"scope\" aria-label=\"Author-provided context\"><h2>Test scope</h2><p>{}</p></section>",
            escape(description)
        )?;
    }
    render_details(output, &report.metadata)?;
    write!(
        output,
        "<div class=\"summary-container\"><dl class=\"totals\" aria-label=\"Scenario totals\">"
    )?;
    for status in [
        Status::Passed,
        Status::Failed,
        Status::Error,
        Status::Skipped,
    ] {
        let count = report
            .scenarios
            .iter()
            .filter(|scenario| {
                scenario
                    .selected
                    .is_some_and(|selection| report.get(selection).1.status == status)
            })
            .count();
        let (class, label) = status_text(status);
        write!(
            output,
            "<div class=\"{class}\"><dt>{label}</dt><dd>{count}</dd></div>"
        )?;
    }
    let missing = report
        .scenarios
        .iter()
        .filter(|scenario| scenario.selected.is_none())
        .count();
    write!(
        output,
        "<div><dt>Not run</dt><dd>{missing}</dd></div></dl></div>"
    )?;
    write!(
        output,
        "<details class=\"contents\" open><summary>Scenarios</summary><nav aria-label=\"Scenarios\"><ol role=\"list\">"
    )?;
    for (index, scenario) in report.scenarios.iter().enumerate() {
        let context = context(report, &scenario.path, scenario.selected);
        let title = context
            .and_then(|file| file.title.as_deref())
            .unwrap_or(&scenario.path);
        let badge = scenario.selected.map_or_else(
            || "<span class=\"badge\">Not run</span>".to_owned(),
            |selection| badge(report.get(selection).1.status),
        );
        write!(
            output,
            "<li><a href=\"#flow-{index}\">{}</a>{badge}</li>",
            escape(title)
        )?;
    }
    write!(
        output,
        "</ol></nav></details><section aria-label=\"Scenarios\">"
    )?;
    for (index, scenario) in report.scenarios.iter().enumerate() {
        if let Some(selection) = scenario.selected {
            render_selected(output, report, selection, index)?;
        } else {
            let context = context(report, &scenario.path, None);
            let title = context
                .and_then(|file| file.title.as_deref())
                .unwrap_or(&scenario.path);
            write!(
                output,
                "<article id=\"flow-{index}\" data-status=\"not-run\"><div class=\"flow-heading\"><div><p class=\"eyebrow\">Scenario {:02}</p><h2>{}</h2>",
                index + 1,
                escape(title)
            )?;
            if let Some(description) = context.and_then(|file| file.description.as_deref()) {
                write!(
                    output,
                    "<p class=\"description\">{}</p>",
                    escape(description)
                )?;
            }
            write!(
                output,
                "<p class=\"file-path\"><code>{}</code></p></div><span class=\"badge\">Not run</span></div><p class=\"media-note\">No requested attempt in the supplied reports.</p></article>",
                escape(&scenario.path)
            )?;
        }
    }
    write!(output, "</section>")?;
    let mut index = report.scenarios.len();
    for (title, selections) in [
        ("Setup attempts", &report.setups),
        ("Other flows", &report.other),
    ] {
        if !selections.is_empty() {
            write!(
                output,
                "<section aria-label=\"{title}\"><h2>{title}</h2><p>{} recorded attempts. Excluded from scenario totals.</p>",
                selections.len()
            )?;
            for &selection in selections {
                render_selected(output, report, selection, index)?;
                index += 1;
            }
            write!(output, "</section>")?;
        }
    }
    write!(
        output,
        "<section aria-label=\"Source reports\"><h2>Source reports</h2>"
    )?;
    for (index, input) in report.inputs.iter().enumerate() {
        let document = &input.document;
        write!(
            output,
            "<details id=\"source-{index}\" class=\"runtime\"><summary>Source {} · {}</summary><p class=\"file-path\">{}</p><p>Whirl {} · {} / {}</p><dl class=\"run-record\">",
            index + 1,
            escape(&input.path.to_string_lossy()),
            escape(&document.working_directory.to_string_lossy()),
            escape(&document.whirl_version),
            escape(&document.platform),
            escape(&document.architecture)
        )?;
        render_timing(output, &document.report.timing)?;
        write!(output, "</dl>")?;
        if let Some(metadata) = &document.metadata {
            if let Some(title) = &metadata.title {
                write!(output, "<h3>{}</h3>", escape(title))?;
            }
            if let Some(description) = &metadata.description {
                write!(
                    output,
                    "<p class=\"description\">{}</p>",
                    escape(description)
                )?;
            }
            render_details(output, metadata)?;
        }
        write!(output, "</details>")?;
    }
    writeln!(
        output,
        "</section><footer><p>Test status is independent of recording availability.</p><a href=\"#top\">Back to top</a></footer></main></body></html>"
    )
}

fn context<'a>(
    report: &'a Report,
    path: &str,
    selected: Option<Selection>,
) -> Option<&'a FileMetadata> {
    report.metadata.files.get(path).or_else(|| {
        let (input, _) = report.get(selected?);
        input.document.metadata.as_ref()?.files.get(path)
    })
}

fn render_selected(
    output: &mut impl io::Write,
    report: &Report,
    selection: Selection,
    index: usize,
) -> io::Result<()> {
    let (input, file) = report.get(selection);
    render_file(
        output,
        index,
        file,
        context(report, &file.path, Some(selection)),
        input.document.video_requested,
        &input.base,
        Some((selection.input, &input.path)),
    )
}
