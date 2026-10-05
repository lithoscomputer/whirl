//! The subset of JSON Schema that an `EXTRACT` schema may use (SPEC 7.6).

use serde_json::{Value as Json, json};

/// The keywords of the subset.
const KEYWORDS: [&str; 9] = [
    "type",
    "properties",
    "required",
    "items",
    "enum",
    "const",
    "anyOf",
    "description",
    "format",
];

/// The types of the subset.
const TYPES: [&str; 7] = [
    "string", "number", "integer", "boolean", "object", "array", "null",
];

/// Why a schema is outside the subset, with the JSON Pointer of the node.
pub(crate) fn unsupported(schema: &Json) -> Option<String> {
    check_node(schema, "")
}

fn check_node(node: &Json, path: &str) -> Option<String> {
    let at = |detail: String| {
        Some(if path.is_empty() {
            detail
        } else {
            format!("{detail} at {path}")
        })
    };
    let Some(object) = node.as_object() else {
        return at("a schema must be a JSON object".to_owned());
    };
    for key in object.keys() {
        if !KEYWORDS.contains(&key.as_str()) {
            return at(format!("the keyword `{key}` is not supported"));
        }
    }
    if let Some(kind) = object.get("type") {
        let kinds: Vec<&Json> = match kind {
            Json::Array(kinds) if !kinds.is_empty() => kinds.iter().collect(),
            Json::String(_) => vec![kind],
            _ => return at("`type` must be a type name or a list of them".to_owned()),
        };
        for kind in kinds {
            if !kind.as_str().is_some_and(|name| TYPES.contains(&name)) {
                return at(format!("the type {kind} is not supported"));
            }
        }
    }
    if let Some(format) = object.get("format") {
        if format != "uri" {
            return at(format!(
                "the format {format} is not supported; only \"uri\" is"
            ));
        }
        if object.get("type") != Some(&json!("string")) {
            return at("`\"format\": \"uri\"` needs `\"type\": \"string\"`".to_owned());
        }
    }
    if let Some(properties) = object.get("properties") {
        let Some(properties) = properties.as_object() else {
            return at("`properties` must be an object".to_owned());
        };
        for (name, property) in properties {
            if let Some(problem) = check_node(property, &format!("{path}/properties/{name}")) {
                return Some(problem);
            }
        }
    }
    if let Some(required) = object.get("required")
        && !required
            .as_array()
            .is_some_and(|names| names.iter().all(Json::is_string))
    {
        return at("`required` must be a list of property names".to_owned());
    }
    if let Some(items) = object.get("items")
        && let Some(problem) = check_node(items, &format!("{path}/items"))
    {
        return Some(problem);
    }
    if let Some(values) = object.get("enum")
        && values.as_array().is_none_or(Vec::is_empty)
    {
        return at("`enum` must be a non-empty list".to_owned());
    }
    if let Some(branches) = object.get("anyOf") {
        let Some(branches) = branches.as_array().filter(|branches| !branches.is_empty()) else {
            return at("`anyOf` must be a non-empty list of schemas".to_owned());
        };
        for (index, branch) in branches.iter().enumerate() {
            if let Some(problem) = check_node(branch, &format!("{path}/anyOf/{index}")) {
                return Some(problem);
            }
        }
    }
    if let Some(description) = object.get("description")
        && !description.is_string()
    {
        return at("`description` must be a string".to_owned());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(text: &str) -> Json {
        serde_json::from_str(text).expect("JSON")
    }

    #[test]
    fn the_subset_rejects_other_keywords_and_formats() {
        assert_eq!(
            unsupported(&schema(r#"{"type": "string", "minLength": 1}"#)).as_deref(),
            Some("the keyword `minLength` is not supported")
        );
        assert_eq!(
            unsupported(&schema(
                r#"{"type": "object", "properties": {"a": {"type": "string", "format": "email"}}}"#
            ))
            .as_deref(),
            Some("the format \"email\" is not supported; only \"uri\" is at /properties/a")
        );
        assert_eq!(
            unsupported(&schema(r#"{"type": "date"}"#)).as_deref(),
            Some("the type \"date\" is not supported")
        );
        assert_eq!(
            unsupported(&schema(
                r#"{"type": ["object", "null"], "properties": {"a": {"enum": [1, "x"]}}, "required": ["a"], "anyOf": [{"type": "object"}], "description": "d"}"#
            )),
            None
        );
    }
}
