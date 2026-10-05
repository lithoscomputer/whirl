//! The installed bundle's layout under the Whirl data directory
//! (protocol section 8). `whirl install` writes this layout and
//! [`crate::resolve_launch`] reads it.

use std::env;
use std::path::PathBuf;

/// Environment variable naming the built shim entry (protocol section 8).
pub const SHIM_JS_ENV: &str = "WHIRL_SHIM_JS";

/// Whirl's directory under the platform data dir (`dirs::data_dir()`).
pub(crate) const DATA_DIR_NAME: &str = "whirl";
/// Environment override for the Whirl data directory. Used by tests to
/// point shim resolution and `whirl install` at a scratch directory; the
/// value replaces `dirs::data_dir()/whirl` entirely.
pub(crate) const DATA_DIR_ENV: &str = "WHIRL_DATA_DIR";
/// The bundled node executable, relative to the data dir.
pub const BUNDLE_NODE: &str = "bundle/node/bin/node";
/// The bundled shim entry, relative to the data dir.
pub const BUNDLE_SHIM_JS: &str = "bundle/shim/index.js";
/// The Whirl version that installed the bundle, relative to the data dir.
/// A binary only runs a bundle its own version installed, so an upgraded
/// binary never drives a stale shim.
pub const BUNDLE_VERSION_FILE: &str = "bundle/shim/whirl-version";
/// This binary's version, as written to [`BUNDLE_VERSION_FILE`]. Every
/// crate of the workspace shares one version, so this is the `whirl`
/// binary's version too.
pub const WHIRL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The Whirl data directory: the [`DATA_DIR_ENV`] override when set,
/// otherwise `dirs::data_dir()/whirl`. `None` only when the platform has
/// no data directory and no override is set.
pub fn data_dir() -> Option<PathBuf> {
    data_dir_from(
        env::var_os(DATA_DIR_ENV).map(PathBuf::from),
        dirs::data_dir(),
    )
}

fn data_dir_from(env_override: Option<PathBuf>, platform: Option<PathBuf>) -> Option<PathBuf> {
    env_override.or_else(|| platform.map(|dir| dir.join(DATA_DIR_NAME)))
}
