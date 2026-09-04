//! Artifact directory mapping (SPEC section 14): the per-flow artifact
//! directory, fixed artifact file names, snapshot baseline paths, and
//! the duplicate-flow and collision rules.

use std::path::{Path, PathBuf};
use std::{env, io};

use sha2::{Digest as _, Sha256};

/// Fixed artifact file name: the on-failure full-page screenshot.
pub const FAILURE_PNG: &str = "failure.png";
/// Fixed artifact file name: the Playwright trace of a failed flow.
pub const TRACE_ZIP: &str = "trace.zip";
/// Fixed artifact file name: the `--video` recording.
pub const VIDEO_WEBM: &str = "video.webm";
/// Fixed artifact file name: the `--har` network log.
pub const NETWORK_HAR: &str = "network.har";

/// The artifact file name of a `SCREENSHOT name` action: `<name>.png`.
pub fn screenshot_file(name: &str) -> String {
    format!("{name}.png")
}

/// The artifact file name of a failed snapshot's captured frame.
pub fn snapshot_actual_file(name: &str) -> String {
    format!("snapshot-{name}-actual.png")
}

/// The artifact file name of a failed snapshot's diff image.
pub fn snapshot_diff_file(name: &str) -> String {
    format!("snapshot-{name}-diff.png")
}

/// The platform tag in snapshot baseline names (SPEC 7): `linux`,
/// `darwin`, or `win32`.
pub fn platform_tag() -> &'static str {
    match env::consts::OS {
        "linux" => "linux",
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

/// The baseline image path of `SNAPSHOT name` (SPEC 7):
/// `<flow>.whirl-snapshots/<name>-<browser>-<platform>.png` next to the
/// flow file.
pub fn snapshot_baseline_path(flow_path: &Path, name: &str, browser: &str) -> PathBuf {
    let file_name = flow_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let snapshots_dir = flow_path.with_file_name(format!("{file_name}-snapshots"));
    snapshots_dir.join(format!(
        "{name}-{browser}-{platform}.png",
        platform = platform_tag()
    ))
}

/// A failure while mapping flows to artifact directories. Both variants
/// are runtime errors (SPEC 13, exit 3).
#[derive(Debug, thiserror::Error)]
pub enum ArtifactsError {
    #[error("cannot resolve '{path}': {source}")]
    Canonicalize { path: PathBuf, source: io::Error },
    #[error(
        "flows '{first}' and '{second}' both map to artifact directory '{dir}'",
        first = first.display(),
        second = second.display(),
        dir = dir.display()
    )]
    Collision {
        first:  PathBuf,
        second: PathBuf,
        dir:    PathBuf,
    },
}

/// One unique flow: the input path as given, its canonical path
/// (absolute, symlinks resolved), and its artifact directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Flow {
    /// The first input path that named this flow.
    pub input:     PathBuf,
    /// The canonical path; the identity that dedups flows.
    pub canonical: PathBuf,
    /// The flow's artifact directory under the artifacts dir.
    pub dir:       PathBuf,
}

/// Deduplicates input paths by canonical path, preserving first-seen
/// order (SPEC 14: overlapping inputs or symlinked duplicates of one
/// file resolve to one flow, run once).
pub fn dedup_flows(inputs: &[PathBuf]) -> Result<Vec<(PathBuf, PathBuf)>, ArtifactsError> {
    let mut seen: Vec<(PathBuf, PathBuf)> = Vec::new();
    for input in inputs {
        let canonical = input
            .canonicalize()
            .map_err(|source| ArtifactsError::Canonicalize {
                path: input.clone(),
                source,
            })?;
        if seen.iter().all(|(_, known)| *known != canonical) {
            seen.push((input.clone(), canonical));
        }
    }
    Ok(seen)
}

/// The first 16 hex digits of the SHA-256 of the canonical path string.
pub fn path_hash(canonical: &Path) -> String {
    use std::fmt::Write as _;

    let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
    let mut hex = String::with_capacity(16);
    for byte in &digest[..8] {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// The path without a trailing `.whirl` extension. Any other extension
/// stays.
fn without_whirl_extension(path: &Path) -> PathBuf {
    if path.extension().is_some_and(|ext| ext == "whirl") {
        path.with_extension("")
    } else {
        path.to_path_buf()
    }
}

/// The artifact directory of one flow (SPEC 14): under `cwd`, the
/// canonical path relative to `cwd` without the `.whirl` extension;
/// outside it, `<file stem>-<16-hex-hash>` so absolute paths and `..`
/// segments never escape the artifacts directory. `cwd` must itself be
/// canonical.
pub fn flow_dir(artifacts_dir: &Path, cwd: &Path, canonical: &Path) -> PathBuf {
    if let Ok(relative) = canonical.strip_prefix(cwd) {
        return artifacts_dir.join(without_whirl_extension(relative));
    }
    let stem = without_whirl_extension(canonical)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    artifacts_dir.join(format!("{stem}-{hash}", hash = path_hash(canonical)))
}

/// Maps every input to a unique flow with its artifact directory:
/// canonicalizes, dedups by canonical path (first-seen order), assigns
/// directories, and verifies that no two flows collide (SPEC 14).
pub fn plan_flows(
    artifacts_dir: &Path,
    cwd: &Path,
    inputs: &[PathBuf],
) -> Result<Vec<Flow>, ArtifactsError> {
    let cwd = cwd
        .canonicalize()
        .map_err(|source| ArtifactsError::Canonicalize {
            path: cwd.to_path_buf(),
            source,
        })?;
    let mut flows: Vec<Flow> = Vec::new();
    for (input, canonical) in dedup_flows(inputs)? {
        let dir = flow_dir(artifacts_dir, &cwd, &canonical);
        if let Some(existing) = flows.iter().find(|flow| flow.dir == dir) {
            return Err(ArtifactsError::Collision {
                first: existing.input.clone(),
                second: input,
                dir,
            });
        }
        flows.push(Flow {
            input,
            canonical,
            dir,
        });
    }
    Ok(flows)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::{fs, process, slice};

    use super::*;

    /// A unique temporary directory removed on drop.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!("whirl-artifacts-test-{}-{id}", process::id()));
            fs::create_dir_all(&path).expect("temp dir should be creatable");
            Self { path }
        }

        fn file(&self, name: &str) -> PathBuf {
            let path = self.path.join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("parent dir should be creatable");
            }
            fs::write(&path, "VISIT /\n").expect("temp file should be writable");
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn fixed_artifact_names_match_spec_14() {
        assert_eq!(FAILURE_PNG, "failure.png");
        assert_eq!(TRACE_ZIP, "trace.zip");
        assert_eq!(VIDEO_WEBM, "video.webm");
        assert_eq!(NETWORK_HAR, "network.har");
        assert_eq!(screenshot_file("overview"), "overview.png");
        assert_eq!(snapshot_actual_file("cart"), "snapshot-cart-actual.png");
        assert_eq!(snapshot_diff_file("cart"), "snapshot-cart-diff.png");
    }

    #[test]
    fn snapshot_baselines_live_next_to_the_flow_file() {
        let path = snapshot_baseline_path(Path::new("flows/checkout.whirl"), "cart", "chromium");
        let expected = PathBuf::from("flows/checkout.whirl-snapshots").join(format!(
            "cart-chromium-{platform}.png",
            platform = platform_tag()
        ));
        assert_eq!(path, expected);
    }

    #[test]
    fn a_flow_under_the_cwd_mirrors_its_relative_path() {
        let dir = TempDir::new();
        let flow = dir.file("flows/checkout.whirl");
        let flows =
            plan_flows(Path::new("whirl-artifacts"), &dir.path, &[flow]).expect("mapping succeeds");
        assert_eq!(flows.len(), 1);
        assert_eq!(
            flows[0].dir,
            Path::new("whirl-artifacts").join("flows/checkout")
        );
    }

    #[test]
    fn a_flow_outside_the_cwd_uses_the_stem_hash_form() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        let flow = outside.file("elsewhere/login.whirl");
        let flows = plan_flows(
            Path::new("whirl-artifacts"),
            &dir.path,
            slice::from_ref(&flow),
        )
        .expect("mapping succeeds");
        let canonical = flow.canonicalize().expect("flow file canonicalizes");
        let expected = format!("login-{hash}", hash = path_hash(&canonical));
        assert_eq!(flows[0].dir, Path::new("whirl-artifacts").join(&expected));
        let hash = path_hash(&canonical);
        assert_eq!(hash.len(), 16);
        assert!(hash.chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_duplicates_dedup_to_one_flow() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new();
        let flow = dir.file("checkout.whirl");
        let link = dir.path.join("link.whirl");
        symlink(&flow, &link).expect("symlink should be creatable");
        let flows = plan_flows(Path::new("whirl-artifacts"), &dir.path, &[
            flow.clone(),
            link,
            flow.clone(),
        ])
        .expect("mapping succeeds");
        assert_eq!(flows.len(), 1);
        assert_eq!(flows[0].input, flow);
    }

    #[test]
    fn dedup_preserves_first_seen_order() {
        let dir = TempDir::new();
        let a = dir.file("a.whirl");
        let b = dir.file("b.whirl");
        let flows = plan_flows(Path::new("whirl-artifacts"), &dir.path, &[
            b.clone(),
            a.clone(),
            b.clone(),
        ])
        .expect("mapping succeeds");
        let inputs: Vec<_> = flows.iter().map(|flow| flow.input.clone()).collect();
        assert_eq!(inputs, vec![b, a]);
    }

    #[test]
    fn two_flows_mapping_to_one_directory_is_an_error() {
        let dir = TempDir::new();
        // "checkout.whirl" and "checkout" both map to <artifacts>/checkout.
        let with_ext = dir.file("checkout.whirl");
        let without_ext = dir.file("checkout");
        let error = plan_flows(Path::new("whirl-artifacts"), &dir.path, &[
            with_ext.clone(),
            without_ext.clone(),
        ])
        .expect_err("colliding directories are a runtime error");
        let ArtifactsError::Collision { first, second, dir } = error else {
            panic!("expected Collision");
        };
        assert_eq!(first, with_ext);
        assert_eq!(second, without_ext);
        assert_eq!(dir, Path::new("whirl-artifacts").join("checkout"));
    }

    #[test]
    fn a_missing_input_is_a_canonicalize_error() {
        let dir = TempDir::new();
        let missing = dir.path.join("missing.whirl");
        let error = plan_flows(
            Path::new("whirl-artifacts"),
            &dir.path,
            slice::from_ref(&missing),
        )
        .expect_err("a missing file cannot be canonicalized");
        assert!(matches!(
            error,
            ArtifactsError::Canonicalize { path, .. } if path == missing
        ));
    }
}
