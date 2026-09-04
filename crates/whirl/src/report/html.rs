//! Portable HTML reports built from the same masked results as JSON and JUnit.

use std::env;
use std::fs::File;
use std::io::{self, BufWriter, Read as _, Seek as _, Write as _};
use std::path::Path;

use anyhow::Context as _;
use base64::engine::general_purpose::STANDARD;
use base64::write::EncoderWriter;
use tempfile::NamedTempFile;

use crate::report::metadata::ReportMetadata;
use crate::report::model::{EntryReport, FileReport, RunReport, Status, StepReport};

const STYLE: &str = include_str!("html.css");

/// Stream media to a sibling temporary file, then atomically replace the
/// report. Missing or invalid media is shown as unavailable; output errors fail
/// the write.
pub(crate) fn write(
    path: &Path,
    report: &RunReport,
    metadata: &ReportMetadata,
    video_requested: bool,
) -> anyhow::Result<()> {
    if let Ok(target) = path.canonicalize() {
        for artifact in report.files.iter().flat_map(|file| {
            file.artifacts
                .iter()
                .chain(file.entries.iter().flat_map(|entry| &entry.artifacts))
        }) {
            anyhow::ensure!(
                Path::new(artifact).canonicalize().ok().as_ref() != Some(&target),
                "HTML report destination conflicts with artifact '{artifact}'"
            );
        }
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary = NamedTempFile::new_in(parent).context("creating temporary HTML report")?;
    {
        let mut output = BufWriter::new(temporary.as_file_mut());
        render(&mut output, report, metadata, video_requested).context("writing HTML report")?;
        output.flush().context("flushing HTML report")?;
    }
    temporary.persist(path).context("saving HTML report")?;
    Ok(())
}

fn render(
    output: &mut impl io::Write,
    report: &RunReport,
    metadata: &ReportMetadata,
    video_requested: bool,
) -> io::Result<()> {
    let title = metadata.title.as_deref().unwrap_or("Browser test report");
    write!(
        output,
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
        <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
        <meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; \
        style-src 'unsafe-inline'; img-src data:; media-src data:; base-uri 'none'; form-action 'none'\">\
        <title>{}</title><style>{STYLE}</style></head><body><main id=\"top\">",
        escape(title)
    )?;
    write!(
        output,
        "<header><p class=\"eyebrow\">Whirl / Browser verification</p><h1>{}</h1>\
        <p>Recorded results from {} flow files.</p></header>",
        escape(title),
        report.files.len()
    )?;
    if let Some(description) = &metadata.description {
        write!(
            output,
            "<section class=\"scope\" aria-label=\"Author-provided context\">\
            <h2>Test scope</h2><p>{}</p></section>",
            escape(description)
        )?;
    }
    write!(
        output,
        "<div class=\"summary-container\"><dl class=\"totals\">"
    )?;
    for status in [
        Status::Passed,
        Status::Failed,
        Status::Error,
        Status::Skipped,
    ] {
        let count = report
            .files
            .iter()
            .filter(|file| file.status == status)
            .count();
        let (class, label) = status_text(status);
        write!(
            output,
            "<div class=\"{class}\"><dt>{label}</dt><dd>{count}</dd></div>"
        )?;
    }
    write!(
        output,
        "<div><dt>Run time</dt><dd>{}</dd></div></dl></div>",
        duration(report.duration_ms)
    )?;
    write!(
        output,
        "<details class=\"contents\" open><summary>Flow files</summary>\
        <nav aria-label=\"Flow files\"><ol role=\"list\">"
    )?;
    for (index, file) in report.files.iter().enumerate() {
        let title = metadata
            .files
            .get(&file.path)
            .and_then(|file| file.title.as_deref())
            .unwrap_or(&file.path);
        write!(
            output,
            "<li><a href=\"#flow-{index}\">{}</a>{}</li>",
            escape(title),
            badge(file.status)
        )?;
    }
    write!(output, "</ol></nav></details>")?;
    for (index, file) in report.files.iter().enumerate() {
        render_file(output, index, file, metadata, video_requested)?;
    }
    writeln!(
        output,
        "<footer><p>Whirl {} · {} / {} · Test status is independent of recording availability.</p>\
        <a href=\"#top\">Back to top</a></footer></main></body></html>",
        env!("CARGO_PKG_VERSION"),
        env::consts::OS,
        env::consts::ARCH
    )
}

fn render_file(
    output: &mut impl io::Write,
    index: usize,
    file: &FileReport,
    metadata: &ReportMetadata,
    video_requested: bool,
) -> io::Result<()> {
    let context = metadata.files.get(&file.path);
    let title = context
        .and_then(|file| file.title.as_deref())
        .unwrap_or(&file.path);
    write!(
        output,
        "<article id=\"flow-{index}\" data-status=\"{}\">\
        <div class=\"flow-heading\"><div><p class=\"eyebrow\">Flow {:02}</p>\
        <h2>{}</h2>",
        status_text(file.status).0,
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
        "<p class=\"file-path\"><code>{}</code></p></div>{}</div>",
        escape(&file.path),
        badge(file.status)
    )?;
    if let Some(step) = file
        .entries
        .iter()
        .flat_map(|entry| &entry.steps)
        .find(|step| matches!(step.status, Status::Failed | Status::Error))
    {
        write!(
            output,
            "<section class=\"failure\" aria-label=\"Failure\"><h3>{} at line {}</h3>\
            <p><code>{}</code></p>",
            status_text(step.status).1,
            step.line,
            escape(&step.text)
        )?;
        render_error(output, step)?;
        write!(output, "</section>")?;
    }
    let video = file
        .artifacts
        .iter()
        .find(|path| Path::new(path).extension().is_some_and(|ext| ext == "webm"));
    if let Some(path) = video {
        render_media(output, file, path, true)?;
    } else {
        let message = if video_requested {
            "Recording unavailable."
        } else {
            "Recording not requested."
        };
        write!(
            output,
            "<p class=\"media-note\" data-recording=\"unavailable\">{message}</p>"
        )?;
    }
    let steps = file
        .entries
        .iter()
        .map(|entry| entry.steps.len())
        .sum::<usize>();
    write!(
        output,
        "<div class=\"file-summary\"><p>{steps} steps · {}</p>",
        duration(file.duration_ms)
    )?;
    if let Some(runtime) = &file.runtime {
        write!(
            output,
            "<p>{} · {} × {}</p>",
            escape(&runtime.browser),
            runtime.viewport.width,
            runtime.viewport.height
        )?;
    }
    write!(output, "</div>")?;
    for warning in &file.warnings {
        write!(
            output,
            "<p class=\"warning\">Warning: {}</p>",
            escape(warning)
        )?;
    }
    if !file.blocked_hosts.is_empty() {
        write!(
            output,
            "<p class=\"warning\">Blocked hosts: {}</p>",
            escape(&file.blocked_hosts.join(", "))
        )?;
    }
    write!(
        output,
        "<details class=\"checkpoints\"{}><summary>Execution checkpoints</summary><ol role=\"list\">",
        if matches!(file.status, Status::Failed | Status::Error) {
            " open"
        } else {
            ""
        }
    )?;
    for entry in &file.entries {
        render_entry(output, file, entry)?;
    }
    write!(output, "</ol></details>")?;
    if let Some(runtime) = &file.runtime {
        write!(
            output,
            "<details class=\"runtime\"><summary>Browser environment</summary><dl>"
        )?;
        for (name, value) in [
            ("Browser version", runtime.browser_version.as_deref()),
            ("Playwright", runtime.playwright_version.as_deref()),
            ("Node", runtime.node_version.as_deref()),
            ("User agent", runtime.user_agent.as_deref()),
        ] {
            write!(
                output,
                "<dt>{name}</dt><dd>{}</dd>",
                escape(value.unwrap_or("Unavailable"))
            )?;
        }
        write!(output, "</dl></details>")?;
    }
    // Traces and HARs remain diagnostic files, with paths shown as text. No
    // arbitrary path becomes a navigable URL in the portable document.
    for path in &file.artifacts {
        if path != video.map_or("", String::as_str) {
            write!(
                output,
                "<p class=\"artifact-path\">External artifact: <code>{}</code></p>",
                escape(path)
            )?;
        }
    }
    write!(output, "</article>")
}

fn render_entry(
    output: &mut impl io::Write,
    file: &FileReport,
    entry: &EntryReport,
) -> io::Result<()> {
    write!(
        output,
        "<li class=\"entry\" data-status=\"{}\"><div class=\"entry-heading\">\
        <h3>{}</h3>{}<p>{}</p></div><ol class=\"steps\" role=\"list\">",
        status_text(entry.status).0,
        escape(&entry.name),
        badge(entry.status),
        duration(entry.duration_ms)
    )?;
    for step in &entry.steps {
        write!(
            output,
            "<li data-status=\"{}\"><div class=\"step-heading\">\
            <p class=\"step-text\"><code>{}</code></p>{}</div>\
            <p class=\"step-location\">Line {} · {}</p>",
            status_text(step.status).0,
            escape(&step.text),
            badge(step.status),
            step.line,
            duration(step.duration_ms)
        )?;
        render_error(output, step)?;
        write!(output, "</li>")?;
    }
    write!(output, "</ol>")?;
    if !entry.captures.is_empty() {
        write!(output, "<details><summary>Captured values</summary><dl>")?;
        for (name, value) in &entry.captures {
            write!(
                output,
                "<dt>{}</dt><dd><code>{}</code></dd>",
                escape(name),
                escape(value)
            )?;
        }
        write!(output, "</dl></details>")?;
    }
    for path in &entry.artifacts {
        if Path::new(path).extension().is_some_and(|ext| ext == "png") {
            render_media(output, file, path, false)?;
        } else {
            write!(
                output,
                "<p class=\"artifact-path\">External artifact: <code>{}</code></p>",
                escape(path)
            )?;
        }
    }
    write!(output, "</li>")
}

fn render_error(output: &mut impl io::Write, step: &StepReport) -> io::Result<()> {
    if let Some(error) = &step.error {
        write!(
            output,
            "<div class=\"error-detail\"><p>{}</p><dl>",
            escape(&error.message)
        )?;
        for (name, value) in [
            ("Expected", error.expected.as_deref()),
            ("Actual", error.actual.as_deref()),
        ] {
            if let Some(value) = value {
                write!(
                    output,
                    "<dt>{name}</dt><dd><code>{}</code></dd>",
                    escape(if value.is_empty() { "\"\"" } else { value })
                )?;
            }
        }
        write!(output, "</dl>")?;
        for candidate in error.candidates.iter().flatten() {
            write!(
                output,
                "<p>Candidate: <code>{}</code></p>",
                escape(candidate)
            )?;
        }
        write!(output, "</div>")?;
    }
    Ok(())
}

fn open_media(file: &FileReport, path: &str, video: bool) -> anyhow::Result<File> {
    let root = Path::new(&file.artifacts_dir).canonicalize()?;
    let canonical = Path::new(path).canonicalize()?;
    anyhow::ensure!(
        canonical.starts_with(root),
        "artifact is outside its flow directory"
    );
    anyhow::ensure!(
        canonical.metadata()?.is_file(),
        "artifact is not a regular file"
    );
    let mut input = File::open(canonical)?;
    anyhow::ensure!(
        input.metadata()?.is_file(),
        "artifact is not a regular file"
    );
    let mut signature = [0; 8];
    input.read_exact(&mut signature)?;
    anyhow::ensure!(
        if video {
            signature.starts_with(&[0x1a, 0x45, 0xdf, 0xa3])
        } else {
            &signature == b"\x89PNG\r\n\x1a\n"
        },
        "artifact has an invalid media header"
    );
    input.rewind()?;
    Ok(input)
}

fn render_media(
    output: &mut impl io::Write,
    file: &FileReport,
    path: &str,
    video: bool,
) -> io::Result<()> {
    let mut input = match open_media(file, path, video) {
        Ok(input) => input,
        Err(error) => {
            return write!(
                output,
                "<p class=\"media-note\" data-recording=\"unavailable\">\
                {} unavailable: <code>{}</code> — {}.</p>",
                if video { "Recording" } else { "Screenshot" },
                escape(path),
                escape(&error.to_string())
            );
        }
    };
    let name = Path::new(path)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    if video {
        write!(
            output,
            "<figure class=\"recording\"><video controls preload=\"metadata\" playsinline \
            aria-label=\"Browser recording\" src=\"data:video/webm;base64,"
        )?;
    } else {
        write!(
            output,
            "<details class=\"screenshot\"><summary>Screenshot: {}</summary>\
            <img loading=\"lazy\" alt=\"\" src=\"data:image/png;base64,",
            escape(&name)
        )?;
    }
    let mut encoder = EncoderWriter::new(&mut *output, &STANDARD);
    io::copy(&mut input, &mut encoder)?;
    encoder.finish()?;
    drop(encoder);
    if video {
        write!(
            output,
            "\"></video><figcaption>Browser recording · {}</figcaption></figure>",
            escape(&name)
        )
    } else {
        write!(output, "\"></details>")
    }
}

fn status_text(status: Status) -> (&'static str, &'static str) {
    match status {
        Status::Passed => ("passed", "Passed"),
        Status::Failed => ("failed", "Failed"),
        Status::Error => ("error", "Error"),
        Status::Skipped => ("skipped", "Skipped"),
    }
}

fn badge(status: Status) -> String {
    let (class, label) = status_text(status);
    format!("<p class=\"badge {class}\">{label}</p>")
}

fn duration(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms} ms")
    } else {
        format!("{:.1} s", ms as f64 / 1_000.0)
    }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
