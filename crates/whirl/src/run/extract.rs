//! `EXTRACT` schemas (SPEC 7.6): the allowed subset of JSON Schema, the
//! strict schema that the model call sends, and the check of the answer.

use serde_json::{Map, Value as Json, json};
use whirl_types::Value;

use crate::check::parse_json;

/// True for a string node with `"format": "uri"`, whose answer is a ref.
fn is_link(node: &Json) -> bool {
    node.get("format") == Some(&json!("uri"))
}

/// The type names of a node, from `type` or, without it, from the values
/// of `enum` or `const`.
fn types_of(node: &Json) -> Vec<String> {
    match node.get("type") {
        Some(Json::String(kind)) => return vec![kind.clone()],
        Some(Json::Array(kinds)) => {
            return kinds
                .iter()
                .filter_map(|kind| kind.as_str().map(str::to_owned))
                .collect();
        }
        _ => {}
    }
    let values: Vec<&Json> = match (node.get("enum"), node.get("const")) {
        (Some(Json::Array(values)), _) => values.iter().collect(),
        (_, Some(value)) => vec![value],
        _ => return Vec::new(),
    };
    let mut kinds: Vec<String> = Vec::new();
    for value in values {
        let kind = match value {
            Json::Null => "null",
            Json::Bool(_) => "boolean",
            Json::Number(_) => "number",
            Json::String(_) => "string",
            Json::Array(_) => "array",
            Json::Object(_) => "object",
        };
        if !kinds.iter().any(|known| known == kind) {
            kinds.push(kind.to_owned());
        }
    }
    kinds
}

/// The schema that the model call sends (SPEC 7.6), and whether the
/// author's root is wrapped in a `value` property.
pub(crate) fn wire_schema(schema: Option<&Json>) -> (Json, bool) {
    let Some(schema) = schema else {
        let root = json!({
            "type": "object",
            "properties": {"value": {"type": ["string", "null"]}},
            "required": ["value"],
            "additionalProperties": false
        });
        return (root, true);
    };
    if types_of(schema) == ["object"] && schema.get("anyOf").is_none() {
        return (strict(schema), false);
    }
    // The value may be null, so the model can say the page does not show
    // it (SPEC 7.6).
    let root = json!({
        "type": "object",
        "properties": {"value": {"anyOf": [strict(schema), {"type": "null"}]}},
        "required": ["value"],
        "additionalProperties": false
    });
    (root, true)
}

/// A node adapted for strict structured output.
fn strict(node: &Json) -> Json {
    let mut out = Map::new();
    let kinds = types_of(node);
    if is_link(node) {
        out.insert("type".to_owned(), json!("string"));
        let description = node
            .get("description")
            .and_then(Json::as_str)
            .map_or_else(String::new, |text| format!("{text}. "));
        out.insert(
            "description".to_owned(),
            json!(format!(
                "{description}The ref of the link element, such as e12, copied from its [ref=...] mark"
            )),
        );
        return Json::Object(out);
    }
    match kinds.as_slice() {
        [] => {}
        [one] => {
            out.insert("type".to_owned(), json!(one));
        }
        many => {
            out.insert("type".to_owned(), json!(many));
        }
    }
    for key in ["description", "enum", "const"] {
        if let Some(value) = node.get(key) {
            out.insert(key.to_owned(), value.clone());
        }
    }
    if let Some(branches) = node.get("anyOf").and_then(Json::as_array) {
        out.insert(
            "anyOf".to_owned(),
            Json::Array(branches.iter().map(strict).collect()),
        );
    }
    if let Some(items) = node.get("items") {
        out.insert("items".to_owned(), strict(items));
    }
    if kinds.iter().any(|kind| kind == "object") || node.get("properties").is_some() {
        let required = required_names(node);
        let mut properties = Map::new();
        if let Some(authored) = node.get("properties").and_then(Json::as_object) {
            for (name, property) in authored {
                let adapted = strict(property);
                let adapted = if required.contains(name) {
                    adapted
                } else {
                    json!({"anyOf": [adapted, {"type": "null"}]})
                };
                properties.insert(name.clone(), adapted);
            }
        }
        let names: Vec<Json> = properties.keys().map(|name| json!(name)).collect();
        out.insert("properties".to_owned(), Json::Object(properties));
        out.insert("required".to_owned(), Json::Array(names));
        out.insert("additionalProperties".to_owned(), json!(false));
    }
    Json::Object(out)
}

fn required_names(node: &Json) -> Vec<String> {
    node.get("required")
        .and_then(Json::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(|name| name.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// The answer with each string that the schema wants as a number read as
/// that number, when the string is a plain number: an optional sign,
/// currency sign, and thousands commas, and an optional trailing `%`, as in
/// `"$1,299.00"` (SPEC 7.6). Any other string stays a string, so the schema
/// check reports it.
pub(crate) fn read_numbers(value: Value, schema: &Json) -> Value {
    match value {
        Value::String(text) if wants_number(schema) => number_text(&text)
            .and_then(|number| parse_json(&number).ok())
            .unwrap_or(Value::String(text)),
        Value::Object(members) => {
            let properties = schema.get("properties").and_then(Json::as_object);
            Value::Object(
                members
                    .into_iter()
                    .map(|(name, member)| {
                        let member = match properties.and_then(|all| all.get(&name)) {
                            Some(property) => read_numbers(member, property),
                            None => member,
                        };
                        (name, member)
                    })
                    .collect(),
            )
        }
        Value::List(items) => match schema.get("items") {
            Some(item) => Value::List(
                items
                    .into_iter()
                    .map(|member| read_numbers(member, item))
                    .collect(),
            ),
            None => Value::List(items),
        },
        other => other,
    }
}

/// Whether a node wants a number and does not allow a string.
fn wants_number(schema: &Json) -> bool {
    let mut kinds = types_of(schema);
    if let Some(branches) = schema.get("anyOf").and_then(Json::as_array) {
        kinds.extend(branches.iter().flat_map(types_of));
    }
    kinds
        .iter()
        .any(|kind| kind == "number" || kind == "integer")
        && !kinds.iter().any(|kind| kind == "string")
}

/// The JSON number that a plain number written as text stands for.
fn number_text(text: &str) -> Option<String> {
    let text = text.trim();
    let (negative, text) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let text = text.trim_start_matches(['$', '€', '£', '¥']);
    let text = text.strip_suffix('%').unwrap_or(text);
    let (whole, fraction) = match text.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (text, None),
    };
    let groups: Vec<&str> = whole.split(',').collect();
    let grouped = groups.len() > 1
        && (1..=3).contains(&groups[0].len())
        && groups[1..].iter().all(|group| group.len() == 3);
    if groups
        .iter()
        .any(|group| group.is_empty() || !group.bytes().all(|byte| byte.is_ascii_digit()))
        || (groups.len() > 1 && !grouped)
        || fraction.is_some_and(|digits| {
            digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return None;
    }
    let mut number = String::new();
    if negative {
        number.push('-');
    }
    number.push_str(&groups.concat());
    if let Some(digits) = fraction {
        number.push('.');
        number.push_str(digits);
    }
    Some(number)
}

/// The answer as the author's schema describes it: unwrapped from the
/// `value` property, with a null optional property removed.
pub(crate) fn unwrap_answer(answer: Value, schema: Option<&Json>, wrapped: bool) -> Value {
    let value = if wrapped {
        match answer {
            Value::Object(members) => members
                .into_iter()
                .find(|(name, _)| name == "value")
                .map_or(Value::Null, |(_, value)| value),
            other => other,
        }
    } else {
        answer
    };
    match schema {
        Some(schema) => drop_absent(value, schema),
        None => value,
    }
}

fn drop_absent(value: Value, schema: &Json) -> Value {
    match value {
        Value::Object(members) => {
            let required = required_names(schema);
            let properties = schema.get("properties").and_then(Json::as_object);
            Value::Object(
                members
                    .into_iter()
                    .filter(|(name, member)| {
                        required.contains(name) || !matches!(member, Value::Null)
                    })
                    .map(|(name, member)| {
                        let member = match properties.and_then(|properties| properties.get(&name)) {
                            Some(property) => drop_absent(member, property),
                            None => member,
                        };
                        (name, member)
                    })
                    .collect(),
            )
        }
        Value::List(items) => match schema.get("items") {
            Some(item) => Value::List(
                items
                    .into_iter()
                    .map(|member| drop_absent(member, item))
                    .collect(),
            ),
            None => Value::List(items),
        },
        other => other,
    }
}

/// Why a value does not match the author's schema, or `None`.
pub(crate) fn mismatch(value: &Value, schema: &Json) -> Option<String> {
    mismatch_at(value, schema, "$")
}

fn mismatch_at(value: &Value, schema: &Json, path: &str) -> Option<String> {
    if let Some(branches) = schema.get("anyOf").and_then(Json::as_array)
        && !branches
            .iter()
            .any(|branch| mismatch_at(value, branch, path).is_none())
    {
        return Some(format!("{path} matches no branch of anyOf"));
    }
    let kinds = types_of(schema);
    if !kinds.is_empty() && !kinds.iter().any(|kind| has_type(value, kind)) {
        return Some(format!(
            "{path} is a {}, not {}",
            value.value_type().name(),
            kinds.join(" or ")
        ));
    }
    if let Some(values) = schema.get("enum").and_then(Json::as_array)
        && !values.iter().any(|allowed| equals(value, allowed))
    {
        return Some(format!("{path} is not one of the enum values"));
    }
    if let Some(constant) = schema.get("const")
        && !equals(value, constant)
    {
        return Some(format!("{path} is not the const value"));
    }
    match value {
        Value::Object(members) => {
            for name in required_names(schema) {
                if !members.iter().any(|(member, _)| *member == name) {
                    return Some(format!("{path}.{name} is missing"));
                }
            }
            let properties = schema.get("properties").and_then(Json::as_object)?;
            for (name, member) in members {
                if let Some(property) = properties.get(name)
                    && let Some(problem) = mismatch_at(member, property, &format!("{path}.{name}"))
                {
                    return Some(problem);
                }
            }
            None
        }
        Value::List(items) => {
            let item = schema.get("items")?;
            items
                .iter()
                .enumerate()
                .find_map(|(index, member)| mismatch_at(member, item, &format!("{path}[{index}]")))
        }
        _ => None,
    }
}

fn has_type(value: &Value, kind: &str) -> bool {
    match (kind, value) {
        ("string", Value::String(_))
        | ("number", Value::Number(_))
        | ("boolean", Value::Bool(_))
        | ("null", Value::Null)
        | ("array", Value::List(_))
        | ("object", Value::Object(_)) => true,
        ("integer", Value::Number(number)) => !number.is_float(),
        _ => false,
    }
}

/// JSON equality between an answer value and a schema literal.
fn equals(value: &Value, literal: &Json) -> bool {
    parse_json(&literal.to_string()).is_ok_and(|literal| value.json_eq(&literal))
}

/// The refs of the link fields in a value, in order (SPEC 7.6).
pub(crate) fn link_refs(value: &Value, schema: &Json) -> Vec<String> {
    let mut refs = Vec::new();
    walk_links(value, schema, &mut |text| {
        refs.push(text.to_owned());
        None
    });
    refs
}

/// The value with each link field replaced by `resolve(ref)`.
pub(crate) fn replace_links(
    mut value: Value,
    schema: &Json,
    resolve: &mut impl FnMut(&str) -> Option<String>,
) -> Value {
    replace_in(&mut value, schema, resolve);
    value
}

fn walk_links(value: &Value, schema: &Json, visit: &mut impl FnMut(&str) -> Option<String>) {
    let mut copy = value.clone();
    replace_in(&mut copy, schema, visit);
}

fn replace_in(value: &mut Value, schema: &Json, resolve: &mut impl FnMut(&str) -> Option<String>) {
    if let Some(branches) = schema.get("anyOf").and_then(Json::as_array) {
        if let Some(branch) = branches
            .iter()
            .find(|branch| mismatch(value, branch).is_none())
        {
            replace_in(value, branch, resolve);
        }
        return;
    }
    match value {
        Value::String(text) if is_link(schema) => {
            if let Some(url) = resolve(text) {
                *text = url;
            }
        }
        Value::Object(members) => {
            let Some(properties) = schema.get("properties").and_then(Json::as_object) else {
                return;
            };
            for (name, member) in members {
                if let Some(property) = properties.get(name) {
                    replace_in(member, property, resolve);
                }
            }
        }
        Value::List(items) => {
            if let Some(item) = schema.get("items") {
                for member in items {
                    replace_in(member, item, resolve);
                }
            }
        }
        _ => {}
    }
}

/// True when an extracted value counts as missing (SPEC 7.6): null, or an
/// empty string without a schema.
pub(crate) fn is_missing(value: &Value, has_schema: bool) -> bool {
    match value {
        Value::Null => true,
        Value::String(text) => !has_schema && text.trim().is_empty(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_number_written_as_text_reads_as_a_number_where_the_schema_wants_one() {
        let read = |answer: &str, schema_text: &str| {
            let value = parse_json(answer).expect("JSON");
            read_numbers(value, &schema(schema_text)).to_json()
        };
        let number = r#"{"type": "number"}"#;
        assert_eq!(read(r#""$12.00""#, number).as_deref(), Some("12.00"));
        assert_eq!(read(r#""1,299.5""#, number).as_deref(), Some("1299.5"));
        assert_eq!(read(r#""-€3""#, number).as_deref(), Some("-3"));
        assert_eq!(read(r#""45%""#, number).as_deref(), Some("45"));
        for text in [
            r#""12 items""#,
            r#""about 12""#,
            r#""1,29""#,
            r#""12.""#,
            r#""$""#,
        ] {
            assert_eq!(
                read(text, number),
                parse_json(text).ok().and_then(|v| v.to_json()),
                "{text}"
            );
        }
        assert_eq!(
            read(r#""12""#, r#"{"type": ["string", "number"]}"#).as_deref(),
            Some(r#""12""#)
        );
        assert_eq!(
            read(
                r#"{"total": "$20.50", "items": ["3"], "note": "7"}"#,
                r#"{"type": "object", "properties": {"total": {"type": "number"}, "items": {"type": "array", "items": {"type": "integer"}}, "note": {"type": "string"}}}"#
            )
            .as_deref(),
            Some(r#"{"total":20.50,"items":[3],"note":"7"}"#)
        );
    }

    fn schema(text: &str) -> Json {
        serde_json::from_str(text).expect("JSON")
    }

    #[test]
    fn the_wire_schema_is_strict_and_wraps_a_non_object_root() {
        let authored = schema(
            r#"{"type": "object", "properties": {"total": {"type": "number"}, "note": {"type": "string"}, "kind": {"enum": ["a", "b"]}, "link": {"type": "string", "format": "uri"}}, "required": ["total"]}"#,
        );
        let (wire, wrapped) = wire_schema(Some(&authored));
        assert!(!wrapped);
        assert_eq!(wire["additionalProperties"], json!(false));
        assert_eq!(wire["required"], json!(["total", "note", "kind", "link"]));
        assert_eq!(wire["properties"]["total"], json!({"type": "number"}));
        assert_eq!(
            wire["properties"]["note"],
            json!({"anyOf": [{"type": "string"}, {"type": "null"}]})
        );
        assert_eq!(
            wire["properties"]["kind"]["anyOf"][0],
            json!({"type": "string", "enum": ["a", "b"]})
        );
        assert_eq!(
            wire["properties"]["link"]["anyOf"][0]["type"],
            json!("string")
        );
        let (list, wrapped) = wire_schema(Some(&schema(
            r#"{"type": "array", "items": {"type": "string"}}"#,
        )));
        assert!(wrapped);
        assert_eq!(
            list["properties"]["value"],
            json!({"anyOf": [{"type": "array", "items": {"type": "string"}}, {"type": "null"}]})
        );
        let (text, wrapped) = wire_schema(None);
        assert!(wrapped);
        assert_eq!(
            text["properties"]["value"]["type"],
            json!(["string", "null"])
        );
    }

    #[test]
    fn an_answer_unwraps_drops_absent_properties_and_keeps_exact_numbers() {
        let authored = schema(
            r#"{"type": "object", "properties": {"total": {"type": "number"}, "note": {"type": "string"}}, "required": ["total"]}"#,
        );
        let answer =
            parse_json(r#"{"total": 12345678901234567890.50, "note": null}"#).expect("JSON");
        let value = unwrap_answer(answer, Some(&authored), false);
        assert_eq!(
            value.to_json().as_deref(),
            Some(r#"{"total":12345678901234567890.50}"#)
        );
        assert_eq!(mismatch(&value, &authored), None);
        let list = unwrap_answer(
            parse_json(r#"{"value": ["a", "b"]}"#).expect("JSON"),
            Some(&schema(r#"{"type": "array", "items": {"type": "string"}}"#)),
            true,
        );
        assert_eq!(list.to_json().as_deref(), Some(r#"["a","b"]"#));
    }

    #[test]
    fn a_mismatch_names_the_path() {
        let authored = schema(
            r#"{"type": "object", "properties": {"count": {"type": "integer"}, "tags": {"type": "array", "items": {"enum": ["a", "b"]}}}, "required": ["count"]}"#,
        );
        let check = |text: &str| mismatch(&parse_json(text).expect("JSON"), &authored);
        assert_eq!(check(r#"{"count": 2, "tags": ["a"]}"#), None);
        assert_eq!(
            check(r#"{"count": 2.5}"#).as_deref(),
            Some("$.count is a number, not integer")
        );
        assert_eq!(check("{}").as_deref(), Some("$.count is missing"));
        assert_eq!(
            check(r#"{"count": 1, "tags": ["c"]}"#).as_deref(),
            Some("$.tags[0] is not one of the enum values")
        );
    }

    #[test]
    fn link_fields_give_their_refs_and_take_urls() {
        let authored = schema(
            r#"{"type": "array", "items": {"type": "object", "properties": {"name": {"type": "string"}, "url": {"type": "string", "format": "uri"}}}}"#,
        );
        let value = parse_json(r#"[{"name": "Docs", "url": "e4"}, {"name": "Blog", "url": "e7"}]"#)
            .expect("JSON");
        assert_eq!(link_refs(&value, &authored), ["e4", "e7"]);
        let replaced = replace_links(value, &authored, &mut |element| {
            Some(format!("https://x.test/{element}"))
        });
        assert_eq!(
            replaced.to_json().as_deref(),
            Some(
                r#"[{"name":"Docs","url":"https://x.test/e4"},{"name":"Blog","url":"https://x.test/e7"}]"#
            )
        );
    }

    #[test]
    fn null_and_empty_text_are_missing() {
        assert!(is_missing(&Value::Null, true));
        assert!(is_missing(&Value::String(" ".to_owned()), false));
        assert!(!is_missing(&Value::String(String::new()), true));
    }
}
