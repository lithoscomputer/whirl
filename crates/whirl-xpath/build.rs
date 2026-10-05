//! Stops a build that cannot see the pinned libxml2 with a remedy, instead
//! of the binding errors that an older system libxml2 would cause. Keep the
//! version in step with `scripts/build-libxml2.sh`.

const LIBXML2_VERSION: &str = "2.15.4";

fn main() {
    println!("cargo::rerun-if-env-changed=PKG_CONFIG_PATH");
    let found = pkg_config::Config::new()
        .cargo_metadata(false)
        .env_metadata(false)
        .atleast_version(LIBXML2_VERSION)
        .probe("libxml-2.0");
    if let Err(error) = found {
        panic!(
            "whirl-xpath needs libxml2 {LIBXML2_VERSION} from `mise run build:libxml2`. \
             Build inside the Mise environment, for example `mise exec -- cargo build`. \
             pkg-config said: {error}"
        );
    }
}
