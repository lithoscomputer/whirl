//! Snapshot defaults are resolved once; each action overlays its own options.

use serde_json::{Value as Json, json};
use whirl_lang::ast::OptionValue;
use whirl_lang::ast::snapshot::{MaxDiff, PixelThreshold, SnapshotOption};
use whirl_lang::render_snapshot_option;

use super::{OptionsError, resolve_option};
use crate::report::model::SnapshotReport;
use crate::run::flow::render_step_text;
use crate::run::shim::wire;
use crate::run::vars::VarStore;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SnapshotSettings {
    pub(super) masks:     Vec<Json>,
    mask_text:            Vec<String>,
    max_diff:             MaxDiff,
    max_diff_text:        String,
    pub(super) threshold: PixelThreshold,
}

impl Default for SnapshotSettings {
    fn default() -> Self {
        Self {
            masks:         Vec::new(),
            mask_text:     Vec::new(),
            max_diff:      MaxDiff::Pixels(0),
            max_diff_text: "0".to_owned(),
            threshold:     PixelThreshold::default(),
        }
    }
}

impl SnapshotSettings {
    /// Each mask as its locator text, for reports.
    pub(super) fn mask_text(&self) -> &[String] {
        &self.mask_text
    }

    /// The `snapshot-max-diff` value as resolved, for reports.
    pub(super) fn max_diff_text(&self) -> &str {
        &self.max_diff_text
    }

    pub(super) fn with_options<'a>(
        &self,
        options: impl Iterator<Item = (&'a SnapshotOption, u32)>,
        vars: &mut VarStore,
    ) -> Result<Self, OptionsError> {
        let mut settings = self.clone();
        let mut replaced_masks = false;
        for (option, line) in options {
            match option {
                SnapshotOption::Mask(locator) => {
                    if !replaced_masks {
                        settings.masks.clear();
                        settings.mask_text.clear();
                        replaced_masks = true;
                    }
                    if let Some(locator) = locator {
                        settings
                            .masks
                            .push(wire::locator_wire(locator, None, &mut |value| {
                                vars.resolve(value)
                            })?);
                        let text = render_step_text(&render_snapshot_option(option), vars);
                        settings.mask_text.push(
                            text.strip_prefix("snapshot-mask: ")
                                .unwrap_or(&text)
                                .to_owned(),
                        );
                    }
                }
                SnapshotOption::MaxDiff(value) => {
                    settings.max_diff =
                        resolve_option(value, option.key(), line, vars, |text| text.parse().ok())?;
                    // Preserve the resolved spelling so normalization cannot
                    // expose a secret count such as an env value of "001".
                    settings.max_diff_text = match value {
                        OptionValue::Literal(value) => value.to_string(),
                        OptionValue::Interpolated(value) => vars.resolve(value)?,
                    };
                }
                SnapshotOption::PixelThreshold(value) => {
                    settings.threshold =
                        resolve_option(value, option.key(), line, vars, |text| text.parse().ok())?;
                }
            }
        }
        Ok(settings)
    }

    pub(super) fn max_diff_wire(&self) -> Json {
        match &self.max_diff {
            MaxDiff::Pixels(count) => json!({"type": "pixels", "value": count}),
            MaxDiff::Percent(percent) => json!({"type": "percent", "value": percent.value()}),
        }
    }

    /// `target` is the element target's rendered locator text, if any.
    pub(super) fn report(&self, target: Option<&str>, vars: &VarStore) -> SnapshotReport {
        SnapshotReport {
            target:          target.map(|text| vars.mask(text)),
            masks:           self.mask_text.iter().map(|text| vars.mask(text)).collect(),
            max_diff:        vars.mask(&self.max_diff_text),
            pixel_threshold: vars.mask(&self.threshold.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use whirl_lang::ast::{ActionKind, FileOption};
    use whirl_lang::parse_file;

    use super::*;

    #[test]
    fn local_settings_override_independently_without_changing_defaults() {
        let file = parse_file(Path::new("test.whirl"), "[Options]\nsnapshot-mask: css:.{{mask}}\nsnapshot-mask: testid:clock\nsnapshot-max-diff: {{limit}}\nsnapshot-pixel-threshold: 0.1\nVISIT /\nSNAPSHOT local\nsnapshot-mask: css:.{{mask}}\nsnapshot-max-diff: {{limit}}\nSNAPSHOT clear\nsnapshot-mask: none\nsnapshot-pixel-threshold: 0\n").expect("flow");
        let mut vars = VarStore::new();
        vars.set_input("mask", "first");
        vars.set_input("limit", "20");
        let defaults = SnapshotSettings::default()
            .with_options(
                file.options.iter().filter_map(|line| match &line.option {
                    FileOption::Snapshot(option) => Some((option, line.line)),
                    _ => None,
                }),
                &mut vars,
            )
            .expect("file settings");
        vars.set_input("mask", "second");
        vars.set_input("limit", "0.125%");
        let ActionKind::Snapshot { options, .. } = &file.entries[0].actions[1].kind else {
            panic!("snapshot");
        };
        let local = defaults
            .with_options(
                options.iter().map(|line| (&line.option, line.line)),
                &mut vars,
            )
            .expect("local settings");
        assert_eq!(
            local.max_diff_wire(),
            json!({"type":"percent", "value":0.125})
        );
        assert_eq!(local.masks, vec![
            json!([{"type":"css", "selector":".second"}])
        ]);
        assert_eq!(local.report(None, &vars), SnapshotReport {
            target:          None,
            masks:           vec!["css:.second".to_owned()],
            max_diff:        "0.125%".to_owned(),
            pixel_threshold: "0.1".to_owned(),
        });
        assert_eq!(
            defaults.max_diff_wire(),
            json!({"type":"pixels", "value":20})
        );
        assert_eq!(defaults.report(None, &vars).masks, [
            "css:.first",
            "testid:clock"
        ]);
        let ActionKind::Snapshot { options, .. } = &file.entries[0].actions[2].kind else {
            panic!("snapshot");
        };
        let cleared = defaults
            .with_options(
                options.iter().map(|line| (&line.option, line.line)),
                &mut vars,
            )
            .expect("cleared masks");
        assert!(cleared.masks.is_empty());
        assert_eq!(cleared.max_diff_wire(), defaults.max_diff_wire());
        assert_eq!(cleared.threshold.to_string(), "0");
    }

    #[test]
    fn settings_validate_interpolation_and_mask_original_numeric_spelling() {
        let file = parse_file(Path::new("test.whirl"), "VISIT /\nSNAPSHOT secret\nsnapshot-max-diff: {{limit}}\nsnapshot-mask: testid:{{mask}}\n").expect("flow");
        let ActionKind::Snapshot { options, .. } = &file.entries[0].actions[1].kind else {
            panic!("snapshot");
        };
        let mut vars = VarStore::new();
        vars.set_input("limit", "0001");
        vars.set_input("mask", "secret-id");
        vars.record_secret("0001");
        vars.record_secret("secret-id");
        let defaults = SnapshotSettings::default();
        let settings = defaults
            .with_options(
                options.iter().map(|line| (&line.option, line.line)),
                &mut vars,
            )
            .expect("valid settings");
        assert_eq!(
            settings.max_diff_wire(),
            json!({"type":"pixels", "value":1})
        );
        assert_eq!(settings.report(None, &vars).max_diff, "***");
        assert_eq!(settings.report(None, &vars).masks, ["testid:***"]);
        vars.set_input("limit", "-1");
        let error = defaults
            .with_options(
                options.iter().map(|line| (&line.option, line.line)),
                &mut vars,
            )
            .expect_err("invalid count");
        assert!(matches!(error, OptionsError::InvalidValue {
            key: "snapshot-max-diff",
            line: 3,
            ..
        }));
    }
}
