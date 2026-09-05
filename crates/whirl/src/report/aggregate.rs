//! Select saved attempts without turning several runs into a synthetic run.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::report::json::Document;
use crate::report::metadata::ReportMetadata;
use crate::report::model::FileReport;

pub(crate) struct Input {
    pub(crate) path:     PathBuf,
    pub(crate) document: Document,
    pub(crate) base:     PathBuf,
}

#[derive(Clone, Copy)]
pub(crate) struct Selection {
    pub(crate) input: usize,
    pub(crate) file:  usize,
}

pub(crate) struct Scenario {
    pub(crate) path:     String,
    pub(crate) selected: Option<Selection>,
}

pub(crate) struct Report {
    pub(crate) inputs:    Vec<Input>,
    pub(crate) scenarios: Vec<Scenario>,
    pub(crate) setups:    Vec<Selection>,
    pub(crate) other:     Vec<Selection>,
    pub(crate) metadata:  ReportMetadata,
}

pub(crate) fn read_expected(path: &Path) -> anyhow::Result<Vec<String>> {
    let source = fs::read_to_string(path)
        .with_context(|| format!("reading expected scenarios '{}'", path.display()))?;
    let paths: Vec<String> = serde_json::from_str(&source)
        .context("expected scenarios must be a JSON array of flow paths")?;
    anyhow::ensure!(!paths.is_empty(), "expected scenarios must not be empty");
    let mut unique = HashSet::new();
    for path in &paths {
        anyhow::ensure!(
            !path.trim().is_empty(),
            "expected flow path must not be empty"
        );
        anyhow::ensure!(unique.insert(path), "duplicate expected flow '{path}'");
    }
    Ok(paths)
}

impl Report {
    pub(crate) fn new(
        mut inputs: Vec<Input>,
        expected: Option<Vec<String>>,
        metadata: Option<ReportMetadata>,
    ) -> anyhow::Result<Self> {
        // A copied report is the same evidence. Use a stable source label even
        // when the caller supplies the same documents in a different order.
        inputs.sort_by(|left, right| left.path.cmp(&right.path));
        let mut unique: Vec<Input> = Vec::new();
        for input in inputs {
            if !unique
                .iter()
                .any(|saved| saved.document == input.document && saved.base == input.base)
            {
                unique.push(input);
            }
        }
        let mut report = Self {
            inputs:    unique,
            scenarios: Vec::new(),
            setups:    Vec::new(),
            other:     Vec::new(),
            metadata:  metadata.unwrap_or_default(),
        };
        let mut latest: BTreeMap<String, Selection> = BTreeMap::new();
        let mut seen_times = HashSet::new();
        for (input_index, input) in report.inputs.iter().enumerate() {
            for (file_index, file) in input.document.report.files.iter().enumerate() {
                let selected = Selection {
                    input: input_index,
                    file:  file_index,
                };
                if file
                    .roles
                    .is_some_and(|roles| roles.setup && !roles.requested)
                {
                    report.setups.push(selected);
                    continue;
                }
                let new_time = file
                    .timing
                    .started_at
                    .or(input.document.report.timing.started_at);
                if let Some(time) = new_time {
                    anyhow::ensure!(
                        seen_times.insert((&file.path, time)),
                        "cannot select latest attempt for '{}': conflicting attempts have the same start timestamp (source '{}')",
                        file.path,
                        input.path.display()
                    );
                }
                if let Some(previous) = latest.get(&file.path) {
                    let (old_input, old_file) = report.get(*previous);
                    let old_time = old_file.timing.started_at.or(old_input
                        .document
                        .report
                        .timing
                        .started_at);
                    let (Some(old_time), Some(new_time)) = (old_time, new_time) else {
                        anyhow::bail!(
                            "cannot select latest attempt for '{}': missing start timestamp in '{}' or '{}'",
                            file.path,
                            old_input.path.display(),
                            input.path.display()
                        );
                    };
                    if new_time < old_time {
                        continue;
                    }
                }
                latest.insert(file.path.clone(), selected);
            }
        }
        if let Some(expected) = expected {
            report.scenarios = expected
                .into_iter()
                .map(|path| Scenario {
                    selected: latest.remove(&path),
                    path,
                })
                .collect();
            report.other = latest.into_values().collect();
        } else {
            report.scenarios = latest
                .into_iter()
                .map(|(path, selected)| Scenario {
                    path,
                    selected: Some(selected),
                })
                .collect();
        }
        Ok(report)
    }

    pub(crate) fn get(&self, selection: Selection) -> (&Input, &FileReport) {
        let input = &self.inputs[selection.input];
        (input, &input.document.report.files[selection.file])
    }
}
