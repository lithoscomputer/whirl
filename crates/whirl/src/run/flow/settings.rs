//! The settings a flow ran with, for reports (SPEC 14).

use std::path::PathBuf;

use whirl_lang::OPTION_KEYS;
use whirl_lang::ast::File;
use whirl_report::model::{SettingReport, SettingSource, SettingValue};

use super::{ResolvedOptions, render_duration_ms};
use crate::run::vars::VarStore;

/// Every setting of the file with its resolved value, masked with the
/// flow's secrets (SPEC 11), and where its value came from. `browsersim-*`
/// settings are validated but inactive in an ordinary run (SPEC 5).
pub(super) fn report(
    file: &File,
    options: &ResolvedOptions,
    vars: &VarStore,
) -> Vec<SettingReport> {
    OPTION_KEYS
        .iter()
        .map(|&key| SettingReport {
            key:    key.to_owned(),
            value:  value_of(key, options).map(|value| mask(value, vars)),
            source: source_of(file, key),
            active: !key.starts_with("browsersim-"),
        })
        .collect()
}

fn source_of(file: &File, key: &str) -> SettingSource {
    if file.command_line_keys.contains(&key) {
        SettingSource::CommandLine
    } else if file.options.iter().any(|line| line.option.key() == key) {
        SettingSource::File
    } else {
        SettingSource::Default
    }
}

fn value_of(key: &str, options: &ResolvedOptions) -> Option<SettingValue> {
    let text = |text: &str| Some(SettingValue::Text(text.to_owned()));
    let path = |path: &Option<PathBuf>| {
        path.as_ref()
            .map(|path| SettingValue::Text(path.to_string_lossy().into_owned()))
    };
    match key {
        "base" => options.base.as_deref().and_then(text),
        "browser" => text(options.browser.as_str()),
        "viewport" => text(&format!(
            "{}x{}",
            options.viewport.width, options.viewport.height
        )),
        "step-timeout" => text(&render_duration_ms(options.step_timeout_ms)),
        "entry-timeout" => options
            .entry_timeout_ms
            .and_then(|ms| text(&render_duration_ms(ms))),
        "nav-timeout" => text(&render_duration_ms(options.nav_timeout_ms)),
        "allow-hosts" => options.allow_hosts.clone().map(SettingValue::List),
        "block-hosts" => options.block_hosts.clone().map(SettingValue::List),
        "dialogs" => text(options.dialogs.as_str()),
        "reduced-motion" => options
            .reduced_motion
            .and_then(|motion| text(motion.as_str())),
        "storage" => path(&options.storage),
        "user-agent" => options.user_agent.as_deref().and_then(text),
        "setup" => path(&options.setup),
        "model" => options.model.as_deref().and_then(text),
        "snapshot-mask" => Some(SettingValue::List(options.snapshot.mask_text().to_vec())),
        "snapshot-max-diff" => text(options.snapshot.max_diff_text()),
        "snapshot-pixel-threshold" => text(&options.snapshot.threshold.to_string()),
        "browsersim-origin" => text(options.browsersim_origin.as_str()),
        _ => None,
    }
}

fn mask(value: SettingValue, vars: &VarStore) -> SettingValue {
    match value {
        SettingValue::Text(text) => SettingValue::Text(vars.mask(&text)),
        SettingValue::List(items) => {
            SettingValue::List(items.iter().map(|item| vars.mask(item)).collect())
        }
    }
}
