//! Validated snapshot options shared by file defaults and individual actions.

use std::fmt;
use std::str::FromStr;

use super::{Locator, OptionValue, Percent, Span};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SnapshotOptionLine {
    pub(crate) option: SnapshotOption,
    pub(crate) line:   u32,
    pub(crate) span:   Span,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SnapshotOption {
    Mask(Option<Locator>),
    MaxDiff(OptionValue<MaxDiff>),
    PixelThreshold(OptionValue<PixelThreshold>),
}

impl SnapshotOption {
    pub(crate) fn key(&self) -> &'static str {
        match self {
            Self::Mask(_) => "snapshot-mask",
            Self::MaxDiff(_) => "snapshot-max-diff",
            Self::PixelThreshold(_) => "snapshot-pixel-threshold",
        }
    }
}

/// Counts and percentages retain their unit through resolution and formatting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MaxDiff {
    Pixels(u64),
    Percent(Percent),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("expected {0}")]
pub(crate) struct InvalidSnapshotValue(&'static str);

impl FromStr for MaxDiff {
    type Err = InvalidSnapshotValue;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let invalid = InvalidSnapshotValue(
            "an integer from 0 to 9007199254740991 or a percentage from 0% to 100%",
        );
        if text.ends_with('%') {
            return Percent::parse(text).map(Self::Percent).ok_or(invalid);
        }
        if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid);
        }
        let count: u64 = text.parse().map_err(|_| invalid)?;
        if count > 9_007_199_254_740_991 {
            return Err(invalid);
        }
        Ok(Self::Pixels(count))
    }
}

impl fmt::Display for MaxDiff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pixels(count) => count.fmt(f),
            Self::Percent(percent) => percent.fmt(f),
        }
    }
}

/// Retain the validated JSON number text, avoiding floating-point equality in
/// the AST.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PixelThreshold(String);

impl FromStr for PixelThreshold {
    type Err = InvalidSnapshotValue;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let invalid = InvalidSnapshotValue("a JSON number from 0 to 1");
        let number: serde_json::Number = serde_json::from_str(text).map_err(|_| invalid)?;
        let value = number.as_f64().ok_or(invalid)?;
        if text.trim() != text || !value.is_finite() || !(0.0..=1.0).contains(&value) {
            return Err(invalid);
        }
        Ok(Self(text.to_owned()))
    }
}

impl PixelThreshold {
    pub(crate) fn value(&self) -> f64 {
        self.0.parse().expect("validated threshold")
    }
}

impl Default for PixelThreshold {
    fn default() -> Self {
        Self("0.2".to_owned())
    }
}

impl fmt::Display for PixelThreshold {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
