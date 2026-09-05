//! Portable HTML reports built from the same masked results as JSON and JUnit.

use std::fs::File;
use std::io::{self, BufWriter, Read as _, Seek as _, Write as _};
use std::path::Path;

use anyhow::Context as _;
use base64::engine::general_purpose::STANDARD;
use base64::write::EncoderWriter;
use tempfile::NamedTempFile;

use crate::report::json::Document;
use crate::report::metadata::{FileMetadata, ReportMetadata};
use crate::report::model::{EntryReport, FileReport, Status, StepReport, Timing};

pub(crate) mod aggregate;

const STYLE: &str = include_str!("html.css");

/// Stream media to a sibling temporary file, then atomically replace the
/// report. Missing or invalid media is shown as unavailable; output errors fail
/// the write.
pub(crate) fn write(path: &Path, document: &Document, base: &Path) -> anyhow::Result<()> {
    check_artifact_destination(path, document, base)?;
    write_atomic(path, |output| render(output, document, base))
}

fn check_artifact_destination(path: &Path, document: &Document, base: &Path) -> anyhow::Result<()> {
    let report = &document.report;
    if let Ok(target) = path.canonicalize() {
        for artifact in report.files.iter().flat_map(|file| {
            file.artifacts
                .iter()
                .chain(file.entries.iter().flat_map(|entry| &entry.artifacts))
        }) {
            anyhow::ensure!(
                base.join(artifact).canonicalize().ok().as_ref() != Some(&target),
                "HTML report destination conflicts with artifact '{artifact}'"
            );
        }
    }
    Ok(())
}

fn write_atomic(
    path: &Path,
    render: impl FnOnce(&mut BufWriter<&mut File>) -> io::Result<()>,
) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary = NamedTempFile::new_in(parent).context("creating temporary HTML report")?;
    {
        let mut output = BufWriter::new(temporary.as_file_mut());
        render(&mut output).context("writing HTML report")?;
        output.flush().context("flushing HTML report")?;
    }
    temporary.persist(path).context("saving HTML report")?;
    Ok(())
}

fn render_head(output: &mut impl io::Write, title: &str) -> io::Result<()> {
    write!(
        output,
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
        <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
        <meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; \
        style-src 'unsafe-inline'; img-src data:; media-src data:; base-uri 'none'; form-action 'none'\">\
        <title>{}</title><style>{STYLE}</style></head><body><main id=\"top\">",
        escape(title)
    )
}

fn render(output: &mut impl io::Write, document: &Document, base: &Path) -> io::Result<()> {
    let report = &document.report;
    let empty_metadata = ReportMetadata::default();
    let metadata = document.metadata.as_ref().unwrap_or(&empty_metadata);
    let title = metadata.title.as_deref().unwrap_or("Browser test report");
    render_head(output, title)?;
    write!(
        output,
        "<header><p class=\"eyebrow\">Whirl / Browser verification</p><h1>{}</h1>\
        <p>Recorded results from {} flow files.</p></header>",
        escape(title),
        report.files.len()
    )?;
    write!(
        output,
        "<dl class=\"run-record\" aria-label=\"Run timestamps\">"
    )?;
    render_timing(output, &report.timing)?;
    write!(output, "</dl>")?;
    if report.files.iter().all(|file| file.roles.is_some()) {
        let requested = report
            .files
            .iter()
            .filter(|file| file.roles.is_some_and(|roles| roles.requested))
            .count();
        let setups = report
            .files
            .iter()
            .filter(|file| file.roles.is_some_and(|roles| roles.setup))
            .count();
        write!(
            output,
            "<p>{requested} requested flows · {setups} setup flows. Status totals include all flow files.</p>"
        )?;
    }
    if let Some(description) = &metadata.description {
        write!(
            output,
            "<section class=\"scope\" aria-label=\"Author-provided context\">\
            <h2>Test scope</h2><p>{}</p></section>",
            escape(description)
        )?;
    }
    render_details(output, metadata)?;
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
            "<li><a href=\"#flow-{index}\">{} <span class=\"flow-role\">({})</span></a>{}</li>",
            escape(title),
            role_label(file),
            badge(file.status)
        )?;
    }
    write!(output, "</ol></nav></details>")?;
    for (index, file) in report.files.iter().enumerate() {
        render_file(
            output,
            index,
            file,
            metadata.files.get(&file.path),
            document.video_requested,
            base,
            None,
        )?;
    }
    writeln!(
        output,
        "<footer><p>Whirl {} · {} / {} · Test status is independent of recording availability.</p>\
        <a href=\"#top\">Back to top</a></footer></main></body></html>",
        escape(&document.whirl_version),
        escape(&document.platform),
        escape(&document.architecture)
    )
}

fn render_details(output: &mut impl io::Write, metadata: &ReportMetadata) -> io::Result<()> {
    if !metadata.details.is_empty() {
        write!(
            output,
            "<details class=\"runtime\"><summary>Author-provided details</summary><dl>"
        )?;
        for (label, value) in &metadata.details {
            write!(
                output,
                "<dt>{}</dt><dd>{}</dd>",
                escape(label),
                escape(value)
            )?;
        }
        write!(output, "</dl></details>")?;
    }
    Ok(())
}

fn render_file(
    output: &mut impl io::Write,
    index: usize,
    file: &FileReport,
    context: Option<&FileMetadata>,
    video_requested: Option<bool>,
    base: &Path,
    source: Option<(usize, &Path)>,
) -> io::Result<()> {
    let title = context
        .and_then(|file| file.title.as_deref())
        .unwrap_or(&file.path);
    write!(
        output,
        "<article id=\"flow-{index}\" data-status=\"{}\">\
        <div class=\"flow-heading\"><div><p class=\"eyebrow\">Flow {:02} · {}</p>\
        <h2>{}</h2>",
        status_text(file.status).0,
        index + 1,
        role_label(file),
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
    if let Some((source_index, path)) = source {
        write!(
            output,
            "<p class=\"artifact-path\">Evidence from <a href=\"#source-{source_index}\">Source {} · {}</a></p>",
            source_index + 1,
            escape(&path.to_string_lossy())
        )?;
    }
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
        render_media(output, file, path, true, base)?;
    } else {
        let message = match video_requested {
            Some(true) => "Recording unavailable.",
            Some(false) => "Recording not requested.",
            None => "Recording unavailable; recording settings were not recorded.",
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
        render_entry(output, file, entry, base)?;
    }
    write!(output, "</ol></details>")?;
    write!(
        output,
        "<details class=\"runtime\"><summary>Run record</summary><dl>"
    )?;
    render_timing(output, &file.timing)?;
    write!(
        output,
        "<dt>Source SHA-256</dt><dd><code>{}</code></dd></dl></details>",
        escape(file.source_sha256.as_deref().unwrap_or("Not recorded"))
    )?;
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
    base: &Path,
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
            render_media(output, file, path, false, base)?;
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

fn open_media(file: &FileReport, path: &str, video: bool, base: &Path) -> anyhow::Result<File> {
    let root = base.join(&file.artifacts_dir).canonicalize()?;
    let canonical = base.join(path).canonicalize()?;
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
    base: &Path,
) -> io::Result<()> {
    let mut input = match open_media(file, path, video, base) {
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

fn role_label(file: &FileReport) -> &'static str {
    match file.roles {
        Some(roles) if roles.requested && roles.setup => "Requested flow and setup",
        Some(roles) if roles.setup => "Setup flow",
        Some(_) => "Requested scenario",
        None => "Role not recorded",
    }
}

fn render_timing(output: &mut impl io::Write, timing: &Timing) -> io::Result<()> {
    for (label, timestamp) in [
        ("Started", timing.started_at),
        ("Finished", timing.finished_at),
    ] {
        write!(output, "<dt>{label}</dt><dd>")?;
        if let Some(timestamp) = timestamp {
            let value = timestamp.to_rfc3339();
            write!(output, "<time datetime=\"{value}\">{value}</time>")?;
        } else {
            write!(output, "Not recorded")?;
        }
        write!(output, "</dd>")?;
    }
    Ok(())
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
