//! Read-only runtime diagnosis with a bounded browser launch.

use std::time::Duration;

use anyhow::{Context as _, bail, ensure};
use serde::Deserialize;
use tokio::fs;
use tokio::process::Command;
use tokio::runtime::Runtime;
use tokio::task::spawn_blocking;
use tokio::time::timeout;
use whirl_shim::{LaunchOrigin, ShimClient, StartFlowParams, ViewportParams, resolve_launch};

use crate::install::{self, Progress};

#[derive(Deserialize)]
struct NodeInfo {
    version:    String,
    executable: String,
}

/// Checks the selected runtime and browser without downloading or repairing
/// files.
pub(crate) fn run(browser: &str, progress: Progress<'_>) -> anyhow::Result<()> {
    let runtime = Runtime::new().context("starting runtime diagnosis")?;
    runtime.block_on(async {
        timeout(Duration::from_secs(30), inspect(browser, progress))
            .await
            .context("runtime diagnosis timed out after 30s; run `whirl install` and retry")?
    })
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// The installed bundle must run its own pinned Node. A development
/// runtime (`WHIRL_NODE`) may run any Node from the pinned major version
/// up, since Playwright supports every current Node release.
fn check_node_version(version: &str, origin: LaunchOrigin) -> anyhow::Result<()> {
    let pinned = format!("v{}", install::NODE_VERSION);
    match origin {
        LaunchOrigin::Bundle => ensure!(
            version == pinned,
            "Node version is '{version}', expected {pinned}; run `whirl install`"
        ),
        LaunchOrigin::Environment => {
            let minimum = major(&pinned).expect("the pinned Node version has a major number");
            ensure!(
                major(version).is_some_and(|found| found >= minimum),
                "Node version is '{version}', expected v{minimum} or later; run `mise install`"
            );
        }
    }
    Ok(())
}

/// The major number of a Node version string such as `v24.19.0`.
fn major(version: &str) -> Option<u32> {
    version.strip_prefix('v')?.split('.').next()?.parse().ok()
}

async fn inspect(browser: &str, progress: Progress<'_>) -> anyhow::Result<()> {
    let launch = spawn_blocking(resolve_launch)
        .await
        .context("checking the shim installation")??;
    ensure!(
        fs::metadata(&launch.shim_js)
            .await
            .is_ok_and(|metadata| metadata.is_file()),
        "shim '{}' is missing; run `whirl install` (development: `mise run dev`)",
        launch.shim_js.display()
    );
    let node = Command::new(&launch.node)
        .args([
            "-p",
            "JSON.stringify({version: process.version, executable: process.execPath})",
        ])
        .kill_on_drop(true)
        .output()
        .await
        .context("cannot start Node; run `whirl install` (development: `mise install`)")?;
    ensure!(
        node.status.success(),
        "Node could not report its version; run `whirl install` (development: `mise install`)"
    );
    let info: NodeInfo = serde_json::from_slice(&node.stdout).context(
        "Node returned invalid runtime details; run `whirl install` (development: `mise install`)",
    )?;
    check_node_version(&info.version, launch.origin)?;
    progress(&format!("Node {}: OK ({})", info.version, info.executable));
    let cli_launch = launch.clone();
    let cli = spawn_blocking(move || install::playwright_cli_for(&cli_launch))
        .await
        .context("finding the Playwright CLI")??;
    let mut client = ShimClient::spawn(&launch)?;
    let result = async {
        let hello = client.hello().await.context("cannot contact the shim; run `whirl install` (development: `mise run dev`)")?;
        ensure!(hello.playwright_version == install::PLAYWRIGHT_VERSION,
            "Playwright version is {}, expected {}; run `whirl install` (development: `mise run setup:shim`)", hello.playwright_version, install::PLAYWRIGHT_VERSION);
        progress(&format!("Shim protocol {}, Playwright {}: OK", hello.protocol, hello.playwright_version));
        let params = StartFlowParams {
            browser: browser.to_owned(), headed: false, connect: None,
            viewport: ViewportParams { width: 1280, height: 720 },
            storage_state_path: None, dialogs: "dismiss".to_owned(), allow_hosts: None, block_hosts: None,
            nav_timeout_ms: 10_000, user_agent: None, reduced_motion: None,
            video: None, har_path: None, trace: false, open_shadow_roots: false, mocks: false,
        };
        if let Err(error) = client.start_flow(&params).await {
            let libraries = if cfg!(target_os = "linux") {
                format!("\nIf system libraries are missing, run: sudo {} {} install-deps {browser}",
                    shell_quote(&info.executable), shell_quote(&cli.to_string_lossy()))
            } else { String::new() };
            bail!("{browser} could not launch: {error}\nInstall its browser build: whirl install {browser}{libraries}");
        }
        progress(&format!("{browser}: browser launch OK"));
        // `--video` needs Playwright's ffmpeg on every engine; browser
        // installs bring it along, so the repair is the same command.
        let Some(ffmpeg) = hello.ffmpeg_path else {
            bail!("Playwright's ffmpeg is missing, so --video cannot record\nInstall it with a browser build: whirl install {browser}");
        };
        progress(&format!("ffmpeg for --video: OK ({ffmpeg})"));
        Ok(())
    }.await;
    let shutdown = client.shutdown().await;
    result?;
    shutdown.context("the diagnostic browser could not shut down cleanly")?;
    progress("Whirl is ready.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_development_runtime_accepts_the_pinned_major_and_later() {
        let pinned = format!("v{}", install::NODE_VERSION);
        assert!(check_node_version(&pinned, LaunchOrigin::Environment).is_ok());
        assert!(check_node_version("v26.0.0", LaunchOrigin::Environment).is_ok());
        let error = check_node_version("v22.12.0", LaunchOrigin::Environment)
            .expect_err("an older major is rejected");
        assert!(error.to_string().contains("or later"), "{error}");
        assert!(check_node_version("node", LaunchOrigin::Environment).is_err());
    }

    #[test]
    fn the_bundle_requires_its_exact_pinned_node() {
        let pinned = format!("v{}", install::NODE_VERSION);
        assert!(check_node_version(&pinned, LaunchOrigin::Bundle).is_ok());
        let error = check_node_version("v26.0.0", LaunchOrigin::Bundle)
            .expect_err("the bundle runs only its pinned Node");
        assert!(error.to_string().contains("whirl install"), "{error}");
    }
}
