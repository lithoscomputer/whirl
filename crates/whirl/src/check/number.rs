//! Exact JSON numbers (SPEC 3.1, 9.3).
//!
//! A [`Number`] keeps the JSON text it came from, so an integer larger
//! than 2^53 compares, captures, and reports exactly. Comparison works on
//! a normalized decimal (sign, significant digits, and a power of ten), so
//! `3` equals `3.0` and `1e3` equals `1000` without floating point.

use std::cmp::Ordering;
use std::fmt;

/// The largest number of integer digits [`Number::truncate`] writes. A
/// JSON exponent can ask for more digits than any real value needs.
const MAX_INTEGER_DIGITS: usize = 4096;

/// A JSON number with its exact text.
#[derive(Clone, Debug)]
pub(crate) struct Number {
    text:  String,
    float: bool,
}

impl Number {
    /// Parses text in the JSON number grammar (RFC 8259 section 6). A
    /// number with a fraction or an exponent is a float.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let parts = Parts::split(text)?;
        let float = parts.fraction.is_some() || parts.exponent.is_some();
        Some(Self {
            text: text.to_owned(),
            float,
        })
    }

    /// An integer from a machine integer.
    pub(crate) fn integer(value: i64) -> Self {
        Self {
            text:  value.to_string(),
            float: false,
        }
    }

    /// A float from a finite `f64`, written as the shortest text that turns
    /// back into the same `f64`, with at least one fractional digit (SPEC
    /// 9.3). Non-finite values have no JSON form.
    pub(crate) fn from_f64(value: f64) -> Option<Self> {
        if !value.is_finite() {
            return None;
        }
        let magnitude = value.abs();
        let mut text = if magnitude != 0.0 && !(1e-5..1e16).contains(&magnitude) {
            format!("{value:e}")
        } else {
            format!("{value}")
        };
        if !text.contains(['.', 'e']) {
            text.push_str(".0");
        }
        Some(Self { text, float: true })
    }

    /// The number's exact JSON text.
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// True for a number with a fraction or an exponent, or a `toFloat`
    /// result.
    pub(crate) fn is_float(&self) -> bool {
        self.float
    }

    /// The same value as a float, for `toFloat`.
    pub(crate) fn to_float(&self) -> Option<Self> {
        if self.float {
            return Some(self.clone());
        }
        let value: f64 = self.text.parse().ok()?;
        Self::from_f64(value)
    }

    /// The integer part, truncated toward zero (`toInt`). `None` when the
    /// integer would need more than [`MAX_INTEGER_DIGITS`] digits.
    pub(crate) fn truncate(&self) -> Option<Self> {
        let decimal = self.decimal();
        let digits = if decimal.exponent >= 0 {
            let zeros = usize::try_from(decimal.exponent).ok()?;
            if decimal.digits.len().saturating_add(zeros) > MAX_INTEGER_DIGITS {
                return None;
            }
            let mut digits = decimal.digits.clone();
            digits.push_str(&"0".repeat(zeros));
            digits
        } else {
            let drop = usize::try_from(decimal.exponent.unsigned_abs()).unwrap_or(usize::MAX);
            let keep = decimal.digits.len().saturating_sub(drop);
            decimal.digits[..keep].to_owned()
        };
        let text = if digits.is_empty() {
            "0".to_owned()
        } else if decimal.negative {
            format!("-{digits}")
        } else {
            digits
        };
        Some(Self { text, float: false })
    }

    /// The number as an `i64`, when it is an integer in range. Used for
    /// list indexes.
    pub(crate) fn to_i64(&self) -> Option<i64> {
        let truncated = self.truncate()?;
        if truncated.decimal() != self.decimal() {
            return None;
        }
        truncated.text.parse().ok()
    }

    /// Compares two numbers by value.
    pub(crate) fn cmp_value(&self, other: &Self) -> Ordering {
        self.decimal().cmp(&other.decimal())
    }

    /// The normalized decimal form of the text.
    fn decimal(&self) -> Decimal {
        let parts = Parts::split(&self.text).expect("a Number holds valid JSON number text");
        Decimal::from_parts(&parts)
    }
}

impl PartialEq for Number {
    fn eq(&self, other: &Self) -> bool {
        self.cmp_value(other) == Ordering::Equal
    }
}

impl fmt::Display for Number {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

/// The pieces of JSON number text.
struct Parts<'a> {
    negative: bool,
    integer:  &'a str,
    fraction: Option<&'a str>,
    exponent: Option<&'a str>,
}

impl<'a> Parts<'a> {
    /// Splits text in the JSON number grammar, or returns `None`.
    fn split(text: &'a str) -> Option<Self> {
        let (negative, rest) = match text.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, text),
        };
        let integer_len = rest.bytes().take_while(u8::is_ascii_digit).count();
        let integer = &rest[..integer_len];
        if integer.is_empty() || (integer.len() > 1 && integer.starts_with('0')) {
            return None;
        }
        let mut rest = &rest[integer_len..];
        let mut fraction = None;
        if let Some(after) = rest.strip_prefix('.') {
            let len = after.bytes().take_while(u8::is_ascii_digit).count();
            if len == 0 {
                return None;
            }
            fraction = Some(&after[..len]);
            rest = &after[len..];
        }
        let mut exponent = None;
        if let Some(after) = rest.strip_prefix(['e', 'E']) {
            let sign_len = usize::from(after.starts_with(['+', '-']));
            let len = after[sign_len..]
                .bytes()
                .take_while(u8::is_ascii_digit)
                .count();
            if len == 0 {
                return None;
            }
            exponent = Some(&after[..sign_len + len]);
            rest = &after[sign_len + len..];
        }
        rest.is_empty().then_some(Self {
            negative,
            integer,
            fraction,
            exponent,
        })
    }
}

/// A normalized decimal: `digits × 10^exponent`, where `digits` has no
/// leading or trailing zeros. Zero has no digits and is never negative.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Decimal {
    negative: bool,
    digits:   String,
    exponent: i128,
}

impl Decimal {
    fn from_parts(parts: &Parts<'_>) -> Self {
        let fraction = parts.fraction.unwrap_or("");
        let mut digits = format!("{}{fraction}", parts.integer);
        let written_exponent = parts.exponent.map_or(0, parse_exponent);
        let fraction_len = i128::try_from(fraction.len()).unwrap_or(i128::MAX);
        let mut exponent = written_exponent.saturating_sub(fraction_len);
        let leading = digits.bytes().take_while(|byte| *byte == b'0').count();
        digits.drain(..leading);
        let trailing = digits
            .bytes()
            .rev()
            .take_while(|byte| *byte == b'0')
            .count();
        digits.truncate(digits.len() - trailing);
        exponent = exponent.saturating_add(i128::try_from(trailing).unwrap_or(i128::MAX));
        if digits.is_empty() {
            return Self {
                negative: false,
                digits,
                exponent: 0,
            };
        }
        Self {
            negative: parts.negative,
            digits,
            exponent,
        }
    }

    /// The power of ten just above the most significant digit.
    fn magnitude(&self) -> i128 {
        i128::try_from(self.digits.len())
            .unwrap_or(i128::MAX)
            .saturating_add(self.exponent)
    }

    fn cmp_magnitude(&self, other: &Self) -> Ordering {
        self.magnitude()
            .cmp(&other.magnitude())
            .then_with(|| self.digits.cmp(&other.digits))
    }
}

impl Ord for Decimal {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.digits.is_empty(), other.digits.is_empty()) {
            (true, true) => return Ordering::Equal,
            (true, false) => {
                return if other.negative {
                    Ordering::Greater
                } else {
                    Ordering::Less
                };
            }
            (false, true) => {
                return if self.negative {
                    Ordering::Less
                } else {
                    Ordering::Greater
                };
            }
            (false, false) => {}
        }
        match (self.negative, other.negative) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => self.cmp_magnitude(other),
            (true, true) => other.cmp_magnitude(self),
        }
    }
}

impl PartialOrd for Decimal {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Parses an exponent such as `+12` or `-3`, saturating at the `i128`
/// range. The grammar has already checked the digits.
fn parse_exponent(text: &str) -> i128 {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(digits) => (true, digits),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let magnitude = digits.bytes().fold(0_i128, |value, digit| {
        value
            .saturating_mul(10)
            .saturating_add(i128::from(digit - b'0'))
    });
    if negative { -magnitude } else { magnitude }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn number(text: &str) -> Number {
        Number::parse(text).unwrap_or_else(|| panic!("{text} should parse"))
    }

    #[test]
    fn accepts_the_json_number_grammar() {
        for text in [
            "0", "-0", "7", "-12", "3.14", "1e6", "1E+6", "2.5e-3", "-0.0",
        ] {
            assert!(Number::parse(text).is_some(), "{text}");
        }
    }

    #[test]
    fn rejects_text_outside_the_json_number_grammar() {
        for text in [
            "", "-", "007", "+1", ".5", "1.", "1e", "1e+", "0x10", "1_000", " 1", "NaN",
        ] {
            assert!(Number::parse(text).is_none(), "{text}");
        }
    }

    #[test]
    fn marks_fractions_and_exponents_as_floats() {
        assert!(!number("42").is_float());
        assert!(number("1.0").is_float());
        assert!(number("1e3").is_float());
    }

    #[test]
    fn compares_by_value() {
        assert_eq!(number("3"), number("3.0"));
        assert_eq!(number("1e3"), number("1000"));
        assert_eq!(number("0"), number("-0.0"));
        assert_eq!(number("12.50"), number("1.25e1"));
        assert!(number("-2").cmp_value(&number("-1")).is_lt());
        assert!(number("0.1").cmp_value(&number("0.09")).is_gt());
        assert!(number("-0.5").cmp_value(&number("0")).is_lt());
    }

    #[test]
    fn keeps_large_integers_exact() {
        let big = number("1234567890123456789");
        assert_eq!(big.text(), "1234567890123456789");
        assert_ne!(big, number("1234567890123456788"));
        assert_eq!(big.to_i64(), Some(1_234_567_890_123_456_789));
    }

    #[test]
    fn truncates_toward_zero() {
        assert_eq!(
            number("3.9").truncate().map(|n| n.text().to_owned()),
            Some("3".to_owned())
        );
        assert_eq!(
            number("-3.9").truncate().map(|n| n.text().to_owned()),
            Some("-3".to_owned())
        );
        assert_eq!(
            number("-0.5").truncate().map(|n| n.text().to_owned()),
            Some("0".to_owned())
        );
        assert_eq!(
            number("1.5e2").truncate().map(|n| n.text().to_owned()),
            Some("150".to_owned())
        );
        assert!(number("1e999999").truncate().is_none());
    }

    #[test]
    fn writes_floats_with_a_fractional_digit() {
        assert_eq!(
            Number::from_f64(3.0).map(|n| n.text().to_owned()),
            Some("3.0".to_owned())
        );
        assert_eq!(
            Number::from_f64(2.75).map(|n| n.text().to_owned()),
            Some("2.75".to_owned())
        );
        assert_eq!(
            Number::from_f64(1e300).map(|n| n.text().to_owned()),
            Some("1e300".to_owned())
        );
        assert!(Number::from_f64(f64::NAN).is_none());
    }

    #[test]
    fn only_integral_values_convert_to_i64() {
        assert_eq!(number("2.0").to_i64(), Some(2));
        assert_eq!(number("2.5").to_i64(), None);
        assert_eq!(number("-1").to_i64(), Some(-1));
    }

    proptest! {
        #[test]
        fn integer_order_matches_i64_order(a in any::<i64>(), b in any::<i64>()) {
            prop_assert_eq!(Number::integer(a).cmp_value(&Number::integer(b)), a.cmp(&b));
        }

        #[test]
        fn float_order_matches_f64_order(a in -1e12_f64..1e12, b in -1e12_f64..1e12) {
            let (Some(x), Some(y)) = (Number::from_f64(a), Number::from_f64(b)) else {
                return Err(TestCaseError::fail("finite floats convert"));
            };
            prop_assert_eq!(Some(x.cmp_value(&y)), a.partial_cmp(&b));
        }

        #[test]
        fn float_text_round_trips(value in any::<f64>().prop_filter("finite", |v| v.is_finite())) {
            let number = Number::from_f64(value).ok_or_else(|| TestCaseError::fail("finite"))?;
            prop_assert!(Number::parse(number.text()).is_some(), "{}", number.text());
            prop_assert_eq!(number.text().parse::<f64>().ok(), Some(value));
        }
    }
}
