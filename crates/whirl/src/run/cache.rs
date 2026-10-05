//! The AI cache (SPEC 12.1): one committed JSON file next to each flow
//! that records what its `ai:` targets, `ACT` lines, and `GOAL` lines
//! resolved to.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::{fs, io};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::lang::ast::{ActionKind, AssertBody, CheckLine, CheckStep, File, Locator, Subject};
use crate::lang::fmt::render_snapshot_target;
use crate::run::act::Fingerprint;

/// The cache file format version (SPEC 12.1).
const VERSION: u32 = 1;

/// What a run does with each flow's cache (SPEC 12.1).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, clap::ValueEnum)]
pub(crate) enum CacheMode {
    /// Replay hits; resolve misses with the model; never write.
    #[default]
    Replay,
    /// As `replay`, then write the cache of each file that passed.
    Update,
    /// Fail a miss instead of asking the model.
    Only,
}

impl CacheMode {
    pub(crate) fn allows_model(self) -> bool {
        self != Self::Only
    }
}

/// The cache path of a flow: `<flow>.whirl-cache.json` beside it.
pub(crate) fn cache_path(flow: &Path) -> PathBuf {
    let mut name = flow.file_name().unwrap_or_default().to_os_string();
    name.push("-cache.json");
    flow.with_file_name(name)
}

/// One action an `ACT` or `GOAL` line ran, as the cache holds it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CachedAction {
    /// The action as a Whirl line; variable references stay references.
    pub(crate) line:         String,
    /// The fingerprint of each element in the line, in order.
    pub(crate) fingerprints: Vec<Fingerprint>,
}

/// One cache entry (SPEC 12.1).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) enum CacheEntry {
    AiTarget {
        line:        String,
        occurrence:  u32,
        target:      String,
        model:       String,
        locator:     String,
        fingerprint: Fingerprint,
    },
    Act {
        line:       String,
        occurrence: u32,
        model:      String,
        actions:    Vec<CachedAction>,
    },
    Goal {
        line:       String,
        occurrence: u32,
        model:      String,
        actions:    Vec<CachedAction>,
    },
}

impl CacheEntry {
    pub(crate) fn key(&self) -> CacheKey {
        match self {
            Self::AiTarget {
                line,
                occurrence,
                target,
                ..
            } => CacheKey {
                kind:       EntryKind::AiTarget,
                line:       line.clone(),
                occurrence: *occurrence,
                target:     Some(target.clone()),
            },
            Self::Act {
                line, occurrence, ..
            } => CacheKey {
                kind:       EntryKind::Act,
                line:       line.clone(),
                occurrence: *occurrence,
                target:     None,
            },
            Self::Goal {
                line, occurrence, ..
            } => CacheKey {
                kind:       EntryKind::Goal,
                line:       line.clone(),
                occurrence: *occurrence,
                target:     None,
            },
        }
    }
}

/// The kinds of entry.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum EntryKind {
    AiTarget,
    Act,
    Goal,
}

/// What names an entry: its kind, its authored line, the line's
/// occurrence, and for a target its authored locator (SPEC 12.1).
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct CacheKey {
    pub(crate) kind:       EntryKind,
    pub(crate) line:       String,
    pub(crate) occurrence: u32,
    pub(crate) target:     Option<String>,
}

/// The file as written.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CacheFile {
    version: u32,
    entries: Vec<CacheEntry>,
}

/// Why a cache file cannot be read (SPEC 12.1).
#[derive(Debug, thiserror::Error)]
pub(crate) enum CacheError {
    #[error("cannot read '{path}'")]
    Read {
        path:   PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("'{path}' is not a valid AI cache: {detail}")]
    Invalid { path: PathBuf, detail: String },
}

/// The entries of a cache file; empty when the file does not exist.
pub(crate) fn load(path: &Path) -> Result<Vec<CacheEntry>, CacheError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(CacheError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let file: CacheFile = serde_json::from_str(&text).map_err(|error| CacheError::Invalid {
        path:   path.to_path_buf(),
        detail: error.to_string(),
    })?;
    if file.version != VERSION {
        return Err(CacheError::Invalid {
            path:   path.to_path_buf(),
            detail: format!("version {} is not version {VERSION}", file.version),
        });
    }
    Ok(file.entries)
}

/// The file text for `entries`, in the order given: two-space JSON with a
/// final newline, so diffs stay small.
pub(crate) fn render(entries: &[CacheEntry]) -> String {
    let file = CacheFile {
        version: VERSION,
        entries: entries.to_vec(),
    };
    let mut text = serde_json::to_string_pretty(&file).expect("a cache always serializes");
    text.push('\n');
    text
}

/// Writes the cache, or deletes it when no entry is left (SPEC 12.1). The
/// write goes through a temporary file, so a reader never sees half a
/// file.
pub(crate) fn save(path: &Path, entries: &[CacheEntry]) -> io::Result<()> {
    if entries.is_empty() {
        return match fs::remove_file(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        };
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let temp = NamedTempFile::new_in(dir)?;
    fs::write(temp.path(), render(entries))?;
    temp.persist(path).map_err(|error| error.error)?;
    Ok(())
}

/// One flow's cache while the flow runs (SPEC 12.1).
#[derive(Debug)]
pub(crate) struct FlowCache {
    mode:        CacheMode,
    path:        PathBuf,
    entries:     HashMap<CacheKey, CacheEntry>,
    occurrences: HashMap<u32, u32>,
    /// The entries this run used, made, or healed, with their line and
    /// their order within it.
    kept:        Vec<(u32, usize, CacheEntry)>,
}

impl FlowCache {
    /// The cache of a flow. A cache that cannot be read counts as empty;
    /// `whirl check` reports it before a run starts.
    pub(crate) fn load(mode: CacheMode, flow: &Path, file: &File) -> (Self, Option<CacheError>) {
        let path = cache_path(flow);
        let (entries, error) = match load(&path) {
            Ok(entries) => (entries, None),
            Err(error) => (Vec::new(), Some(error)),
        };
        let entries = entries
            .into_iter()
            .map(|entry| (entry.key(), entry))
            .collect();
        let cache = Self {
            mode,
            path,
            entries,
            occurrences: occurrences(file),
            kept: Vec::new(),
        };
        (cache, error)
    }

    pub(crate) fn mode(&self) -> CacheMode {
        self.mode
    }

    /// The key of a step line's entry.
    pub(crate) fn key(
        &self,
        kind: EntryKind,
        line: u32,
        text: &str,
        target: Option<String>,
    ) -> CacheKey {
        CacheKey {
            kind,
            line: text.to_owned(),
            occurrence: self.occurrences.get(&line).copied().unwrap_or(1),
            target,
        }
    }

    pub(crate) fn get(&self, key: &CacheKey) -> Option<&CacheEntry> {
        self.entries.get(key)
    }

    /// Keeps an entry for the next write, in line order.
    pub(crate) fn keep(&mut self, line: u32, entry: CacheEntry) {
        let order = self.kept.iter().filter(|(kept, ..)| *kept == line).count();
        self.kept.retain(|(_, _, kept)| kept.key() != entry.key());
        self.kept.push((line, order, entry));
    }

    /// Writes the kept entries in `update` mode after a passing file, and
    /// removes the entries the run did not use (SPEC 12.1).
    pub(crate) fn finish(mut self, passed: bool) -> io::Result<Option<PathBuf>> {
        if self.mode != CacheMode::Update || !passed {
            return Ok(None);
        }
        if self.kept.is_empty() && self.entries.is_empty() {
            return Ok(None);
        }
        self.kept.sort_by_key(|(line, order, _)| (*line, *order));
        let entries: Vec<CacheEntry> = self.kept.into_iter().map(|(.., entry)| entry).collect();
        save(&self.path, &entries)?;
        Ok(Some(self.path))
    }
}

/// The key of every entry the file's lines could have (SPEC 12.1).
pub(crate) fn file_keys(file: &File) -> Vec<CacheKey> {
    let occurrences = occurrences(file);
    let key = |kind, line: u32, text: &str, target: Option<String>| CacheKey {
        kind,
        line: text.to_owned(),
        occurrence: occurrences.get(&line).copied().unwrap_or(1),
        target,
    };
    let mut keys = Vec::new();
    let targets = |line: u32, text: &str, locators: Vec<&Locator>, keys: &mut Vec<CacheKey>| {
        for locator in locators {
            if locator.ai_description().is_some() {
                keys.push(key(
                    EntryKind::AiTarget,
                    line,
                    text,
                    Some(render_snapshot_target(locator)),
                ));
            }
        }
    };
    for entry in &file.entries {
        for action in &entry.actions {
            match action.kind {
                ActionKind::Act { .. } => {
                    keys.push(key(EntryKind::Act, action.line, &action.text, None));
                }
                ActionKind::Goal { .. } => {
                    keys.push(key(EntryKind::Goal, action.line, &action.text, None));
                }
                _ => {}
            }
            targets(action.line, &action.text, action.kind.locators(), &mut keys);
        }
        for check in &entry.checks {
            let (line, text, locator) = match check {
                CheckStep::Assert(assert) => {
                    let locator = match &assert.body {
                        AssertBody::ElementState { locator, .. }
                        | AssertBody::Check(CheckLine {
                            subject: Subject::Element { locator, .. },
                            ..
                        }) => Some(locator),
                        AssertBody::Check(_) | AssertBody::TabClosed { .. } => None,
                    };
                    (assert.line, assert.text.as_str(), locator)
                }
                CheckStep::Capture(capture) => {
                    let locator = match &capture.subject {
                        Subject::Element { locator, .. } => Some(locator),
                        _ => None,
                    };
                    (capture.line, capture.text.as_str(), locator)
                }
                CheckStep::Judge(judge) => (judge.line, judge.text.as_str(), judge.scope.as_ref()),
            };
            targets(line, text, locator.into_iter().collect(), &mut keys);
        }
    }
    keys
}

/// The occurrence of each step line's authored text among identical lines
/// of the file, from 1, by source line (SPEC 12.1).
pub(crate) fn occurrences(file: &File) -> HashMap<u32, u32> {
    let mut seen: HashMap<&str, u32> = HashMap::new();
    let mut by_line = HashMap::new();
    for entry in &file.entries {
        let actions = entry
            .actions
            .iter()
            .map(|action| (action.line, action.text.as_str()));
        let checks = entry
            .checks
            .iter()
            .map(|check| (check.line(), check.text()));
        for (line, text) in actions.chain(checks) {
            let count = seen.entry(text).or_insert(0);
            *count += 1;
            by_line.insert(line, *count);
        }
    }
    by_line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::parse::parse_file;

    fn target_entry() -> CacheEntry {
        CacheEntry::AiTarget {
            line:        "CLICK ai:\"the buy button\"".to_owned(),
            occurrence:  1,
            target:      "ai:\"the buy button\"".to_owned(),
            model:       "gpt-test".to_owned(),
            locator:     "role:button Buy".to_owned(),
            fingerprint: Fingerprint {
                role: "button".to_owned(),
                name: Some("Buy".to_owned()),
            },
        }
    }

    #[test]
    fn the_cache_sits_beside_its_flow() {
        assert_eq!(
            cache_path(Path::new("flows/checkout.whirl")),
            Path::new("flows/checkout.whirl-cache.json")
        );
    }

    #[test]
    fn entries_render_in_a_stable_order_and_read_back() {
        let text = render(&[target_entry()]);
        assert_eq!(
            text,
            r#"{
  "version": 1,
  "entries": [
    {
      "kind": "ai-target",
      "line": "CLICK ai:\"the buy button\"",
      "occurrence": 1,
      "target": "ai:\"the buy button\"",
      "model": "gpt-test",
      "locator": "role:button Buy",
      "fingerprint": {
        "role": "button",
        "name": "Buy"
      }
    }
  ]
}
"#
        );
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("flow.whirl-cache.json");
        save(&path, &[target_entry()]).expect("the cache writes");
        assert_eq!(load(&path).expect("the cache reads"), [target_entry()]);
        save(&path, &[]).expect("an empty cache deletes the file");
        assert!(!path.exists());
        assert_eq!(load(&path).expect("no file is an empty cache"), []);
    }

    #[test]
    fn an_unknown_version_or_shape_is_invalid() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("flow.whirl-cache.json");
        fs::write(&path, r#"{"version": 2, "entries": []}"#).expect("write");
        assert!(matches!(load(&path), Err(CacheError::Invalid { .. })));
        fs::write(&path, r#"{"version": 1, "entries": [{"kind": "nope"}]}"#).expect("write");
        assert!(matches!(load(&path), Err(CacheError::Invalid { .. })));
    }

    #[test]
    fn identical_lines_count_their_occurrences() {
        let file = parse_file(
            Path::new("t.whirl"),
            "VISIT /\nCLICK Next\nASSERT url == /\nCLICK Next\n",
        )
        .expect("parses");
        let occurrences = occurrences(&file);
        assert_eq!(occurrences[&2], 1);
        assert_eq!(occurrences[&4], 2);
    }
}
