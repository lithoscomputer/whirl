//! The check engine (SPEC 9 and 10; ADR `evaluate-checks-in-rust`).
//!
//! A check reads a value from its subject, passes it through filters, and
//! tests it with one predicate. The runner resolves the parsed check into
//! a [`Check`] and hands it each [`Read`]; this module never talks to the
//! browser.

mod filter;
mod json;
mod number;
mod pattern;
mod predicate;
mod types;
mod value;
mod xpath;

use std::fmt;

use chrono::{DateTime, Utc};
pub(crate) use filter::{Charset, DateFormat, Filter, FilterError, Missing, Step};
pub(crate) use json::{JsonQuery, parse as parse_json};
pub(crate) use number::Number;
pub(crate) use pattern::{Pattern, PatternFlags};
pub(crate) use predicate::{Expected, Predicate, bytes_literal};
pub(crate) use types::{
    COMPARE_KEYWORDS, FILTER_KEYWORDS, FilterKind, PredicateKind, StaticType, WORD_PREDICATES,
};
pub(crate) use value::{Value, quote as quote_json};
pub(crate) use xpath::{Markup, XpathQuery, is_xml_content_type};

use self::predicate::Outcome;

/// What a subject gave: a value, or a missing value (SPEC 9.2).
#[derive(Clone, Debug)]
pub(crate) enum Read {
    Value(Value),
    Missing(Missing),
}

/// What filters need besides their input.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ReadContext {
    /// The time of the read, for `daysAfterNow` and `daysBeforeNow`.
    pub(crate) now:    DateTime<Utc>,
    /// How a first `xpath:` filter parses the read: XML for the body of an
    /// XML response, else HTML (SPEC 9.5). Later filters always parse HTML.
    pub(crate) markup: Markup,
}

#[cfg(test)]
impl ReadContext {
    /// A read that no filter parses as XML.
    pub(crate) fn html(now: DateTime<Utc>) -> Self {
        Self {
            now,
            markup: Markup::Html,
        }
    }
}

/// One check, resolved and ready to test values.
#[derive(Clone, Debug)]
pub(crate) struct Check {
    pub(crate) filters:   Vec<Filter>,
    pub(crate) negated:   bool,
    pub(crate) predicate: Predicate,
}

/// Why a check did not pass, with the report's stable code (SPEC 9.7).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Failure {
    pub(crate) code:     FailureCode,
    pub(crate) message:  String,
    /// The check as written, such as `not contains admin`.
    pub(crate) expected: String,
    /// What the check saw.
    pub(crate) actual:   String,
}

/// The report codes of SPEC 9.7 that the engine produces.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FailureCode {
    /// A predicate that does not hold.
    Assert,
    TypeMismatch,
    FilterError,
    MissingValue,
}

impl FailureCode {
    /// The stable report code.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Assert => "assert",
            Self::TypeMismatch => "type-mismatch",
            Self::FilterError => "filter-error",
            Self::MissingValue => "missing-value",
        }
    }
}

impl fmt::Display for FailureCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Check {
    /// The check as written after its subject, such as `not contains x`.
    pub(crate) fn describe(&self) -> String {
        let predicate = self.predicate.describe();
        if self.negated {
            format!("not {predicate}")
        } else {
            predicate
        }
    }

    /// Tests one read of the subject (SPEC 9.7).
    pub(crate) fn evaluate(&self, read: Read, context: ReadContext) -> Result<(), Failure> {
        let value = match apply_filters(&self.filters, read, context) {
            Ok(Read::Value(value)) => value,
            Ok(Read::Missing(missing)) => return self.on_missing(&missing),
            Err(error) => {
                return Err(self.failure(
                    FailureCode::FilterError,
                    error.to_string(),
                    error.to_string(),
                ));
            }
        };
        match self.predicate.test(&value) {
            Outcome::TypeMismatch => Err(self.failure(
                FailureCode::TypeMismatch,
                format!(
                    "{} cannot test a {}",
                    self.predicate.kind().name(),
                    value.value_type()
                ),
                typed(&value),
            )),
            Outcome::Decided {
                holds,
                types_differ,
            } => {
                if holds != self.negated {
                    return Ok(());
                }
                let code = if types_differ && !self.negated {
                    FailureCode::TypeMismatch
                } else {
                    FailureCode::Assert
                };
                if code == FailureCode::TypeMismatch {
                    let message =
                        format!("expected {}, got a {}", self.describe(), value.value_type());
                    return Err(self.failure(code, message, typed(&value)));
                }
                let message = format!("check did not pass: {}", self.describe());
                Err(self.failure(code, message, value.describe()))
            }
        }
    }

    /// SPEC 9.7: a missing value passes only `not exists`, plus negated
    /// predicates on an absent attribute of an element that exists.
    fn on_missing(&self, missing: &Missing) -> Result<(), Failure> {
        let kind = self.predicate.kind();
        let negated_form = self.negated || kind == PredicateKind::Ne;
        let passes = if kind == PredicateKind::Exists {
            self.negated
        } else {
            negated_form && matches!(missing, Missing::AbsentAttribute(_))
        };
        if passes {
            return Ok(());
        }
        Err(self.failure(
            FailureCode::MissingValue,
            missing.to_string(),
            missing.to_string(),
        ))
    }

    fn failure(&self, code: FailureCode, message: String, actual: String) -> Failure {
        Failure {
            code,
            message,
            expected: self.describe(),
            actual,
        }
    }
}

/// A value with its type, for type-mismatch reports: `number 42`.
fn typed(value: &Value) -> String {
    match value {
        Value::Bytes(_) | Value::Date(_) | Value::NodeSet(_) => value.describe(),
        other => format!("{} {}", other.value_type(), other.describe()),
    }
}

/// Runs filters over a read. A missing value passes through without
/// running them (SPEC 9.5).
pub(crate) fn apply_filters(
    filters: &[Filter],
    read: Read,
    context: ReadContext,
) -> Result<Read, FilterError> {
    let mut current = read;
    for (position, filter) in filters.iter().enumerate() {
        let Read::Value(value) = current else {
            return Ok(current);
        };
        let markup = if position == 0 {
            context.markup
        } else {
            Markup::Html
        };
        current = match filter.apply(value, context.now, markup)? {
            Step::Value(value) => Read::Value(value),
            Step::Missing(missing) => Read::Missing(missing),
        };
    }
    Ok(current)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;

    use super::*;

    fn now() -> ReadContext {
        ReadContext::html(
            Utc.with_ymd_and_hms(2026, 9, 26, 12, 0, 0)
                .single()
                .expect("valid date"),
        )
    }

    fn check(filters: Vec<Filter>, negated: bool, predicate: Predicate) -> Check {
        Check {
            filters,
            negated,
            predicate,
        }
    }

    fn eq(expected: Expected) -> Predicate {
        Predicate::Compare(PredicateKind::Eq, expected)
    }

    fn doc() -> Read {
        Read::Value(Value::String(
            r#"{"id":42,"roles":["admin"],"deleted":null}"#.to_owned(),
        ))
    }

    fn json(path: &str) -> Filter {
        Filter::Json(JsonQuery::parse(path).expect("valid query"))
    }

    #[test]
    fn passes_a_typed_json_check() {
        let check = check(
            vec![json("$.id")],
            false,
            eq(Expected::bare("42".to_owned())),
        );
        assert_eq!(check.evaluate(doc(), now()), Ok(()));
    }

    #[test]
    fn reports_a_type_mismatch_for_equality_across_types() {
        let check = check(
            vec![json("$.id")],
            false,
            eq(Expected::Typed(Value::String("42".to_owned()))),
        );
        let failure = check
            .evaluate(doc(), now())
            .expect_err("number is not a string");
        assert_eq!(failure.code, FailureCode::TypeMismatch);
        assert_eq!(failure.expected, r#"== "42""#);
        assert_eq!(failure.actual, "number 42");
    }

    #[test]
    fn negates_predicates() {
        let contains =
            Predicate::Compare(PredicateKind::Contains, Expected::bare("owner".to_owned()));
        assert_eq!(
            check(vec![json("$.roles")], true, contains).evaluate(doc(), now()),
            Ok(())
        );
    }

    #[test]
    fn missing_values_pass_only_not_exists() {
        let missing = || Read::Missing(Missing::NoJsonMatch("$.x".to_owned()));
        let exists = || Predicate::Word(PredicateKind::Exists);
        assert_eq!(
            check(vec![], true, exists()).evaluate(missing(), now()),
            Ok(())
        );
        let failure = check(vec![], false, exists())
            .evaluate(missing(), now())
            .expect_err("fails");
        assert_eq!(failure.code, FailureCode::MissingValue);
        let ne = Predicate::Compare(PredicateKind::Ne, Expected::bare("x".to_owned()));
        let failure = check(vec![], false, ne)
            .evaluate(missing(), now())
            .expect_err("fails");
        assert_eq!(failure.code, FailureCode::MissingValue);
    }

    #[test]
    fn absent_attributes_pass_negated_predicates() {
        let absent = || Read::Missing(Missing::AbsentAttribute("aria-current".to_owned()));
        let ne = Predicate::Compare(PredicateKind::Ne, Expected::Text("page".to_owned()));
        assert_eq!(check(vec![], false, ne).evaluate(absent(), now()), Ok(()));
        let contains =
            Predicate::Compare(PredicateKind::Contains, Expected::Text("page".to_owned()));
        assert_eq!(
            check(vec![], true, contains.clone()).evaluate(absent(), now()),
            Ok(())
        );
        let failure = check(vec![], false, contains)
            .evaluate(absent(), now())
            .expect_err("fails");
        assert_eq!(failure.code, FailureCode::MissingValue);
        let exists = Predicate::Word(PredicateKind::Exists);
        assert!(
            check(vec![], false, exists)
                .evaluate(absent(), now())
                .is_err()
        );
    }

    #[test]
    fn missing_values_skip_filters() {
        let absent = Read::Missing(Missing::AbsentAttribute("n".to_owned()));
        let ne = Predicate::Compare(PredicateKind::Ne, Expected::bare("3".to_owned()));
        assert_eq!(
            check(vec![Filter::ToInt], false, ne).evaluate(absent, now()),
            Ok(())
        );
    }

    #[test]
    fn reports_filter_errors() {
        let failure = check(
            vec![Filter::ToInt],
            false,
            eq(Expected::bare("1".to_owned())),
        )
        .evaluate(Read::Value(Value::String("abc".to_owned())), now())
        .expect_err("toInt fails");
        assert_eq!(failure.code, FailureCode::FilterError);
        assert_eq!(failure.message, r#"toInt: "abc" is not an integer"#);
    }

    #[test]
    fn type_mismatch_ignores_negation() {
        let gt = Predicate::Compare(PredicateKind::Gt, Expected::bare("3".to_owned()));
        let failure = check(vec![], true, gt)
            .evaluate(Read::Value(Value::String("5".to_owned())), now())
            .expect_err("strings do not order");
        assert_eq!(failure.code, FailureCode::TypeMismatch);
    }
}
