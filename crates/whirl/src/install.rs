//! `whirl install` (SPEC section 13): provisions the shim bundle and the
//! browser builds so users need no Node of their own.
//!
//! The bundle lives under the Whirl data directory
//! ([`shim::whirl_data_dir`]) in the layout `run/shim.rs` resolves:
//! `bundle/node/` holds the pinned Node runtime, `bundle/bun/` holds the
//! pinned Bun binary that installs dependencies, and `bundle/shim/`
//! holds the shim entry `index.js`, its sibling dist files, a
//! `package.json`, and a `node_modules` tree with the pinned
//! `@playwright/test`.
//!
//! Every step is idempotent: re-running `whirl install` refreshes the
//! bundle in place and resumes after an interrupted attempt. Playwright
//! keeps the browser builds in its own default cache; Whirl does not
//! relocate them.
//!
//! The shim source is the dist tree embedded at compile time when
//! `shim/dist` was built before this binary (see `build.rs`); a binary
//! built without it falls back to the tree next to `WHIRL_SHIM_JS`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use std::{env, fs, io};

use anyhow::{Context as _, bail};
use flate2::read::GzDecoder;
use reqwest::blocking::{Client, Response};
use sha2::{Digest as _, Sha256};

use crate::run::shim;

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/shim_embed.rs"));
}

/// The pinned Node runtime version (no `v` prefix).
pub const NODE_VERSION: &str = "24.19.0";
/// The pinned Playwright version, matching `shim/package.json`.
pub const PLAYWRIGHT_VERSION: &str = "1.62.1";
/// The pinned Bun version used to install the bundle's dependencies
/// (Bun package-management decision). Bun is not a runtime here: the
/// bundle always runs on the pinned Node.
pub const BUN_VERSION: &str = "1.4.0";
/// Where the pinned Node release tarballs and checksums live.
const NODE_DIST_BASE: &str = "https://nodejs.org/dist";
/// Where the pinned Bun release archives and checksums live.
const BUN_RELEASE_BASE: &str = "https://github.com/oven-sh/bun/releases/download";

/// Progress reporting: one human-readable line per call.
pub type Progress<'a> = &'a mut dyn FnMut(&str);

/// Runs the full provisioning: Node runtime, shim files, Bun binary,
/// dependencies, and browser builds. Each step prints a progress line
/// and is safe to re-run.
pub fn run(progress: Progress<'_>) -> anyhow::Result<()> {
    let data_dir = shim::whirl_data_dir()
        .context("no data directory on this platform; set WHIRL_DATA_DIR to choose one")?;
    let bundle = BundleLayout::new(&data_dir);
    provision_node(&bundle, progress)?;
    provision_shim_files(&bundle, progress)?;
    provision_bun(&bundle, progress)?;
    provision_dependencies(&bundle, progress)?;
    provision_browsers(&bundle, progress)?;
    progress(&format!(
        "whirl install complete: bundle at {}",
        bundle.root.display()
    ));
    Ok(())
}

/// The on-disk layout of the installed bundle, matching the constants
/// `run/shim.rs` resolves.
struct BundleLayout {
    /// `<data dir>` — the directory containing `bundle/`.
    root:     PathBuf,
    /// `<data dir>/bundle/node` — the unpacked Node runtime.
    node_dir: PathBuf,
    /// `<data dir>/bundle/bun` — the pinned Bun binary (installer only).
    bun_dir:  PathBuf,
    /// `<data dir>/bundle/shim` — shim JS, package.json, node_modules.
    shim_dir: PathBuf,
}

impl BundleLayout {
    fn new(data_dir: &Path) -> Self {
        Self {
            root:     data_dir.to_path_buf(),
            node_dir: data_dir.join("bundle/node"),
            bun_dir:  data_dir.join("bundle/bun"),
            shim_dir: data_dir.join("bundle/shim"),
        }
    }

    /// The bundled node executable; equals
    /// `<data dir>/<`[`shim::BUNDLE_NODE`]`>`.
    fn node_bin(&self) -> PathBuf {
        self.node_dir.join("bin/node")
    }

    /// The bundled Bun executable, used only to install the bundle's
    /// dependencies.
    fn bun_bin(&self) -> PathBuf {
        self.bun_dir.join("bun")
    }

    /// Playwright's CLI entry inside the installed dependencies.
    fn playwright_cli(&self) -> PathBuf {
        self.shim_dir.join("node_modules/@playwright/test/cli.js")
    }
}

/// Step (a): the pinned Node runtime. Skipped when the bundled node
/// already reports the pinned version; otherwise the tarball is
/// downloaded from nodejs.org, verified against `SHASUMS256.txt`, and
/// unpacked into place through a staging directory so an interrupted
/// attempt never leaves a half-written `bundle/node`.
fn provision_node(bundle: &BundleLayout, progress: Progress<'_>) -> anyhow::Result<()> {
    if node_version_matches(&bundle.node_bin()) {
        progress(&format!("Node v{NODE_VERSION}: already installed"));
        return Ok(());
    }
    let archive = node_archive_name(env::consts::OS, env::consts::ARCH).context(
        "installing the Node runtime: unsupported platform; \
         install is currently available on macOS and Linux only",
    )?;
    progress(&format!("Downloading Node v{NODE_VERSION} ({archive})..."));
    let client = http_client()?;
    let dist_dir = format!("{NODE_DIST_BASE}/v{NODE_VERSION}");
    let shasums = fetch_text(&client, &format!("{dist_dir}/SHASUMS256.txt"))
        .context("downloading SHASUMS256.txt from nodejs.org; check network access and retry")?;
    let expected = parse_shasum(&shasums, &archive).with_context(|| {
        format!("no SHASUMS256.txt entry for {archive}; this Whirl build may pin a bad version")
    })?;
    let tarball = fetch_bytes(&client, &format!("{dist_dir}/{archive}"))
        .context("downloading the Node tarball from nodejs.org; check network access and retry")?;
    verify_sha256(&tarball, &expected)
        .with_context(|| format!("verifying {archive}; delete nothing and retry the download"))?;

    progress(&format!("Unpacking Node v{NODE_VERSION}..."));
    let staging = bundle.root.join("bundle/.node-staging");
    replace_dir_with(&staging, |staging| unpack_tar_gz(&tarball, staging))
        .context("unpacking the Node tarball")?;
    let unpacked = staging.join(archive.trim_end_matches(".tar.gz"));
    if !unpacked.join("bin/node").is_file() {
        bail!(
            "the Node tarball did not contain {}/bin/node; retry `whirl install`",
            unpacked.display()
        );
    }
    remove_dir_if_present(&bundle.node_dir).context("replacing the previous Node runtime")?;
    fs::rename(&unpacked, &bundle.node_dir)
        .with_context(|| format!("moving the Node runtime into {}", bundle.node_dir.display()))?;
    remove_dir_if_present(&staging).context("cleaning the Node staging directory")?;
    Ok(())
}

/// True when `node_bin` exists and prints the pinned version.
fn node_version_matches(node_bin: &Path) -> bool {
    let Ok(output) = Command::new(node_bin).arg("--version").output() else {
        return false;
    };
    output.status.success()
        && String::from_utf8_lossy(&output.stdout).trim() == format!("v{NODE_VERSION}")
}

/// Maps `std::env::consts` OS and arch to the official Node tarball
/// name, for example `node-v24.19.0-darwin-arm64.tar.gz`. `None` for
/// platforms Whirl does not provision (Windows included, for now).
fn node_archive_name(os: &str, arch: &str) -> Option<String> {
    let os = match os {
        "macos" => "darwin",
        "linux" => "linux",
        _ => return None,
    };
    let arch = match arch {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        _ => return None,
    };
    Some(format!("node-v{NODE_VERSION}-{os}-{arch}.tar.gz"))
}

/// Finds the lowercase hex SHA-256 for `file_name` in a nodejs.org
/// `SHASUMS256.txt` (`<hex>  <name>` lines).
fn parse_shasum(shasums: &str, file_name: &str) -> Option<String> {
    shasums.lines().find_map(|line| {
        let (hash, name) = line.split_once(char::is_whitespace)?;
        (name.trim() == file_name && !hash.is_empty()).then(|| hash.to_ascii_lowercase())
    })
}

/// Checks `bytes` against a lowercase hex SHA-256.
fn verify_sha256(bytes: &[u8], expected_hex: &str) -> anyhow::Result<()> {
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual == expected_hex {
        Ok(())
    } else {
        bail!("checksum mismatch: expected {expected_hex}, got {actual}");
    }
}

/// A blocking HTTP client with no overall timeout (the Node tarball is
/// large) but a bounded connect.
fn http_client() -> anyhow::Result<Client> {
    Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(None)
        .build()
        .context("building the HTTP client")
}

fn fetch_text(client: &Client, url: &str) -> anyhow::Result<String> {
    fetch_response(client, url)?
        .text()
        .context("reading the response body")
}

fn fetch_bytes(client: &Client, url: &str) -> anyhow::Result<Vec<u8>> {
    let bytes = fetch_response(client, url)?
        .bytes()
        .context("reading the response body")?;
    Ok(bytes.to_vec())
}

fn fetch_response(client: &Client, url: &str) -> anyhow::Result<Response> {
    client
        .get(url)
        .send()
        .and_then(Response::error_for_status)
        .with_context(|| format!("GET {url}"))
}

/// Unpacks a gzipped tarball into `dir`, preserving permissions and
/// symlinks (the Node runtime relies on both).
fn unpack_tar_gz(tarball: &[u8], dir: &Path) -> anyhow::Result<()> {
    tar::Archive::new(GzDecoder::new(tarball))
        .unpack(dir)
        .with_context(|| format!("unpacking into {}", dir.display()))
}

/// Removes `dir` if it exists, recreates it empty, and runs `fill` on
/// it, so a re-run never sees a stale partial tree.
fn replace_dir_with(
    dir: &Path,
    fill: impl FnOnce(&Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    remove_dir_if_present(dir)?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    fill(dir)
}

fn remove_dir_if_present(dir: &Path) -> anyhow::Result<()> {
    match fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {}", dir.display())),
    }
}

/// Step (b): the shim JS files and their `package.json`. The source is
/// the dist tree embedded at build time, or, in a binary built without
/// one, the tree next to `WHIRL_SHIM_JS`. Existing `.js` files in
/// `bundle/shim` are removed first so no stale file survives an
/// upgrade; `node_modules` is left in place for step (c).
fn provision_shim_files(bundle: &BundleLayout, progress: Progress<'_>) -> anyhow::Result<()> {
    let files = shim_source_files()?;
    progress(&format!(
        "Writing the shim ({count} files) to {dir}...",
        count = files.len(),
        dir = bundle.shim_dir.display()
    ));
    fs::create_dir_all(&bundle.shim_dir)
        .with_context(|| format!("creating {}", bundle.shim_dir.display()))?;
    remove_top_level_js(&bundle.shim_dir).context("removing stale shim files")?;
    for (name, contents) in &files {
        let path = bundle.shim_dir.join(name);
        fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;
    }
    let package_json = bundle.shim_dir.join("package.json");
    fs::write(&package_json, bundle_package_json())
        .with_context(|| format!("writing {}", package_json.display()))?;
    Ok(())
}

/// The shim dist tree to install: embedded files when the binary has
/// them, otherwise every `.js` file in the directory of
/// `WHIRL_SHIM_JS` (the built dist tree the entry point lives in).
fn shim_source_files() -> anyhow::Result<Vec<(String, Vec<u8>)>> {
    if !embedded::SHIM_DIST_FILES.is_empty() {
        return Ok(embedded::SHIM_DIST_FILES
            .iter()
            .map(|&(name, contents)| (name.to_owned(), contents.to_vec()))
            .collect());
    }
    let Some(shim_js) = env::var_os(shim::SHIM_JS_ENV).map(PathBuf::from) else {
        bail!(
            "this whirl binary was built without an embedded shim and {env} is not set; \
             build the shim (cd shim && bun install --frozen-lockfile && bun run build) \
             and either rebuild whirl \
             or set {env} to shim/dist/index.js",
            env = shim::SHIM_JS_ENV
        );
    };
    let dist_dir = shim_js.parent().unwrap_or(Path::new("."));
    let mut files = Vec::new();
    let entries = fs::read_dir(dist_dir)
        .with_context(|| format!("reading the shim dist directory {}", dist_dir.display()))?;
    for entry in entries {
        let path = entry.context("reading the shim dist directory")?.path();
        if path.extension().is_none_or(|ext| ext != "js") || !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let contents = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        files.push((name.to_owned(), contents));
    }
    if !files.iter().any(|(name, _)| name == "index.js") {
        bail!(
            "no index.js next to {env}={shim_js}; point {env} at the built shim entry \
             (shim/dist/index.js)",
            env = shim::SHIM_JS_ENV,
            shim_js = shim_js.display()
        );
    }
    files.sort();
    Ok(files)
}

/// Removes the top-level `.js` files of `dir`, leaving subdirectories
/// (notably `node_modules`) alone.
fn remove_top_level_js(dir: &Path) -> anyhow::Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry
            .with_context(|| format!("reading {}", dir.display()))?
            .path();
        if path.is_file() && path.extension().is_some_and(|ext| ext == "js") {
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
    }
    Ok(())
}

/// The bundle's `package.json`: ESM, private, and the pinned
/// `@playwright/test` as its only dependency.
fn bundle_package_json() -> String {
    format!(
        "{{\n  \"name\": \"whirl-shim-bundle\",\n  \"private\": true,\n  \
         \"type\": \"module\",\n  \"dependencies\": {{\n    \
         \"@playwright/test\": \"{PLAYWRIGHT_VERSION}\"\n  }}\n}}\n"
    )
}

/// Step (c): the pinned Bun binary that installs the bundle's
/// dependencies. Skipped when the bundled bun already reports the pinned
/// version; otherwise the release zip is downloaded from GitHub,
/// verified against the release's `SHASUMS256.txt`, and unpacked into
/// place through a staging directory like the Node step.
fn provision_bun(bundle: &BundleLayout, progress: Progress<'_>) -> anyhow::Result<()> {
    if bun_version_matches(&bundle.bun_bin()) {
        progress(&format!("Bun {BUN_VERSION}: already installed"));
        return Ok(());
    }
    let archive = bun_archive_name(env::consts::OS, env::consts::ARCH).context(
        "installing the Bun binary: unsupported platform; \
         install is currently available on macOS and Linux only",
    )?;
    progress(&format!("Downloading Bun {BUN_VERSION} ({archive})..."));
    let client = http_client()?;
    let release_dir = format!("{BUN_RELEASE_BASE}/bun-v{BUN_VERSION}");
    let shasums = fetch_text(&client, &format!("{release_dir}/SHASUMS256.txt")).context(
        "downloading SHASUMS256.txt from the Bun release; check network access and retry",
    )?;
    let expected = parse_shasum(&shasums, &archive).with_context(|| {
        format!("no SHASUMS256.txt entry for {archive}; this Whirl build may pin a bad version")
    })?;
    let zip = fetch_bytes(&client, &format!("{release_dir}/{archive}"))
        .context("downloading the Bun archive from GitHub; check network access and retry")?;
    verify_sha256(&zip, &expected)
        .with_context(|| format!("verifying {archive}; delete nothing and retry the download"))?;

    progress(&format!("Unpacking Bun {BUN_VERSION}..."));
    let staging = bundle.root.join("bundle/.bun-staging");
    replace_dir_with(&staging, |staging| unpack_zip(&zip, staging))
        .context("unpacking the Bun archive")?;
    let unpacked = staging.join(archive.trim_end_matches(".zip"));
    let unpacked_bun = unpacked.join("bun");
    if !unpacked_bun.is_file() {
        bail!(
            "the Bun archive did not contain {}/bun; retry `whirl install`",
            unpacked.display()
        );
    }
    make_executable(&unpacked_bun)?;
    remove_dir_if_present(&bundle.bun_dir).context("replacing the previous Bun binary")?;
    fs::rename(&unpacked, &bundle.bun_dir)
        .with_context(|| format!("moving the Bun binary into {}", bundle.bun_dir.display()))?;
    remove_dir_if_present(&staging).context("cleaning the Bun staging directory")?;
    Ok(())
}

/// True when `bun_bin` exists and prints the pinned version.
fn bun_version_matches(bun_bin: &Path) -> bool {
    let Ok(output) = Command::new(bun_bin).arg("--version").output() else {
        return false;
    };
    output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == BUN_VERSION
}

/// Maps `std::env::consts` OS and arch to the official Bun release zip
/// name, for example `bun-darwin-aarch64.zip`. `None` for platforms
/// Whirl does not provision (Windows included, for now).
fn bun_archive_name(os: &str, arch: &str) -> Option<String> {
    let os = match os {
        "macos" => "darwin",
        "linux" => "linux",
        _ => return None,
    };
    let arch = match arch {
        "aarch64" => "aarch64",
        "x86_64" => "x64",
        _ => return None,
    };
    Some(format!("bun-{os}-{arch}.zip"))
}

/// Unpacks a zip archive into `dir`, preserving permissions where the
/// archive records them.
fn unpack_zip(bytes: &[u8], dir: &Path) -> anyhow::Result<()> {
    zip::ZipArchive::new(io::Cursor::new(bytes))
        .context("reading the zip archive")?
        .extract(dir)
        .with_context(|| format!("unpacking into {}", dir.display()))
}

/// Marks `path` executable (`rwxr-xr-x`). A no-op on non-Unix targets,
/// which the install steps reject earlier anyway.
fn make_executable(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("marking {} executable", path.display()))?;
    }
    Ok(())
}

/// Step (d): `bun install` in the bundle shim directory. Bun is the
/// installer only: the generated `package.json` pins `@playwright/test`,
/// and the resulting `node_modules` tree serves the bundled Node
/// runtime. Skipped when the pinned `@playwright/test` is already
/// installed.
fn provision_dependencies(bundle: &BundleLayout, progress: Progress<'_>) -> anyhow::Result<()> {
    if installed_playwright_version(bundle).as_deref() == Some(PLAYWRIGHT_VERSION) {
        progress(&format!(
            "@playwright/test {PLAYWRIGHT_VERSION}: already installed"
        ));
        return Ok(());
    }
    progress(&format!(
        "Installing @playwright/test {PLAYWRIGHT_VERSION}..."
    ));
    let mut command = Command::new(bundle.bun_bin());
    command.arg("install");
    run_in_shim_dir(bundle, command).context(
        "bun install of @playwright/test failed; check network access and \
         re-run `whirl install`",
    )?;
    if !bundle.playwright_cli().is_file() {
        bail!(
            "bun install finished but {} is missing; re-run `whirl install`",
            bundle.playwright_cli().display()
        );
    }
    Ok(())
}

/// The version field of the installed `@playwright/test`, if any.
fn installed_playwright_version(bundle: &BundleLayout) -> Option<String> {
    let manifest = bundle
        .shim_dir
        .join("node_modules/@playwright/test/package.json");
    let json: serde_json::Value = serde_json::from_str(&fs::read_to_string(manifest).ok()?).ok()?;
    Some(json.get("version")?.as_str()?.to_owned())
}

/// Step (e): the browser builds, via Playwright's own CLI (`playwright
/// install chromium firefox webkit`) run with the bundled Node.
/// Playwright skips builds that are already in its cache, so re-runs
/// are cheap.
fn provision_browsers(bundle: &BundleLayout, progress: Progress<'_>) -> anyhow::Result<()> {
    progress("Installing browser builds (chromium, firefox, webkit)...");
    run_bundle_node(bundle, &bundle.playwright_cli(), &[
        "install", "chromium", "firefox", "webkit",
    ])
    .context(
        "playwright install failed; check network and disk space, then \
         re-run `whirl install`",
    )
}

/// Runs `<bundle node> <script> <args...>` in the bundle shim
/// directory. Output streams through to the user.
fn run_bundle_node(bundle: &BundleLayout, script: &Path, args: &[&str]) -> anyhow::Result<()> {
    let mut command = Command::new(bundle.node_bin());
    command.arg(script).args(args);
    run_in_shim_dir(bundle, command)
}

/// Runs `command` in the bundle shim directory with the bundled node
/// `bin/` first on PATH (Bun's lifecycle scripts and Playwright spawn
/// `node` themselves). Output streams through to the user.
fn run_in_shim_dir(bundle: &BundleLayout, mut command: Command) -> anyhow::Result<()> {
    let bin_dir = bundle.node_dir.join("bin");
    let mut path_entries = vec![bin_dir];
    if let Some(existing) = env::var_os("PATH") {
        path_entries.extend(env::split_paths(&existing));
    }
    let path = env::join_paths(path_entries).context("rebuilding PATH for the bundled node")?;
    let args = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    let description = format!(
        "{program} {args}",
        program = command.get_program().to_string_lossy()
    );
    let status = command
        .current_dir(&bundle.shim_dir)
        .env("PATH", path)
        .status()
        .with_context(|| format!("running {description}"))?;
    if !status.success() {
        bail!("{description} exited with {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_names_cover_macos_and_linux() {
        assert_eq!(
            node_archive_name("macos", "aarch64").as_deref(),
            Some("node-v24.19.0-darwin-arm64.tar.gz")
        );
        assert_eq!(
            node_archive_name("macos", "x86_64").as_deref(),
            Some("node-v24.19.0-darwin-x64.tar.gz")
        );
        assert_eq!(
            node_archive_name("linux", "aarch64").as_deref(),
            Some("node-v24.19.0-linux-arm64.tar.gz")
        );
        assert_eq!(
            node_archive_name("linux", "x86_64").as_deref(),
            Some("node-v24.19.0-linux-x64.tar.gz")
        );
    }

    #[test]
    fn archive_names_reject_unsupported_platforms() {
        assert_eq!(node_archive_name("windows", "x86_64"), None);
        assert_eq!(node_archive_name("linux", "riscv64"), None);
    }

    #[test]
    fn bun_archive_names_cover_macos_and_linux() {
        assert_eq!(
            bun_archive_name("macos", "aarch64").as_deref(),
            Some("bun-darwin-aarch64.zip")
        );
        assert_eq!(
            bun_archive_name("macos", "x86_64").as_deref(),
            Some("bun-darwin-x64.zip")
        );
        assert_eq!(
            bun_archive_name("linux", "aarch64").as_deref(),
            Some("bun-linux-aarch64.zip")
        );
        assert_eq!(
            bun_archive_name("linux", "x86_64").as_deref(),
            Some("bun-linux-x64.zip")
        );
    }

    #[test]
    fn bun_archive_names_reject_unsupported_platforms() {
        assert_eq!(bun_archive_name("windows", "x86_64"), None);
        assert_eq!(bun_archive_name("linux", "riscv64"), None);
    }

    #[test]
    fn shasum_parsing_reads_the_bun_release_format() {
        // Two lines in the exact `<hex>  <name>` shape of the Bun
        // release's SHASUMS256.txt.
        let shasums = "c669e97f6164e1c96e0701748db98dfa77492908cbd8394c7557134a735de381  \
                       bun-darwin-aarch64.zip\n\
                       2d03fb5fb83ac8b567aca0a281b2ce1a1a19d488f56c2968d88c3f25e92fe452  \
                       bun-linux-x64.zip\n";
        assert_eq!(
            parse_shasum(shasums, "bun-linux-x64.zip").as_deref(),
            Some("2d03fb5fb83ac8b567aca0a281b2ce1a1a19d488f56c2968d88c3f25e92fe452")
        );
        assert_eq!(parse_shasum(shasums, "bun-windows-x64.zip"), None);
    }

    #[test]
    fn shasum_parsing_finds_the_named_file() {
        let shasums = "aaaa  node-v24.19.0-darwin-arm64.tar.gz\n\
                       BBBB  node-v24.19.0-linux-x64.tar.gz\n";
        assert_eq!(
            parse_shasum(shasums, "node-v24.19.0-linux-x64.tar.gz").as_deref(),
            Some("bbbb"),
            "hashes normalize to lowercase"
        );
        assert_eq!(parse_shasum(shasums, "node-v24.19.0-win-x64.zip"), None);
    }

    #[test]
    fn zip_unpacking_recreates_the_archived_tree() {
        use std::io::Write as _;
        use std::process;

        use zip::write::SimpleFileOptions;

        let mut writer = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .unix_permissions(0o755);
        writer
            .start_file("bun-test-platform/bun", options)
            .expect("the zip entry should start");
        writer
            .write_all(b"#!/bin/sh\n")
            .expect("the zip entry should be writable");
        let archive = writer
            .finish()
            .expect("the zip archive should finish")
            .into_inner();

        let dir = env::temp_dir().join(format!("whirl-unzip-test-{}", process::id()));
        replace_dir_with(&dir, |dir| unpack_zip(&archive, dir)).expect("the zip should unpack");
        let bun = dir.join("bun-test-platform/bun");
        assert!(bun.is_file(), "the archived file should exist");
        fs::remove_dir_all(&dir).expect("temp dirs should be removable");
    }

    #[test]
    fn sha256_verification_accepts_only_the_expected_digest() {
        // SHA-256 of the empty input.
        let empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        verify_sha256(b"", empty).expect("the empty digest should verify");
        verify_sha256(b"x", empty).expect_err("a wrong digest should fail");
    }

    #[test]
    fn bundle_layout_matches_the_resolver_constants() {
        let bundle = BundleLayout::new(Path::new("/data"));
        assert_eq!(
            bundle.node_bin(),
            Path::new("/data").join(shim::BUNDLE_NODE)
        );
        assert_eq!(
            bundle.shim_dir.join("index.js"),
            Path::new("/data").join(shim::BUNDLE_SHIM_JS)
        );
        assert_eq!(bundle.bun_bin(), Path::new("/data/bundle/bun/bun"));
    }

    #[test]
    fn the_bundle_package_json_pins_playwright() {
        let json: serde_json::Value =
            serde_json::from_str(&bundle_package_json()).expect("package.json should be JSON");
        assert_eq!(
            json["dependencies"]["@playwright/test"],
            serde_json::json!(PLAYWRIGHT_VERSION)
        );
    }
}
