//! Canonical text conversions shared by the parser, CLI, and runtime.

use std::str::FromStr;

use super::{BrowserKind, DialogPolicy, DurationLit, DurationUnit, ReducedMotion, Viewport};

/// Conversion errors describe the required shape without retaining input
/// that could have come from a secret environment variable.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("expected {0}")]
pub(crate) struct InvalidOptionValue(&'static str);

fn decimal(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

impl FromStr for DurationLit {
    type Err = InvalidOptionValue;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let invalid = InvalidOptionValue("a duration like 500ms or 10s");
        let (digits, unit) = if let Some(digits) = text.strip_suffix("ms") {
            (digits, DurationUnit::Milliseconds)
        } else {
            (
                text.strip_suffix('s').ok_or(invalid)?,
                DurationUnit::Seconds,
            )
        };
        Ok(Self {
            amount: decimal(digits).ok_or(invalid)?,
            unit,
        })
    }
}

impl FromStr for Viewport {
    type Err = InvalidOptionValue;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let invalid = InvalidOptionValue("WIDTHxHEIGHT like 1280x800");
        let (width, height) = text.split_once('x').ok_or(invalid)?;
        Ok(Self {
            width:  decimal(width).ok_or(invalid)?,
            height: decimal(height).ok_or(invalid)?,
        })
    }
}

impl BrowserKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Chromium => "chromium",
            Self::Firefox => "firefox",
            Self::Webkit => "webkit",
        }
    }
}

impl FromStr for BrowserKind {
    type Err = InvalidOptionValue;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "chromium" => Ok(Self::Chromium),
            "firefox" => Ok(Self::Firefox),
            "webkit" => Ok(Self::Webkit),
            _ => Err(InvalidOptionValue("chromium, firefox, or webkit")),
        }
    }
}

impl DialogPolicy {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Dismiss => "dismiss",
            Self::Accept => "accept",
        }
    }
}

impl FromStr for DialogPolicy {
    type Err = InvalidOptionValue;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "dismiss" => Ok(Self::Dismiss),
            "accept" => Ok(Self::Accept),
            _ => Err(InvalidOptionValue("dismiss or accept")),
        }
    }
}

impl ReducedMotion {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Reduce => "reduce",
            Self::NoPreference => "no-preference",
        }
    }
}

impl FromStr for ReducedMotion {
    type Err = InvalidOptionValue;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "reduce" => Ok(Self::Reduce),
            "no-preference" => Ok(Self::NoPreference),
            _ => Err(InvalidOptionValue("reduce or no-preference")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_keep_their_unit_and_saturate_millisecond_conversion() {
        assert_eq!(
            "500ms"
                .parse::<DurationLit>()
                .expect("valid duration")
                .millis(),
            500
        );
        assert_eq!(
            "10s"
                .parse::<DurationLit>()
                .expect("valid duration")
                .millis(),
            10_000
        );
        assert_eq!(
            "0ms"
                .parse::<DurationLit>()
                .expect("valid duration")
                .millis(),
            0
        );
        assert_eq!(
            format!("{}s", u64::MAX)
                .parse::<DurationLit>()
                .expect("valid amount")
                .millis(),
            u64::MAX
        );
        for invalid in [
            "10",
            "s",
            "1.5s",
            "+1s",
            "-1s",
            " 1s",
            "18446744073709551616ms",
        ] {
            assert!(invalid.parse::<DurationLit>().is_err(), "{invalid}");
        }
    }

    #[test]
    fn viewports_require_two_unsigned_decimal_dimensions() {
        assert_eq!(
            "1280x800".parse::<Viewport>(),
            Ok(Viewport {
                width:  1280,
                height: 800,
            })
        );
        for invalid in ["1280", "+1280x800", "1280x-800", "1280x800x1", "1.5x2"] {
            assert!(invalid.parse::<Viewport>().is_err(), "{invalid}");
        }
    }

    #[test]
    fn closed_options_round_trip_their_canonical_names() {
        for browser in [
            BrowserKind::Chromium,
            BrowserKind::Firefox,
            BrowserKind::Webkit,
        ] {
            assert_eq!(browser.as_str().parse::<BrowserKind>(), Ok(browser));
        }
        for policy in [DialogPolicy::Dismiss, DialogPolicy::Accept] {
            assert_eq!(policy.as_str().parse::<DialogPolicy>(), Ok(policy));
        }
        for motion in [ReducedMotion::Reduce, ReducedMotion::NoPreference] {
            assert_eq!(motion.as_str().parse::<ReducedMotion>(), Ok(motion));
        }
        assert!("Chromium".parse::<BrowserKind>().is_err());
        assert!("ignore".parse::<DialogPolicy>().is_err());
        assert!("default".parse::<ReducedMotion>().is_err());
    }
}
