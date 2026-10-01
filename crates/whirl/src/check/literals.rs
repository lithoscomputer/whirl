//! Validation of the literal filter arguments of a parsed file (SPEC 3.1,
//! 9.5, 17.1): every literal regex, `json:` query, `xpath:` expression,
//! date format, and charset label. The same code builds the filters at
//! run time, so a literal that passes `whirl check` cannot fail later.

use whirl_types::FilterKind;

use super::{Charset, DateFormat, JsonQuery, Pattern, PatternFlags, XpathQuery};
use crate::lang::ast::{
    AssertBody, CheckStep, File, FilterArg, FilterSpec, PageCheck, PredicateSpec, Regex,
    RequestField, ResponseField, Span, Subject, Value,
};
use crate::lang::parse::ParseError;

/// Checks every literal filter argument of `file`, in source order, and
/// reports the first invalid one as the parse diagnostic the parser used
/// to produce (SPEC 16). `source` is the file's text, for the diagnostic's
/// source line.
pub(crate) fn validate_literals(file: &File, source: &str) -> Result<(), ParseError> {
    let mut literals = Vec::new();
    for entry in &file.entries {
        if let Some(page) = &entry.page
            && let PageCheck::Matches(regex) = &page.check
        {
            literals.push(Literal::Regex(regex));
        }
        for check in &entry.checks {
            match check {
                CheckStep::Assert(assert) => {
                    if let AssertBody::Check(line) = &assert.body {
                        collect_subject(&line.subject, &mut literals);
                        collect_filters(&line.filters, &mut literals);
                        if let PredicateSpec::Matches(regex) = &line.predicate {
                            literals.push(Literal::Regex(regex));
                        }
                    }
                }
                CheckStep::Capture(capture) => {
                    collect_subject(&capture.subject, &mut literals);
                    collect_filters(&capture.filters, &mut literals);
                }
                CheckStep::Judge(_) => {}
            }
        }
    }
    for literal in literals {
        if let Err((span, message)) = literal.validate() {
            return Err(ParseError::at_span(&file.path, source, span, message));
        }
    }
    Ok(())
}

/// One literal argument and the span the parser reported it at.
enum Literal<'a> {
    Regex(&'a Regex),
    /// A `json:PATH` subject field, reported at the whole token.
    JsonField(&'a Value),
    /// An `xpath:EXPR` subject field, reported at the whole token.
    XpathField(&'a Value),
    /// A filter with one value argument, reported at `span`.
    Filter {
        kind:  FilterKind,
        value: &'a Value,
        span:  Span,
    },
}

impl Literal<'_> {
    fn validate(&self) -> Result<(), (Span, String)> {
        match self {
            Self::Regex(regex) => {
                let flags = PatternFlags {
                    ignore_case: regex.flags.ignore_case,
                    dot_all:     regex.flags.dot_all,
                    multiline:   regex.flags.multiline,
                };
                Pattern::validate(&regex.pattern, flags).map_err(|error| {
                    (
                        regex.span,
                        format!("invalid regex in Unicode mode: {}", error.reason),
                    )
                })
            }
            Self::JsonField(value) => {
                let span = prefixed_span(value.span, "json:");
                validate_value(value, span, FilterKind::Json)
            }
            Self::XpathField(value) => {
                let span = prefixed_span(value.span, "xpath:");
                validate_value(value, span, FilterKind::Xpath)
            }
            Self::Filter { kind, value, span } => validate_value(value, *span, *kind),
        }
    }
}

/// Checks a literal value argument of `kind` at `span`. An interpolated
/// value is checked when the file runs.
fn validate_value(value: &Value, span: Span, kind: FilterKind) -> Result<(), (Span, String)> {
    let Some(literal) = value.as_literal() else {
        return Ok(());
    };
    let outcome = match kind {
        FilterKind::Json => JsonQuery::parse(&literal).map(|_| ()),
        FilterKind::Xpath => XpathQuery::parse(&literal).map(|_| ()),
        FilterKind::ToDate | FilterKind::DateFormat => DateFormat::new(&literal).map(|_| ()),
        FilterKind::CharsetDecode => Charset::from_label(&literal).map(|_| ()),
        _ => Ok(()),
    };
    outcome.map_err(|message| (span, message))
}

/// The span of a whole `prefix:argument` token, from the span of its
/// argument: the parser reports a bad `json:` or `xpath:` subject field
/// under the whole token.
fn prefixed_span(argument: Span, prefix: &str) -> Span {
    let offset = u32::try_from(prefix.len()).unwrap_or(u32::MAX);
    Span {
        line:   argument.line,
        column: argument.column.saturating_sub(offset),
        len:    argument.len.saturating_add(offset),
    }
}

fn collect_subject<'a>(subject: &'a Subject, literals: &mut Vec<Literal<'a>>) {
    match subject {
        Subject::Response {
            field: ResponseField::Json(value),
            ..
        }
        | Subject::Request {
            field: RequestField::Json(value),
            ..
        } => literals.push(Literal::JsonField(value)),
        Subject::Response {
            field: ResponseField::Xpath(value),
            ..
        }
        | Subject::Request {
            field: RequestField::Xpath(value),
            ..
        } => literals.push(Literal::XpathField(value)),
        Subject::Element { .. }
        | Subject::Url
        | Subject::Title
        | Subject::Eval(_)
        | Subject::Response { .. }
        | Subject::Request { .. }
        | Subject::Extract { .. } => {}
    }
}

fn collect_filters<'a>(filters: &'a [FilterSpec], literals: &mut Vec<Literal<'a>>) {
    for filter in filters {
        for arg in &filter.args {
            match arg {
                FilterArg::Regex(regex) => literals.push(Literal::Regex(regex)),
                FilterArg::Value(value) => {
                    // A `json:` or `xpath:` filter is reported at its whole
                    // token, which is the filter's span; the parser reported
                    // a date format or charset label at the value itself.
                    let span = match filter.kind {
                        FilterKind::Json | FilterKind::Xpath => filter.span,
                        _ => value.span,
                    };
                    literals.push(Literal::Filter {
                        kind: filter.kind,
                        value,
                        span,
                    });
                }
                FilterArg::Index(_) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::lang::parse::parse_file;

    fn validate(source: &str) -> Result<(), ParseError> {
        let file = parse_file(Path::new("test.whirl"), source).expect("source should parse");
        validate_literals(&file, source)
    }

    fn validate_err(source: &str) -> ParseError {
        validate(source).expect_err("a literal should be invalid")
    }

    #[test]
    fn json_paths_are_checked_when_literal() {
        let error = validate_err("VISIT /\nASSERT response:x json:$.[ == 1\n");
        assert!(
            error.message.starts_with("invalid JSONPath"),
            "{}",
            error.message
        );
        validate("VISIT /\nASSERT response:r json:\"$[?@.name == 'Ada Lovelace']\" count == 1\n")
            .expect("a valid query passes");
        validate("VISIT /\nASSERT response:r json:{{path}} == 1\n")
            .expect("an interpolated query is checked when the file runs");
    }

    #[test]
    fn xpath_expressions_are_checked_when_literal() {
        for invalid in ["//li[", "count(", "foo()"] {
            let error = validate_err(&format!(
                "VISIT /\nASSERT response:x xpath:\"{invalid}\" exists\n"
            ));
            assert!(
                error.message.starts_with("invalid XPath expression"),
                "{invalid}: {}",
                error.message
            );
        }
    }

    #[test]
    fn regexes_must_be_valid_in_unicode_mode() {
        let error = validate_err("VISIT /\nASSERT url matches /a\\-b/\n");
        assert!(
            error.message.starts_with("invalid regex in Unicode mode"),
            "{}",
            error.message
        );
    }

    #[test]
    fn date_formats_and_charset_labels_are_checked_when_literal() {
        let error = validate_err("VISIT /\nASSERT url toDate \"%Q\" == x\n");
        assert_eq!(error.message, "invalid date format \"%Q\"");
        let error = validate_err("VISIT /\nASSERT url charsetDecode nope == x\n");
        assert_eq!(error.message, "unknown encoding label \"nope\"");
        validate("VISIT /\nASSERT url toDate \"%Y\" dateFormat %+ == x\n")
            .expect("valid formats pass");
    }

    #[test]
    fn the_diagnostic_points_at_the_token_the_parser_pointed_at() {
        // A subject field is reported under the whole `json:` token.
        let error = validate_err("VISIT /\nASSERT response:x json:$.[ == 1\n");
        assert_eq!((error.line, error.column, error.len), (2, 19, 8));
        assert_eq!(error.source_line, "ASSERT response:x json:$.[ == 1");
        assert!(error.expected.is_empty());
        assert_eq!(error.code.as_str(), "parse-error");
        // A filter is reported under its whole token too.
        let error = validate_err("VISIT /\nASSERT url xpath:\"//li[\" exists\n");
        assert_eq!((error.line, error.column, error.len), (2, 12, 13));
        // A date format or a charset label is reported at the value.
        let error = validate_err("VISIT /\nASSERT url charsetDecode nope == x\n");
        assert_eq!((error.line, error.column, error.len), (2, 26, 4));
        // A regex is reported at the literal, in a PAGE line too.
        let error = validate_err("VISIT /\nPAGE matches /\\-/\n");
        assert_eq!((error.line, error.column, error.len), (2, 14, 4));
        let error = validate_err("VISIT /\nCAPTURE a: url regex /\\-/\n");
        assert_eq!((error.line, error.column, error.len), (2, 22, 4));
    }

    #[test]
    fn the_first_invalid_literal_in_source_order_is_reported() {
        let error = validate_err(
            "VISIT /\nASSERT url == x\nCAPTURE a: url toDate \"%Q\"\nASSERT url matches /\\-/\n",
        );
        assert_eq!(error.line, 3);
        let error = validate_err("VISIT /\nASSERT response:r json:$.[ regex /\\-/ exists\n");
        assert!(
            error.message.starts_with("invalid JSONPath"),
            "{}",
            error.message
        );
    }
}
