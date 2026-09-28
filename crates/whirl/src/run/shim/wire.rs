//! Conversion of AST nodes to the shim wire JSON of
//! `docs/engineering/shim-protocol.md` sections 4.1-4.4.
//!
//! Checks with a subject never reach the shim as checks: Rust evaluates
//! them (ADR `evaluate-checks-in-rust`) and sends only reads.
//!
//! The wire carries resolved strings, while the AST holds interpolation
//! segments. Every conversion therefore takes a resolver closure that
//! turns a [`Value`] into its resolved string; the runner supplies its
//! variable table (and masking registry) through it, and a resolver
//! error aborts the conversion unchanged.

use serde_json::{Value as Json, json};

use crate::lang::ast::{
    DefaultEngine, Extractor, Ident, Locator, PageCheck, Regex, ScrollMotion, SegmentKind,
    StateCheck, Subject, TextPrefix, Value,
};

/// Shim-only response key for an independent HTTP entry. `$` and `:` cannot
/// occur in a public `RESPONSE` name.
pub(crate) fn independent_http_response(line: u32) -> String {
    format!("$whirl:http:{line}")
}

/// A `scroll` command's motion (protocol section 4): into view without a
/// motion, else one chunk or a vertical position.
pub(crate) fn scroll_motion_wire(motion: Option<&ScrollMotion>) -> Json {
    match motion {
        None => json!({"type": "intoView"}),
        Some(ScrollMotion::Chunk(direction)) => {
            json!({"type": "chunk", "direction": direction.keyword()})
        }
        Some(ScrollMotion::To(percent)) => json!({"type": "position", "percent": percent.value()}),
    }
}

/// Resolves a value to its final string; `E` is the runner's error.
pub(crate) type Resolve<'a, E> = dyn FnMut(&Value) -> Result<String, E> + 'a;

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

/// Converts a locator to the wire array of protocol section 4.1.
/// `default_engine` names the engine an unprefixed segment selects; it
/// must be present when the locator can hold one (actions, SPEC 6.1).
pub(crate) fn locator_wire<E>(
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
pub(crate) fn page_wire<E>(check: &PageCheck, resolve: &mut Resolve<'_, E>) -> Result<Json, E> {
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

fn locator_subject<E>(locator: &Locator, resolve: &mut Resolve<'_, E>) -> Result<Json, E> {
    // Asserts and captures never hold default-engine segments (SPEC 6.1).
    let locator = locator_wire(locator, None, resolve)?;
    Ok(json!({"type": "locator", "locator": locator}))
}

/// The wire spec of a state check (protocol section 4.3).
pub(crate) fn state_assert_wire<E>(
    locator: &Locator,
    state: StateCheck,
    resolve: &mut Resolve<'_, E>,
) -> Result<Json, E> {
    Ok(json!({
        "subject": locator_subject(locator, resolve)?,
        "check": {"type": "state", "state": state_text(state)},
    }))
}

/// The wire spec of a `tab:NAME closed` check (protocol section 4.3).
pub(crate) fn tab_closed_wire(name: &Ident) -> Json {
    json!({"subject": {"type": "tab", "name": name.text}, "check": {"type": "closed"}})
}

/// The wire read subject of a page subject (protocol section 4.4), or
/// `None` for a response subject, which `readResponse` reads.
pub(crate) fn read_subject_wire<E>(
    subject: &Subject,
    resolve: &mut Resolve<'_, E>,
) -> Result<Option<Json>, E> {
    let json = match subject {
        Subject::Element { locator, extractor } => {
            let locator = locator_wire(locator, None, resolve)?;
            let extract = match extractor {
                Extractor::Count => return Ok(Some(json!({"type": "count", "locator": locator}))),
                Extractor::Text => json!({"type": "text"}),
                Extractor::Value => json!({"type": "value"}),
                Extractor::Attr(name) => json!({"type": "attr", "name": name}),
            };
            json!({"type": "element", "locator": locator, "extract": extract})
        }
        Subject::Url => json!({"type": "url"}),
        Subject::Title => json!({"type": "title"}),
        Subject::Eval(script) => json!({"type": "eval", "script": resolve(script)?}),
        Subject::Response { .. } => return Ok(None),
    };
    Ok(Some(json))
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::path::Path;

    use serde_json::json;

    use super::*;
    use crate::lang::ast::{ActionKind, AssertBody, File, ValueSegment};
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
        let ActionKind::Click { target, .. } = &action.kind else {
            panic!("expected CLICK");
        };
        locator_wire(target, action.kind.default_engine(), &mut resolve).unwrap()
    }

    /// The wire JSON of a state or tab assert.
    fn assert_json(line: &str) -> Json {
        let file = parse(&format!("VISIT /\nASSERT {line}\n"));
        match &file.entries[0].asserts().collect::<Vec<_>>()[0].body {
            AssertBody::ElementState { locator, state } => {
                state_assert_wire(locator, *state, &mut resolve).unwrap()
            }
            AssertBody::TabClosed { name } => tab_closed_wire(name),
            AssertBody::Check(_) => panic!("checks with a subject never reach the shim"),
        }
    }

    /// The read subject of the first capture.
    fn read_json(line: &str) -> Option<Json> {
        let file = parse(&format!("VISIT /\nCAPTURE {line}\n"));
        let capture = &file.entries[0].captures().collect::<Vec<_>>()[0];
        read_subject_wire(&capture.subject, &mut resolve).unwrap()
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
        let ActionKind::Click { target, .. } = &file.entries[0].actions[1].kind else {
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
    fn tab_closed_asserts_convert_to_protocol_json() {
        assert_eq!(
            assert_json("tab:payment closed"),
            json!({"subject": {"type": "tab", "name": "payment"}, "check": {"type": "closed"}})
        );
    }

    #[test]
    fn page_subjects_convert_to_read_subjects() {
        assert_eq!(
            read_json("a: testid:x text"),
            Some(json!({
                "type": "element",
                "locator": [{"type": "testid", "id": "x"}],
                "extract": {"type": "text"},
            }))
        );
        assert_eq!(
            read_json("a: label:Amount value"),
            Some(json!({
                "type": "element",
                "locator": [{"type": "label", "text": "Amount", "exact": true}],
                "extract": {"type": "value"},
            }))
        );
        assert_eq!(
            read_json("a: testid:row >> nth:-1 count"),
            Some(json!({
                "type": "count",
                "locator": [{"type": "testid", "id": "row"}, {"type": "nth", "index": -1}],
            }))
        );
        assert_eq!(
            read_json("a: role:link \"Docs\" attr:href"),
            Some(json!({
                "type": "element",
                "locator": [{"type": "role", "role": "link", "name": "Docs", "exact": true}],
                "extract": {"type": "attr", "name": "href"},
            }))
        );
        assert_eq!(read_json("a: url"), Some(json!({"type": "url"})));
        assert_eq!(read_json("a: title"), Some(json!({"type": "title"})));
        assert_eq!(
            read_json("a: eval \"document.title.trim()\""),
            Some(json!({"type": "eval", "script": "document.title.trim()"}))
        );
    }

    #[test]
    fn response_subjects_have_no_read_subject() {
        let file = parse("VISIT /\nRESPONSE order GET /x\nCAPTURE id: response:order json:$.id\n");
        let capture = &file.entries[0].captures().collect::<Vec<_>>()[0];
        assert_eq!(
            read_subject_wire(&capture.subject, &mut resolve).unwrap(),
            None
        );
    }
}
