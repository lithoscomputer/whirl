//! Conversion of AST nodes to the shim wire JSON of
//! `docs/engineering/shim-protocol.md` sections 4.1-4.4.
//!
//! The wire carries resolved strings, while the AST holds interpolation
//! segments. Every conversion therefore takes a resolver closure that
//! turns a [`Value`] into its resolved string; the runner supplies its
//! variable table (and masking registry) through it, and a resolver
//! error aborts the conversion unchanged.

use serde_json::{Value as Json, json};

use crate::lang::ast::{
    AssertBody, CaptureSource, DefaultEngine, Extractor, Locator, NumOp, PageCheck, Regex,
    ResponseField, SegmentKind, StateCheck, StrCheck, TextPrefix, Value, ValueSource,
};

/// Resolves a value to its final string; `E` is the runner's error.
pub type Resolve<'a, E> = dyn FnMut(&Value) -> Result<String, E> + 'a;

/// The canonical flag string of a regex literal: `i`, `s`, `m` in that
/// order.
fn regex_flags(regex: &Regex) -> String {
    let mut flags = String::new();
    if regex.flags.ignore_case {
        flags.push('i');
    }
    if regex.flags.dot_all {
        flags.push('s');
    }
    if regex.flags.multiline {
        flags.push('m');
    }
    flags
}

/// The wire pattern of a regex literal: as written, except that the
/// `\/` escape (which only exists to end-delimit `/pattern/`) becomes a
/// plain `/`, matching the protocol examples.
fn regex_source(regex: &Regex) -> String {
    let mut out = String::new();
    let mut chars = regex.pattern.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('/') => out.push('/'),
            Some(next) => {
                out.push('\\');
                out.push(next);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn regex_json(regex: &Regex) -> Json {
    json!({"source": regex_source(regex), "flags": regex_flags(regex)})
}

/// Converts a locator to the wire array of protocol section 4.1.
/// `default_engine` names the engine an unprefixed segment selects; it
/// must be present when the locator can hold one (actions, SPEC 6.1).
pub fn locator_wire<E>(
    locator: &Locator,
    default_engine: Option<DefaultEngine>,
    resolve: &mut Resolve<'_, E>,
) -> Result<Json, E> {
    let mut segments = Vec::with_capacity(locator.segments.len());
    for segment in &locator.segments {
        segments.push(segment_wire(&segment.kind, default_engine, resolve)?);
    }
    Ok(Json::Array(segments))
}

fn text_engine_type(prefix: TextPrefix) -> &'static str {
    match prefix {
        TextPrefix::Label => "label",
        TextPrefix::Placeholder => "placeholder",
        TextPrefix::Text => "text",
        TextPrefix::Alt => "alt",
        TextPrefix::Title => "title",
    }
}

fn segment_wire<E>(
    kind: &SegmentKind,
    default_engine: Option<DefaultEngine>,
    resolve: &mut Resolve<'_, E>,
) -> Result<Json, E> {
    let json = match kind {
        SegmentKind::Role {
            substring,
            role,
            name,
        } => {
            let name = match name {
                Some(name) => Json::String(resolve(name)?),
                None => Json::Null,
            };
            json!({"type": "role", "role": role, "name": name, "exact": !substring})
        }
        SegmentKind::TextEngine {
            prefix,
            substring,
            value,
        } => {
            json!({"type": text_engine_type(*prefix), "text": resolve(value)?, "exact": !substring})
        }
        SegmentKind::TestId(value) => json!({"type": "testid", "id": resolve(value)?}),
        SegmentKind::Css(value) => json!({"type": "css", "selector": resolve(value)?}),
        SegmentKind::Frame(value) => json!({"type": "frame", "selector": resolve(value)?}),
        SegmentKind::Nth(index) => json!({"type": "nth", "index": index}),
        SegmentKind::Default(value) => {
            let engine = default_engine
                .expect("a default-engine segment only parses in actions, which have an engine");
            let engine_type = match engine {
                DefaultEngine::Label => "label",
                DefaultEngine::Text => "text",
            };
            json!({"type": engine_type, "text": resolve(value)?, "exact": true})
        }
    };
    Ok(json)
}

/// Converts a `PAGE` check to the wire expectation of protocol section
/// 4.2, classifying a resolved value per SPEC 8: a value starting with
/// `/` compares the path (or path plus query when it contains `?`); any
/// other value compares the full URL.
pub fn page_wire<E>(check: &PageCheck, resolve: &mut Resolve<'_, E>) -> Result<Json, E> {
    let json = match check {
        PageCheck::Value(value) => {
            let resolved = resolve(value)?;
            let kind = if !resolved.starts_with('/') {
                "url"
            } else if resolved.contains('?') {
                "pathQuery"
            } else {
                "path"
            };
            json!({"kind": kind, "value": resolved})
        }
        PageCheck::Matches(regex) => {
            json!({"kind": "regex", "source": regex_source(regex), "flags": regex_flags(regex)})
        }
    };
    Ok(json)
}

fn str_check_wire<E>(check: &StrCheck, resolve: &mut Resolve<'_, E>) -> Result<Json, E> {
    let json = match check {
        StrCheck::Eq(value) => json!({"op": "==", "value": resolve(value)?}),
        StrCheck::Ne(value) => json!({"op": "!=", "value": resolve(value)?}),
        StrCheck::Contains(value) => json!({"op": "contains", "value": resolve(value)?}),
        StrCheck::Matches(regex) => {
            json!({"op": "matches", "source": regex_source(regex), "flags": regex_flags(regex)})
        }
    };
    Ok(json)
}

fn state_text(state: StateCheck) -> &'static str {
    match state {
        StateCheck::Visible => "visible",
        StateCheck::Hidden => "hidden",
        StateCheck::Enabled => "enabled",
        StateCheck::Disabled => "disabled",
        StateCheck::Checked => "checked",
        StateCheck::Unchecked => "unchecked",
        StateCheck::Focused => "focused",
    }
}

fn num_op_text(op: NumOp) -> &'static str {
    match op {
        NumOp::Eq => "==",
        NumOp::Ne => "!=",
        NumOp::Lt => "<",
        NumOp::Le => "<=",
        NumOp::Gt => ">",
        NumOp::Ge => ">=",
    }
}

fn locator_subject<E>(locator: &Locator, resolve: &mut Resolve<'_, E>) -> Result<Json, E> {
    // Asserts and captures never hold default-engine segments (SPEC 6.1).
    let locator = locator_wire(locator, None, resolve)?;
    Ok(json!({"type": "locator", "locator": locator}))
}

fn response_field_wire<E>(field: &ResponseField, resolve: &mut Resolve<'_, E>) -> Result<Json, E> {
    Ok(match field {
        ResponseField::Status => json!({"type": "status"}),
        ResponseField::Header(value) => json!({"type": "header", "name": resolve(value)?}),
        ResponseField::Json(value) => json!({"type": "json", "pointer": resolve(value)?}),
    })
}

/// Converts an assert to the wire spec of protocol section 4.3.
pub fn assert_wire<E>(body: &AssertBody, resolve: &mut Resolve<'_, E>) -> Result<Json, E> {
    let json = match body {
        AssertBody::ResponseStatus { name, op, status } => {
            json!({"subject": {"type": "response", "name": name.text}, "check": {"type": "status", "op": num_op_text(*op), "value": status}})
        }
        AssertBody::ResponseValue { name, field, check } => {
            json!({"subject": {"type": "response", "name": name.text}, "check": {"type": "value", "field": response_field_wire(field, resolve)?, "op": str_check_wire(check, resolve)?}})
        }
        AssertBody::TabClosed { name } => {
            json!({"subject": {"type": "tab", "name": name.text}, "check": {"type": "closed"}})
        }
        AssertBody::ElementState { locator, state } => json!({
            "subject": locator_subject(locator, resolve)?,
            "check": {"type": "state", "state": state_text(*state)},
        }),
        AssertBody::ElementValue {
            locator,
            source,
            check,
        } => {
            let op = str_check_wire(check, resolve)?;
            let check = match source {
                ValueSource::Text => json!({"type": "text", "op": op}),
                ValueSource::Value => json!({"type": "value", "op": op}),
                ValueSource::Attr(name) => json!({"type": "attr", "name": name, "op": op}),
            };
            json!({"subject": locator_subject(locator, resolve)?, "check": check})
        }
        AssertBody::ElementCount { locator, op, count } => json!({
            "subject": locator_subject(locator, resolve)?,
            "check": {"type": "count", "op": num_op_text(*op), "value": count},
        }),
        AssertBody::Url(check) => json!({
            "subject": {"type": "url"},
            "check": {"type": "text", "op": str_check_wire(check, resolve)?},
        }),
        AssertBody::Title(check) => json!({
            "subject": {"type": "title"},
            "check": {"type": "text", "op": str_check_wire(check, resolve)?},
        }),
    };
    Ok(json)
}

/// Converts a capture source to the wire shape of protocol section 4.4.
pub fn capture_source_wire<E>(
    source: &CaptureSource,
    resolve: &mut Resolve<'_, E>,
) -> Result<Json, E> {
    let json = match source {
        CaptureSource::Response { name, field } => {
            json!({"type": "response", "name": name.text, "field": response_field_wire(field, resolve)?})
        }
        CaptureSource::Element { locator, extractor } => {
            let extract = match extractor {
                Extractor::Text => json!({"type": "text"}),
                Extractor::Value => json!({"type": "value"}),
                Extractor::Count => json!({"type": "count"}),
                Extractor::Attr(name) => json!({"type": "attr", "name": name}),
            };
            let locator = locator_wire(locator, None, resolve)?;
            json!({"type": "element", "locator": locator, "extract": extract})
        }
        CaptureSource::Url => json!({"type": "url"}),
        CaptureSource::Title => json!({"type": "title"}),
        CaptureSource::Eval(script) => json!({"type": "eval", "script": resolve(script)?}),
    };
    Ok(json)
}

/// Converts a capture's optional `regex` filter to the wire `filter`
/// param of protocol section 4.4: an object, or JSON `null`.
pub fn filter_wire(filter: Option<&Regex>) -> Json {
    match filter {
        Some(regex) => regex_json(regex),
        None => Json::Null,
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::path::Path;

    use serde_json::json;

    use super::*;
    use crate::lang::ast::{ActionKind, File, ValueSegment};
    use crate::lang::parse::parse_file;

    /// A test resolver: literals pass through and variable references
    /// resolve to `<name>` / `<env:NAME>` markers.
    #[expect(
        clippy::unnecessary_wraps,
        reason = "the resolver contract is fallible; this test resolver never fails"
    )]
    fn resolve(value: &Value) -> Result<String, String> {
        let mut out = String::new();
        for segment in &value.segments {
            match segment {
                ValueSegment::Literal(text) => out.push_str(text),
                ValueSegment::Var(name) => {
                    let _ = write!(out, "<{name}>");
                }
                ValueSegment::EnvVar(name) => {
                    let _ = write!(out, "<env:{name}>");
                }
                ValueSegment::SetupVar(name) => {
                    let _ = write!(out, "<setup:{name}>");
                }
            }
        }
        Ok(out)
    }

    fn parse(source: &str) -> File {
        parse_file(Path::new("test.whirl"), source)
            .unwrap_or_else(|error| panic!("fixture should parse:\n{error}"))
    }

    /// The wire JSON of a CLICK action's locator.
    fn click_locator(locator: &str) -> Json {
        let file = parse(&format!("VISIT /\nCLICK {locator}\n"));
        let action = &file.entries[0].actions[1];
        let ActionKind::Click { target } = &action.kind else {
            panic!("expected CLICK");
        };
        locator_wire(target, action.kind.default_engine(), &mut resolve).unwrap()
    }

    /// The wire JSON of the first assert in `[Asserts]`.
    fn assert_json(line: &str) -> Json {
        let file = parse(&format!("VISIT /\n[Asserts]\n{line}\n"));
        assert_wire(&file.entries[0].asserts[0].body, &mut resolve).unwrap()
    }

    /// The wire JSON of the first capture's source and filter.
    fn capture_json(line: &str) -> (Json, Json) {
        let file = parse(&format!("VISIT /\n[Captures]\n{line}\n"));
        let capture = &file.entries[0].captures[0];
        let source = capture_source_wire(&capture.source, &mut resolve).unwrap();
        (source, filter_wire(capture.filter.as_ref()))
    }

    /// The wire JSON of a PAGE line's expectation.
    fn page_json(rest: &str) -> Json {
        let file = parse(&format!("VISIT /\nPAGE {rest}\n"));
        let page = file.entries[0].page.as_ref().unwrap();
        page_wire(&page.check, &mut resolve).unwrap()
    }

    #[test]
    fn every_segment_type_converts_to_protocol_json() {
        assert_eq!(
            click_locator(
                "role:button \"Sign in\" >> role~:button >> label:Email >> placeholder~:Search \
                 >> text:\"Add to cart\" >> alt:Logo >> title~:Info >> testid:cart-badge \
                 >> css:\".foo > .bar\" >> nth:1"
            ),
            json!([
                {"type": "role", "role": "button", "name": "Sign in", "exact": true},
                {"type": "role", "role": "button", "name": null, "exact": false},
                {"type": "label", "text": "Email", "exact": true},
                {"type": "placeholder", "text": "Search", "exact": false},
                {"type": "text", "text": "Add to cart", "exact": true},
                {"type": "alt", "text": "Logo", "exact": true},
                {"type": "title", "text": "Info", "exact": false},
                {"type": "testid", "id": "cart-badge"},
                {"type": "css", "selector": ".foo > .bar"},
                {"type": "nth", "index": 1},
            ])
        );
        assert_eq!(
            click_locator("label~:mail >> text~:go >> alt~:logo >> title:Info"),
            json!([
                {"type": "label", "text": "mail", "exact": false},
                {"type": "text", "text": "go", "exact": false},
                {"type": "alt", "text": "logo", "exact": false},
                {"type": "title", "text": "Info", "exact": true},
            ])
        );
    }

    #[test]
    fn default_segments_use_the_actions_engine() {
        assert_eq!(
            click_locator("\"Add to cart\""),
            json!([{"type": "text", "text": "Add to cart", "exact": true}])
        );
        let file = parse("VISIT /\nFILL Email alice\n");
        let action = &file.entries[0].actions[1];
        let ActionKind::Fill { target, .. } = &action.kind else {
            panic!("expected FILL");
        };
        assert_eq!(
            locator_wire(target, action.kind.default_engine(), &mut resolve).unwrap(),
            json!([{"type": "label", "text": "Email", "exact": true}])
        );
    }

    #[test]
    fn variable_references_resolve_through_the_closure() {
        assert_eq!(
            click_locator("testid:{{row_id}} >> text:{{env.LABEL}}"),
            json!([
                {"type": "testid", "id": "<row_id>"},
                {"type": "text", "text": "<env:LABEL>", "exact": true},
            ])
        );
    }

    #[test]
    fn a_resolver_error_aborts_the_conversion() {
        let file = parse("VISIT /\nCLICK testid:{{missing}}\n");
        let ActionKind::Click { target } = &file.entries[0].actions[1].kind else {
            panic!("expected CLICK");
        };
        let mut failing = |_: &Value| Err("undefined variable".to_owned());
        assert_eq!(
            locator_wire(target, None, &mut failing),
            Err("undefined variable".to_owned())
        );
    }

    #[test]
    fn page_values_classify_per_spec_section_8() {
        assert_eq!(
            page_json("/dashboard"),
            json!({"kind": "path", "value": "/dashboard"})
        );
        assert_eq!(
            page_json("\"/search?q=widget\""),
            json!({"kind": "pathQuery", "value": "/search?q=widget"})
        );
        assert_eq!(
            page_json("https://shop.example.com/x"),
            json!({"kind": "url", "value": "https://shop.example.com/x"})
        );
        assert_eq!(
            page_json("matches /checkout\\/\\d+/"),
            json!({"kind": "regex", "source": "checkout/\\d+", "flags": ""})
        );
    }

    #[test]
    fn state_asserts_convert_to_protocol_json() {
        assert_eq!(
            assert_json("testid:user-menu visible"),
            json!({
                "subject": {"type": "locator", "locator": [{"type": "testid", "id": "user-menu"}]},
                "check": {"type": "state", "state": "visible"},
            })
        );
    }

    #[test]
    fn value_asserts_convert_to_protocol_json() {
        assert_eq!(
            assert_json("testid:a text == Alice"),
            json!({
                "subject": {"type": "locator", "locator": [{"type": "testid", "id": "a"}]},
                "check": {"type": "text", "op": {"op": "==", "value": "Alice"}},
            })
        );
        assert_eq!(
            assert_json("testid:a value != \"0\""),
            json!({
                "subject": {"type": "locator", "locator": [{"type": "testid", "id": "a"}]},
                "check": {"type": "value", "op": {"op": "!=", "value": "0"}},
            })
        );
        assert_eq!(
            assert_json("testid:a attr:aria-expanded contains tru"),
            json!({
                "subject": {"type": "locator", "locator": [{"type": "testid", "id": "a"}]},
                "check": {
                    "type": "attr",
                    "name": "aria-expanded",
                    "op": {"op": "contains", "value": "tru"},
                },
            })
        );
        assert_eq!(
            assert_json("testid:a text matches /Order #\\w+/i"),
            json!({
                "subject": {"type": "locator", "locator": [{"type": "testid", "id": "a"}]},
                "check": {
                    "type": "text",
                    "op": {"op": "matches", "source": "Order #\\w+", "flags": "i"},
                },
            })
        );
    }

    #[test]
    fn count_asserts_convert_every_operator() {
        for (source_op, wire_op) in [
            ("==", "=="),
            ("!=", "!="),
            ("<", "<"),
            ("<=", "<="),
            (">", ">"),
            (">=", ">="),
        ] {
            assert_eq!(
                assert_json(&format!("testid:row count {source_op} 3")),
                json!({
                    "subject": {"type": "locator", "locator": [{"type": "testid", "id": "row"}]},
                    "check": {"type": "count", "op": wire_op, "value": 3},
                })
            );
        }
    }

    #[test]
    fn url_and_title_asserts_convert_to_protocol_json() {
        assert_eq!(
            assert_json("url contains \"q=widget\""),
            json!({
                "subject": {"type": "url"},
                "check": {"type": "text", "op": {"op": "contains", "value": "q=widget"}},
            })
        );
        assert_eq!(
            assert_json("title matches /a.b/ism"),
            json!({
                "subject": {"type": "title"},
                "check": {"type": "text", "op": {"op": "matches", "source": "a.b", "flags": "ism"}},
            })
        );
    }

    #[test]
    fn capture_sources_convert_to_protocol_json() {
        let (source, filter) = capture_json("a: testid:x text");
        assert_eq!(
            source,
            json!({
                "type": "element",
                "locator": [{"type": "testid", "id": "x"}],
                "extract": {"type": "text"},
            })
        );
        assert_eq!(filter, json!(null));

        let (source, _) = capture_json("a: label:Amount value");
        assert_eq!(
            source,
            json!({
                "type": "element",
                "locator": [{"type": "label", "text": "Amount", "exact": true}],
                "extract": {"type": "value"},
            })
        );

        let (source, _) = capture_json("a: testid:row count");
        assert_eq!(
            source,
            json!({
                "type": "element",
                "locator": [{"type": "testid", "id": "row"}],
                "extract": {"type": "count"},
            })
        );

        let (source, _) = capture_json("a: role:link \"Docs\" attr:href");
        assert_eq!(
            source,
            json!({
                "type": "element",
                "locator": [{"type": "role", "role": "link", "name": "Docs", "exact": true}],
                "extract": {"type": "attr", "name": "href"},
            })
        );

        assert_eq!(capture_json("a: url").0, json!({"type": "url"}));
        assert_eq!(capture_json("a: title").0, json!({"type": "title"}));
        assert_eq!(
            capture_json("a: eval \"document.title.trim()\"").0,
            json!({"type": "eval", "script": "document.title.trim()"})
        );
    }

    #[test]
    fn capture_filters_convert_to_protocol_json() {
        let (_, filter) = capture_json("a: testid:x text regex /Order #(\\w+)/");
        assert_eq!(filter, json!({"source": "Order #(\\w+)", "flags": ""}));
        let (_, filter) = capture_json("a: testid:x text regex /a\\/b/im");
        assert_eq!(filter, json!({"source": "a/b", "flags": "im"}));
    }
}
