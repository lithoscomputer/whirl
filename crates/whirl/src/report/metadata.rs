//! Author-written report context. It never changes execution or outcomes.

use std::collections::BTreeMap;
use std::path::Path;
use std::{fmt, fs};

use anyhow::Context as _;
use serde::de::{Error as _, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) title:       Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReportMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) title:       Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    #[serde(default, deserialize_with = "unique_files")]
    pub(crate) files:       BTreeMap<String, FileMetadata>,
}

fn unique_files<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, FileMetadata>, D::Error> {
    struct FilesVisitor;
    impl<'de> Visitor<'de> for FilesVisitor {
        type Value = BTreeMap<String, FileMetadata>;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("an object with unique flow paths")
        }
        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
            let mut files = BTreeMap::new();
            while let Some((path, metadata)) = map.next_entry::<String, FileMetadata>()? {
                if files.insert(path.clone(), metadata).is_some() {
                    return Err(M::Error::custom(format!(
                        "duplicate report metadata for flow '{path}'"
                    )));
                }
            }
            Ok(files)
        }
    }
    deserializer.deserialize_map(FilesVisitor)
}

impl ReportMetadata {
    /// Resolve metadata keys relative to its file, before a browser starts.
    pub(crate) fn load(path: &Path) -> anyhow::Result<Self> {
        let mut metadata: Self =
            serde_json::from_str(&fs::read_to_string(path)?).context("invalid report metadata")?;
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let mut files = BTreeMap::new();
        for (key, value) in metadata.files {
            let canonical = parent
                .join(&key)
                .canonicalize()
                .with_context(|| format!("resolving report metadata flow '{key}'"))?;
            anyhow::ensure!(canonical.is_file(), "metadata flow '{key}' is not a file");
            anyhow::ensure!(
                files
                    .insert(canonical.to_string_lossy().into_owned(), value)
                    .is_none(),
                "duplicate report metadata for flow '{key}'"
            );
        }
        metadata.files = files;
        Ok(metadata)
    }

    /// Match canonical inputs once, retaining the paths used in run reports.
    pub(crate) fn select<'a>(&mut self, paths: impl Iterator<Item = &'a Path>) {
        self.files = paths
            .filter_map(|path| {
                let canonical = path.canonicalize().ok()?;
                let value = self.files.get(canonical.to_string_lossy().as_ref())?;
                Some((path.to_string_lossy().into_owned(), value.clone()))
            })
            .collect();
    }
}
