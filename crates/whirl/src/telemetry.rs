//! Optional Rust diagnostics on stderr. Set `WHIRL_LOG` to `error`,
//! `warn`, `info`, `debug`, or `trace`; absent or invalid values disable
//! telemetry. This is separate from Playwright's `--trace` artifacts.
//!
//! Only Whirl targets are enabled. Fields contain counts, durations,
//! static command names, and execution states. Never record paths, URLs,
//! variable values, step text, or raw external errors (SPEC 11).

use std::{env, io};

use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{fmt, registry};

pub(crate) fn init() {
    let Some(level) = env::var("WHIRL_LOG")
        .ok()
        .and_then(|value| value.parse::<LevelFilter>().ok())
    else {
        return;
    };
    // A caller that already installed a subscriber retains ownership.
    let _ = registry()
        .with(Targets::new().with_target("whirl", level))
        .with(fmt::layer().with_writer(io::stderr).with_ansi(false))
        .try_init();
}
