//! XPath 1.0 queries for the `xpath:` filter (SPEC 9.5), evaluated by
//! libxml2 through the `whirl-xpath` crate.

pub(crate) use whirl_xpath::Markup;
use whirl_xpath::Output;

use super::number::Number;
use super::value::{Value, quote};

/// The largest `f64` below which every whole number is exact.
const EXACT_INTEGER_LIMIT: f64 = 9_007_199_254_740_992.0;

/// An XPath 1.0 expression that libxml2 accepts.
#[derive(Clone, Debug)]
pub(crate) struct XpathQuery(String);

impl XpathQuery {
    /// Checks the expression. An unknown namespace prefix passes, because
    /// only the document can define it.
    pub(crate) fn parse(expression: &str) -> Result<Self, String> {
        whirl_xpath::validate(expression)
            .map_err(|error| format!("invalid XPath expression {}: {error}", quote(expression)))?;
        Ok(Self(expression.to_owned()))
    }

    /// Evaluates the expression on a string, or on UTF-8 bytes.
    pub(crate) fn apply(&self, input: Value, markup: Markup) -> Result<Value, String> {
        let text = match input {
            Value::String(text) => text,
            Value::Bytes(bytes) => {
                String::from_utf8(bytes).map_err(|_| "the bytes are not UTF-8".to_owned())?
            }
            other => return Err(format!("cannot take {}", other.value_type())),
        };
        let output =
            whirl_xpath::evaluate(&text, markup, &self.0).map_err(|error| error.to_string())?;
        Ok(match output {
            Output::NodeSet(nodes) => Value::NodeSet(nodes),
            Output::Boolean(value) => Value::Bool(value),
            Output::Number(value) => {
                Value::Number(number(value).ok_or_else(|| {
                    format!("the result {value} is not a number that JSON can hold")
                })?)
            }
            Output::String(text) => Value::String(text),
        })
    }
}

/// XPath has one number type, a double. A whole number within the exact
/// range is an integer, so `count(…)` gives `3`; any other finite number
/// is a float (SPEC 9.5). NaN and the infinities have no JSON form.
fn number(value: f64) -> Option<Number> {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < EXACT_INTEGER_LIMIT {
        // Adding zero turns -0 into 0.
        return Number::parse(&format!("{}", value + 0.0));
    }
    Number::from_f64(value)
}

/// True when a `Content-Type` value names an XML media type: `text/xml`,
/// `application/xml`, or a type that ends in `+xml` (SPEC 9.5).
pub(crate) fn is_xml_content_type(content_type: &str) -> bool {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    essence == "text/xml" || essence == "application/xml" || essence.ends_with("+xml")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(expression: &str) -> XpathQuery {
        XpathQuery::parse(expression).expect("valid expression")
    }

    fn html(expression: &str, page: &str) -> Result<Value, String> {
        query(expression).apply(Value::String(page.to_owned()), Markup::Html)
    }

    const PAGE: &str = "<ul><li>1</li><li>2</li><li>3</li></ul><p>2.5</p>";

    #[test]
    fn gives_each_result_type() {
        assert_eq!(
            html("//li", PAGE).map(|v| v.describe()),
            Ok("node set of 3".to_owned())
        );
        assert_eq!(
            html("boolean(//p)", PAGE).map(|v| v.describe()),
            Ok("true".to_owned())
        );
        assert_eq!(
            html("string(//p)", PAGE).map(|v| v.describe()),
            Ok("\"2.5\"".to_owned())
        );
    }

    #[test]
    fn writes_whole_numbers_as_integers() {
        let count = html("count(//li)", PAGE).expect("a number");
        assert_eq!(count.text_form().as_deref(), Some("3"));
        assert_eq!(
            html("-0", PAGE).expect("a number").text_form().as_deref(),
            Some("0")
        );
        let fraction = html("number(//p)", PAGE).expect("a number");
        assert_eq!(fraction.text_form().as_deref(), Some("2.5"));
        let large = html("9007199254740992", PAGE).expect("a number");
        assert_eq!(large.text_form().as_deref(), Some("9007199254740992.0"));
    }

    #[test]
    fn rejects_nan_and_infinity() {
        let error = html("number('x')", PAGE).expect_err("NaN has no JSON form");
        assert!(error.contains("NaN"), "{error}");
        assert!(html("1 div 0", PAGE).is_err());
    }

    #[test]
    fn takes_utf8_bytes() {
        let value = query("string(//b)")
            .apply(Value::Bytes(b"<b>caf\xc3\xa9</b>".to_vec()), Markup::Html)
            .expect("UTF-8 bytes");
        assert_eq!(value.text_form().as_deref(), Some("caf\u{e9}"));
        let error = query("//b")
            .apply(Value::Bytes(vec![0xff]), Markup::Html)
            .expect_err("not UTF-8");
        assert_eq!(error, "the bytes are not UTF-8");
    }

    #[test]
    fn rejects_invalid_expressions_with_the_expression() {
        let error = XpathQuery::parse("//li[").expect_err("invalid");
        assert_eq!(
            error,
            "invalid XPath expression \"//li[\": Invalid expression"
        );
    }

    #[test]
    fn recognizes_xml_content_types() {
        for xml in [
            "text/xml",
            "application/xml; charset=utf-8",
            "application/atom+xml",
            "Application/RSS+XML",
        ] {
            assert!(is_xml_content_type(xml), "{xml}");
        }
        for other in ["text/html", "application/json", "application/xml-dtd", ""] {
            assert!(!is_xml_content_type(other), "{other}");
        }
    }
}
