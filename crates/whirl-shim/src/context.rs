//! The browser context of one flow (protocol section 3, `startFlow`):
//! the typed options the runner resolved, what the flow records, and
//! the mapping of both to the `startFlow` wire params, which only this
//! module knows.

use std::path::{Path, PathBuf};

use whirl_lang::ast::{BrowserKind, DialogPolicy, ReducedMotion, Viewport};

use crate::{StartFlowParams, VideoParams, ViewportParams};

/// The frame rate of Chromium recordings without `--video-fps` (SPEC 13).
const DEFAULT_VIDEO_FPS: u8 = 60;

/// Where the shim records a video before it moves the recording to its
/// final file at `endFlow`: a scratch directory beside that file.
const VIDEO_TEMP_DIR: &str = "video-temp";

/// A file's browser options after resolution (SPEC 5): the browser
/// context its flow runs in. The runner resolves them from the file's
/// option lines and the command line; [`StartFlowParams::from_options`]
/// maps them to the wire.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrowserOptions {
    pub engine:         BrowserKind,
    pub viewport:       Viewport,
    /// The `base` option; its host is always allowed (SPEC 5).
    pub base:           Option<String>,
    /// As the options set it; `startFlow` adds the `base` host (SPEC 5).
    pub allow_hosts:    Option<Vec<String>>,
    /// Hosts blocked even when `allow_hosts` allows them (SPEC 5).
    pub block_hosts:    Option<Vec<String>>,
    pub dialogs:        DialogPolicy,
    /// The `prefers-reduced-motion` value the page sees; the engine
    /// default when unset (SPEC 5).
    pub reduced_motion: Option<ReducedMotion>,
    /// The storage state the context starts from, as an absolute path
    /// (SPEC 5, 12, 13).
    pub storage:        Option<PathBuf>,
    /// `--headed`: show the browser window.
    pub headed:         bool,
    /// Browser user agent string; the engine default when unset (SPEC 5).
    pub user_agent:     Option<String>,
    pub nav_timeout_ms: u64,
}

impl BrowserOptions {
    /// The `allow-hosts` list the shim enforces: the `base` host is always
    /// allowed (SPEC 5).
    fn shim_allow_hosts(&self) -> Option<Vec<String>> {
        let mut hosts = self.allow_hosts.clone()?;
        if let Some(host) = self.base.as_deref().and_then(url_host) {
            hosts.push(host);
        }
        Some(hosts)
    }
}

/// What a flow records (SPEC 13), as the runner decided it: the files
/// the recordings end up in. `None` records nothing of that kind.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Recording {
    pub video: Option<VideoOutput>,
    /// The HAR network log.
    pub har:   Option<PathBuf>,
    /// Playwright tracing; the runner names the trace file at `endFlow`.
    pub trace: bool,
}

/// A requested video recording: the file it ends up in, and the frame
/// rate the `--video-fps` flag asked for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoOutput {
    pub path: PathBuf,
    /// The `--video-fps` flag; `None` asks for the default rate.
    pub fps:  Option<u8>,
}

/// Context features that a file's lines need (SPEC 7.4, 7.5).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Features {
    /// Open every shadow root that page scripts attach, so `ACT` and
    /// `GOAL` see closed ones (SPEC 7.4).
    pub open_shadow_roots: bool,
    /// Route requests through the flow's mocks (SPEC 7.5): the file uses
    /// `MOCK`.
    pub mocks:             bool,
}

impl StartFlowParams {
    /// The `startFlow` params of one flow (protocol section 3).
    #[must_use]
    pub fn from_options(
        browser: &BrowserOptions,
        recording: &Recording,
        features: &Features,
    ) -> Self {
        Self {
            browser:            browser.engine.as_str().to_owned(),
            headed:             browser.headed,
            viewport:           ViewportParams {
                width:  browser.viewport.width,
                height: browser.viewport.height,
            },
            storage_state_path: browser.storage.as_deref().map(wire_path),
            dialogs:            browser.dialogs.as_str().to_owned(),
            allow_hosts:        browser.shim_allow_hosts(),
            block_hosts:        browser.block_hosts.clone(),
            nav_timeout_ms:     browser.nav_timeout_ms,
            user_agent:         browser.user_agent.clone(),
            reduced_motion:     browser
                .reduced_motion
                .map(|motion| motion.as_str().to_owned()),
            video:              recording.video.as_ref().map(|video| VideoParams {
                temp_dir:   wire_path(&video.path.with_file_name(VIDEO_TEMP_DIR)),
                final_path: wire_path(&video.path),
                // Only Chromium has the screencast recorder; the other
                // engines keep Playwright's recorder at its fixed rate
                // (SPEC 13).
                fps:        (browser.engine == BrowserKind::Chromium)
                    .then(|| video.fps.unwrap_or(DEFAULT_VIDEO_FPS)),
            }),
            har_path:           recording.har.as_deref().map(wire_path),
            trace:              recording.trace,
            open_shadow_roots:  features.open_shadow_roots,
            mocks:              features.mocks,
        }
    }
}

/// The hostname of a URL, textually: scheme and userinfo stripped, cut
/// at the first `/`, `?`, or `#`, port removed (IPv6 brackets kept
/// textual per SPEC 5). `None` when the URL has no host (`data:`).
fn url_host(url: &str) -> Option<String> {
    // Without `://` the URL is opaque (`data:`) and has no host.
    let (_, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = if let Some(end) = host_port.strip_prefix('[') {
        // IPv6 literal: keep the bracketed text without the port.
        end.split_once(']').map_or(host_port, |(ip, _)| ip)
    } else {
        host_port.split(':').next().unwrap_or_default()
    };
    (!host.is_empty()).then(|| host.to_owned())
}

/// Flow paths are canonical; CLI paths are made absolute during run
/// preparation. Rendering a wire path performs no filesystem access.
fn wire_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> BrowserOptions {
        BrowserOptions {
            engine:         BrowserKind::Chromium,
            viewport:       Viewport {
                width:  1280,
                height: 720,
            },
            base:           None,
            allow_hosts:    None,
            block_hosts:    None,
            dialogs:        DialogPolicy::Dismiss,
            reduced_motion: None,
            storage:        None,
            headed:         false,
            user_agent:     None,
            nav_timeout_ms: 30_000,
        }
    }

    #[test]
    fn the_defaults_map_to_the_minimal_start_flow() {
        let params =
            StartFlowParams::from_options(&options(), &Recording::default(), &Features::default());
        assert_eq!(params, StartFlowParams {
            browser:            "chromium".to_owned(),
            headed:             false,
            viewport:           ViewportParams {
                width:  1280,
                height: 720,
            },
            storage_state_path: None,
            dialogs:            "dismiss".to_owned(),
            allow_hosts:        None,
            block_hosts:        None,
            nav_timeout_ms:     30_000,
            user_agent:         None,
            reduced_motion:     None,
            video:              None,
            har_path:           None,
            trace:              false,
            open_shadow_roots:  false,
            mocks:              false,
        });
    }

    #[test]
    fn every_option_reaches_the_wire() {
        let browser = BrowserOptions {
            engine:         BrowserKind::Webkit,
            viewport:       Viewport {
                width:  800,
                height: 600,
            },
            base:           Some("https://user:pw@shop.test:8443/path?q#f".to_owned()),
            allow_hosts:    Some(vec!["*.cdn.test".to_owned()]),
            block_hosts:    Some(vec!["*.analytics.test".to_owned()]),
            dialogs:        DialogPolicy::Accept,
            reduced_motion: Some(ReducedMotion::Reduce),
            storage:        Some(PathBuf::from("/abs/state.json")),
            headed:         true,
            user_agent:     Some("chrome".to_owned()),
            nav_timeout_ms: 5_000,
        };
        let recording = Recording {
            video: Some(VideoOutput {
                path: PathBuf::from("/abs/flow/video.webm"),
                fps:  Some(30),
            }),
            har:   Some(PathBuf::from("/abs/flow/network.har")),
            trace: true,
        };
        let features = Features {
            open_shadow_roots: true,
            mocks:             true,
        };
        let params = StartFlowParams::from_options(&browser, &recording, &features);
        assert_eq!(params, StartFlowParams {
            browser:            "webkit".to_owned(),
            headed:             true,
            viewport:           ViewportParams {
                width:  800,
                height: 600,
            },
            storage_state_path: Some("/abs/state.json".to_owned()),
            dialogs:            "accept".to_owned(),
            allow_hosts:        Some(vec!["*.cdn.test".to_owned(), "shop.test".to_owned()]),
            block_hosts:        Some(vec!["*.analytics.test".to_owned()]),
            nav_timeout_ms:     5_000,
            user_agent:         Some("chrome".to_owned()),
            reduced_motion:     Some("reduce".to_owned()),
            video:              Some(VideoParams {
                temp_dir:   "/abs/flow/video-temp".to_owned(),
                final_path: "/abs/flow/video.webm".to_owned(),
                // WebKit has no screencast recorder (SPEC 13).
                fps:        None,
            }),
            har_path:           Some("/abs/flow/network.har".to_owned()),
            trace:              true,
            open_shadow_roots:  true,
            mocks:              true,
        });
    }

    #[test]
    fn chromium_records_at_the_requested_or_default_rate() {
        let video = |fps| Recording {
            video: Some(VideoOutput {
                path: PathBuf::from("/abs/flow/video.webm"),
                fps,
            }),
            ..Recording::default()
        };
        let fps_of = |recording: &Recording| {
            StartFlowParams::from_options(&options(), recording, &Features::default())
                .video
                .expect("a video was requested")
                .fps
        };
        assert_eq!(fps_of(&video(None)), Some(DEFAULT_VIDEO_FPS));
        assert_eq!(fps_of(&video(Some(24))), Some(24));
    }

    #[test]
    fn the_base_host_joins_the_allow_list_only_when_one_is_set() {
        let mut browser = options();
        browser.base = Some("https://shop.test/".to_owned());
        let params =
            StartFlowParams::from_options(&browser, &Recording::default(), &Features::default());
        assert_eq!(params.allow_hosts, None);
        browser.allow_hosts = Some(Vec::new());
        let params =
            StartFlowParams::from_options(&browser, &Recording::default(), &Features::default());
        assert_eq!(params.allow_hosts, Some(vec!["shop.test".to_owned()]));
    }

    #[test]
    fn url_host_is_textual() {
        assert_eq!(
            url_host("https://shop.test/x").as_deref(),
            Some("shop.test")
        );
        assert_eq!(
            url_host("http://user:pw@shop.test:8080?q").as_deref(),
            Some("shop.test")
        );
        assert_eq!(url_host("http://[::1]:3000/").as_deref(), Some("::1"));
        assert_eq!(url_host("data:text/html,hi"), None);
        assert_eq!(url_host("https:///path"), None);
    }
}
