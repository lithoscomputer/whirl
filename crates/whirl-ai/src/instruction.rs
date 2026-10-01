//! The instruction an `ACT` line sends to the model (SPEC 7.4, 11).
//!
//! Masked values never leave Whirl. Each `{{env.NAME}}` segment becomes a
//! `%env.NAME%` placeholder, and a recorded secret inside any other
//! variable becomes `%secretN%`. The model writes placeholders into its
//! arguments, and Whirl puts the values back only when it builds the shim
//! command.

use whirl_lang::ast::{Value, ValueSegment};

/// The variable store an instruction resolves through (SPEC 11): what the
/// runner's store does for every other value, seen from the model side.
///
/// Implementors resolve `{{name}}`, `{{setup.name}}`, and `{{env.NAME}}`
/// references to their text form. Resolving an env reference records its
/// value as a secret, so [`Variables::secrets`] grows as resolution goes
/// on. [`Variables::mask`] replaces every recorded secret in a text, so a
/// text that masks to itself holds no secret.
pub trait Variables {
    /// A failed resolution: an undefined variable or an unset environment
    /// variable. The step that referenced it fails with this error.
    type Error;

    /// Resolves a value to its final string (SPEC 11) and records every
    /// `{{env.NAME}}` value it reads as a secret.
    fn resolve(&mut self, value: &Value) -> Result<String, Self::Error>;

    /// Replaces every recorded secret in `text` with its mask.
    fn mask(&self, text: &str) -> String;

    /// Every recorded secret, longest first.
    fn secrets(&self) -> &[String];
}

/// Placeholder names and the secret values they stand for.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SecretBindings {
    /// `(name, value)` pairs; the name has no `%` delimiters.
    entries: Vec<(String, String)>,
}

impl SecretBindings {
    /// Binds a value under a name and returns the `%name%` placeholder.
    fn bind(&mut self, name: &str, value: &str) -> String {
        if !self.entries.iter().any(|(known, _)| known == name) {
            self.entries.push((name.to_owned(), value.to_owned()));
        }
        format!("%{name}%")
    }

    /// Binds a secret found inside another variable's value, reusing the
    /// placeholder of an equal secret.
    fn bind_secret(&mut self, value: &str) -> String {
        if let Some((name, _)) = self.entries.iter().find(|(_, known)| known == value) {
            return format!("%{name}%");
        }
        let name = format!("secret{}", self.entries.len() + 1);
        self.bind(&name, value)
    }

    /// The placeholders the prompt lists, such as `%env.PASSWORD%`.
    pub fn placeholders(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|(name, _)| format!("%{name}%"))
            .collect()
    }

    /// Replaces every bound placeholder in `text` with its value.
    pub(crate) fn fill(&self, text: &str) -> Result<String, UnboundPlaceholder> {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find('%') {
            out.push_str(&rest[..start]);
            let after = &rest[start + 1..];
            let Some(end) = after.find('%') else {
                out.push_str(&rest[start..]);
                return Ok(out);
            };
            let name = &after[..end];
            if !is_placeholder_name(name) {
                // Ordinary text such as "50%"; keep the `%` and go on.
                out.push('%');
                rest = after;
                continue;
            }
            let value = self
                .entries
                .iter()
                .find(|(known, _)| known == name)
                .map(|(_, value)| value)
                .ok_or_else(|| UnboundPlaceholder {
                    name: name.to_owned(),
                })?;
            out.push_str(value);
            rest = &after[end + 1..];
        }
        out.push_str(rest);
        Ok(out)
    }
}

/// True for the names Whirl writes: `env.NAME` and `secretN`.
fn is_placeholder_name(name: &str) -> bool {
    if let Some(env_name) = name.strip_prefix("env.") {
        let mut chars = env_name.chars();
        return chars
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
            && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_');
    }
    name.strip_prefix("secret")
        .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
}

/// A model argument named a placeholder the instruction never had.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("the answer uses the unknown placeholder %{name}%")]
pub struct UnboundPlaceholder {
    pub(crate) name: String,
}

/// The instruction text the model sees, and the secrets its placeholders
/// stand for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Instruction {
    prompt:     String,
    bindings:   SecretBindings,
    /// Each `{{name}}` and `{{setup.name}}` reference the instruction used,
    /// with its value, when the value holds no secret (SPEC 12.1).
    references: Vec<(String, String)>,
}

impl Instruction {
    /// Resolves an `ACT` value (SPEC 11) with masked values replaced by
    /// placeholders. Resolution records env values for masking as usual.
    pub fn try_new<V: Variables>(value: &Value, vars: &mut V) -> Result<Self, V::Error> {
        let mut prompt = String::new();
        let mut bindings = SecretBindings::default();
        let mut references = Vec::new();
        for segment in &value.segments {
            let resolved = vars.resolve(&Value {
                segments: vec![segment.clone()],
                span:     value.span,
                quoted:   value.quoted,
            })?;
            match segment {
                ValueSegment::Literal(text) => prompt.push_str(text),
                ValueSegment::EnvVar(name) => {
                    prompt.push_str(&bindings.bind(&format!("env.{name}"), &resolved));
                }
                ValueSegment::Var(name) | ValueSegment::SetupVar(name) => {
                    if vars.mask(&resolved) == resolved {
                        let reference = match segment {
                            ValueSegment::SetupVar(_) => format!("{{{{setup.{name}}}}}"),
                            _ => format!("{{{{{name}}}}}"),
                        };
                        references.push((reference, resolved.clone()));
                    }
                    redact_into(&mut prompt, &resolved, vars.secrets(), &mut bindings);
                }
            }
        }
        Ok(Self {
            prompt,
            bindings,
            references,
        })
    }

    /// An argument as the AI cache writes it (SPEC 12.1): a quoted Whirl
    /// value in which each `%env.NAME%` placeholder is the reference
    /// `{{env.NAME}}`. Text equal to a variable the instruction used is that
    /// variable's reference. `None` when the text holds a masked value that
    /// no reference names, a `%secretN%` placeholder.
    pub fn cache_value(&self, text: &str) -> Option<String> {
        if let Some((reference, _)) = self.references.iter().find(|(_, value)| value == text) {
            return Some(reference.clone());
        }
        let mut out = String::from("\"");
        let mut rest = text;
        while let Some(start) = rest.find('%') {
            push_literal(&mut out, &rest[..start]);
            let after = &rest[start + 1..];
            match after.find('%') {
                Some(end) if is_placeholder_name(&after[..end]) => {
                    let name = &after[..end];
                    if !name.starts_with("env.") {
                        return None;
                    }
                    out.push_str("{{");
                    out.push_str(name);
                    out.push_str("}}");
                    rest = &after[end + 1..];
                }
                _ => {
                    out.push('%');
                    rest = after;
                }
            }
        }
        push_literal(&mut out, rest);
        out.push('"');
        Some(out)
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    pub fn bindings(&self) -> &SecretBindings {
        &self.bindings
    }

    /// The instruction's own characters for text the model copied from one
    /// of its quoted strings, or `text` itself when it matches none. A model
    /// can change the case or spacing of what it copies; what Whirl types
    /// must be what the author wrote. Only a match counts: a quoted string
    /// can also name the field, not the text to type.
    /// The instruction's own characters for `text`, which a model copied
    /// from it, or `None` when the instruction does not contain it. Case
    /// and runs of spaces may differ; words may not. A placeholder the
    /// instruction binds is its own span.
    pub(crate) fn span(&self, text: &str) -> Option<String> {
        let text = text.trim();
        if self
            .bindings
            .placeholders()
            .iter()
            .any(|placeholder| placeholder == text)
        {
            return Some(text.to_owned());
        }
        find_loosely(&self.prompt, text).map(str::to_owned)
    }

    pub(crate) fn ground<'a>(&'a self, text: &'a str) -> &'a str {
        quoted_strings(&self.prompt)
            .into_iter()
            .find(|quoted| same_text(quoted, text))
            .unwrap_or(text)
    }
}

/// Appends text to a quoted Whirl value with its escapes (SPEC 3.1), so a
/// literal `{{` stays text.
fn push_literal(out: &mut String, text: &str) {
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '{' if chars.peek() == Some(&'{') => out.push_str("\\{"),
            _ => out.push(ch),
        }
    }
}

/// The first part of `haystack` that reads as `needle`, ignoring case and
/// how much whitespace separates the words.
fn find_loosely<'a>(haystack: &'a str, needle: &str) -> Option<&'a str> {
    let words: Vec<&str> = needle.split_whitespace().collect();
    if words.is_empty() {
        return None;
    }
    let same = |left: char, right: char| left.to_lowercase().eq(right.to_lowercase());
    'start: for (start, _) in haystack.char_indices() {
        let mut position = start;
        for (index, word) in words.iter().enumerate() {
            if index > 0 {
                let rest = &haystack[position..];
                let spaces = rest.len() - rest.trim_start().len();
                if spaces == 0 {
                    continue 'start;
                }
                position += spaces;
            }
            let mut rest = haystack[position..].chars();
            for expected in word.chars() {
                match rest.next() {
                    Some(actual) if same(actual, expected) => position += actual.len_utf8(),
                    _ => continue 'start,
                }
            }
        }
        return Some(&haystack[start..position]);
    }
    None
}

/// Whether two texts have the same letters and digits, ignoring case,
/// spaces, and punctuation. Text with no letters or digits matches only
/// itself.
pub(crate) fn same_text(left: &str, right: &str) -> bool {
    fn significant(text: &str) -> impl Iterator<Item = char> + '_ {
        text.chars()
            .filter(|ch| ch.is_alphanumeric())
            .flat_map(char::to_lowercase)
    }
    if significant(left).next().is_none() || significant(right).next().is_none() {
        return left == right;
    }
    significant(left).eq(significant(right))
}

/// The strings quoted in `text`: in straight or curly double quotes, in
/// curly single quotes, or in straight single quotes that stand at word
/// boundaries, so the apostrophe in "user's" opens no quote. A quote does
/// not cross a line.
pub(crate) fn quoted_strings(text: &str) -> Vec<&str> {
    let is_word = |ch: Option<char>| ch.is_some_and(|ch| ch.is_alphanumeric() || ch == '_');
    let mut found = Vec::new();
    let mut start = 0;
    while let Some(offset) = text[start..].find(['"', '\'', '\u{201C}', '\u{2018}']) {
        let open = start + offset;
        let quote = text[open..]
            .chars()
            .next()
            .expect("find returns a char boundary");
        let close = match quote {
            '\u{201C}' => '\u{201D}',
            '\u{2018}' => '\u{2019}',
            straight => straight,
        };
        let body_start = open + quote.len_utf8();
        // Without a match here, the search goes on after the opening quote.
        start = body_start;
        if quote == '\'' && is_word(text[..open].chars().next_back()) {
            continue;
        }
        let Some(length) = text[body_start..].find([close, '\n']) else {
            continue;
        };
        let body_end = body_start + length;
        if length == 0 || text[body_end..].starts_with('\n') {
            continue;
        }
        let rest = body_end + close.len_utf8();
        if quote == '\'' && is_word(text[rest..].chars().next()) {
            continue;
        }
        found.push(&text[body_start..body_end]);
        start = rest;
    }
    found
}

/// Appends `text` with every recorded secret replaced by a placeholder,
/// longest secret first, as masking does (SPEC 11).
fn redact_into(out: &mut String, text: &str, secrets: &[String], bindings: &mut SecretBindings) {
    let mut rest = text;
    'outer: while !rest.is_empty() {
        for secret in secrets {
            if let Some(after) = rest.strip_prefix(secret.as_str()) {
                out.push_str(&bindings.bind_secret(secret));
                rest = after;
                continue 'outer;
            }
        }
        let ch = rest
            .chars()
            .next()
            .expect("a non-empty string has a first char");
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
}

/// A [`Variables`] store for unit tests: string inputs by name and the
/// secrets of a runner's store, masked with `***`.
#[cfg(test)]
pub(crate) mod testing {
    use std::cmp::Reverse;
    use std::collections::HashMap;

    use whirl_lang::ast::{Value, ValueSegment};

    use super::Variables;

    /// An undefined variable; the tests never resolve env or setup references.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub(crate) struct Undefined(pub(crate) String);

    #[derive(Debug, Default)]
    pub(crate) struct TestVars {
        values:  HashMap<String, String>,
        /// Recorded secrets, longest first.
        secrets: Vec<String>,
    }

    impl TestVars {
        pub(crate) fn new() -> Self {
            Self::default()
        }

        pub(crate) fn set_input(&mut self, name: &str, text: &str) {
            self.values.insert(name.to_owned(), text.to_owned());
        }

        pub(crate) fn record_secret(&mut self, secret: &str) {
            self.secrets.push(secret.to_owned());
            self.secrets.sort_by_key(|secret| Reverse(secret.len()));
        }
    }

    impl Variables for TestVars {
        type Error = Undefined;

        fn resolve(&mut self, value: &Value) -> Result<String, Undefined> {
            let mut out = String::new();
            for segment in &value.segments {
                match segment {
                    ValueSegment::Literal(text) => out.push_str(text),
                    ValueSegment::Var(name) => out.push_str(
                        self.values
                            .get(name)
                            .ok_or_else(|| Undefined(name.clone()))?,
                    ),
                    ValueSegment::EnvVar(name) | ValueSegment::SetupVar(name) => {
                        return Err(Undefined(name.clone()));
                    }
                }
            }
            Ok(out)
        }

        fn mask(&self, text: &str) -> String {
            self.secrets
                .iter()
                .fold(text.to_owned(), |text, secret| text.replace(secret, "***"))
        }

        fn secrets(&self) -> &[String] {
            &self.secrets
        }
    }
}

#[cfg(test)]
mod tests {
    use whirl_lang::ast::Span;

    use super::testing::TestVars;
    use super::*;

    fn value(segments: Vec<ValueSegment>) -> Value {
        Value {
            segments,
            span: Span {
                line:   1,
                column: 1,
                len:    1,
            },
            quoted: true,
        }
    }

    #[test]
    fn the_cache_writes_placeholders_and_variables_as_references() {
        let mut vars = TestVars::new();
        vars.set_input("user", "ada");
        let instruction = Instruction::try_new(
            &value(vec![
                ValueSegment::Literal("sign in as ".to_owned()),
                ValueSegment::Var("user".to_owned()),
            ]),
            &mut vars,
        )
        .expect("resolves");
        assert_eq!(instruction.cache_value("ada").as_deref(), Some("{{user}}"));
        assert_eq!(
            instruction.cache_value("%env.PASSWORD%!").as_deref(),
            Some(r#""{{env.PASSWORD}}!""#)
        );
        assert_eq!(
            instruction.cache_value(r#"50% off {{x}} "now""#).as_deref(),
            Some(r#""50% off \{{x}} \"now\"""#)
        );
        assert_eq!(instruction.cache_value("%secret1%"), None);
    }

    #[test]
    fn plain_variables_reach_the_model_as_values() {
        let mut vars = TestVars::new();
        vars.set_input("user", "ada");
        let instruction = Instruction::try_new(
            &value(vec![
                ValueSegment::Literal("sign in as ".to_owned()),
                ValueSegment::Var("user".to_owned()),
            ]),
            &mut vars,
        )
        .expect("resolves");
        assert_eq!(instruction.prompt(), "sign in as ada");
        assert!(instruction.bindings().placeholders().is_empty());
    }

    #[test]
    fn a_recorded_secret_inside_a_variable_becomes_a_placeholder() {
        let mut vars = TestVars::new();
        vars.record_secret("hunter2");
        vars.set_input("login", "ada:hunter2");
        let instruction = Instruction::try_new(
            &value(vec![
                ValueSegment::Literal("log in with ".to_owned()),
                ValueSegment::Var("login".to_owned()),
            ]),
            &mut vars,
        )
        .expect("resolves");
        assert_eq!(instruction.prompt(), "log in with ada:%secret1%");
        assert_eq!(
            instruction.bindings().fill("%secret1%"),
            Ok("hunter2".to_owned())
        );
    }

    #[test]
    fn fill_replaces_bound_placeholders_and_keeps_other_percent_signs() {
        let mut bindings = SecretBindings::default();
        bindings.bind("env.PASSWORD", "s3cret");
        assert_eq!(
            bindings.fill("pw=%env.PASSWORD% at 50% off"),
            Ok("pw=s3cret at 50% off".to_owned())
        );
        assert_eq!(bindings.fill("100%"), Ok("100%".to_owned()));
        assert_eq!(bindings.fill("%not a name%"), Ok("%not a name%".to_owned()));
        assert_eq!(
            bindings.fill("%env.OTHER%"),
            Err(UnboundPlaceholder {
                name: "env.OTHER".to_owned(),
            })
        );
    }

    fn literal(text: &str) -> Instruction {
        Instruction::try_new(
            &value(vec![ValueSegment::Literal(text.to_owned())]),
            &mut TestVars::new(),
        )
        .expect("resolves")
    }

    #[test]
    fn quoted_strings_find_every_quote_style_but_not_apostrophes() {
        assert_eq!(
            quoted_strings(
                "type \"Ada\" into 'Name', then \u{201C}x\u{201D} and \u{2018}y\u{2019}"
            ),
            vec!["Ada", "Name", "x", "y"]
        );
        assert_eq!(
            quoted_strings("fill in the user's name with 'Grace'"),
            vec!["Grace"]
        );
        assert_eq!(quoted_strings("an \"\" empty and a \"real\" one"), vec![
            " empty and a "
        ]);
        assert!(quoted_strings("\"no\nclose\"").is_empty());
    }

    #[test]
    fn grounding_restores_the_instructions_own_characters() {
        let instruction = literal("type \"AbC 123.\" into the \"Search\" box");
        assert_eq!(instruction.ground("abc  123"), "AbC 123.");
        assert_eq!(instruction.ground("search"), "Search");
        assert_eq!(instruction.ground("something else"), "something else");
        assert_eq!(literal("type hello").ground("Hello"), "Hello");
    }

    #[test]
    fn a_span_is_the_instructions_own_characters() {
        let instruction = literal("type  AbC   123 into the Search box");
        assert_eq!(instruction.span("abc 123"), Some("AbC   123".to_owned()));
        assert_eq!(
            instruction.span("  search box "),
            Some("Search box".to_owned())
        );
        assert_eq!(instruction.span("abc 124"), None);
        assert_eq!(instruction.span(""), None);
        let mut vars = TestVars::new();
        vars.record_secret("hunter2");
        vars.set_input("pw", "hunter2");
        let secret = Instruction::try_new(
            &value(vec![
                ValueSegment::Literal("type ".to_owned()),
                ValueSegment::Var("pw".to_owned()),
            ]),
            &mut vars,
        )
        .expect("resolves");
        assert_eq!(secret.span("%secret1%"), Some("%secret1%".to_owned()));
    }

    #[test]
    fn text_without_letters_or_digits_matches_only_itself() {
        assert!(same_text("(555) 123-4567", "5551234567"));
        assert!(same_text("---", "---"));
        assert!(!same_text("---", "..."));
        assert!(!same_text("", "abc"));
    }

    #[test]
    fn equal_secrets_share_one_placeholder() {
        let mut bindings = SecretBindings::default();
        assert_eq!(bindings.bind_secret("a"), "%secret1%");
        assert_eq!(bindings.bind_secret("b"), "%secret2%");
        assert_eq!(bindings.bind_secret("a"), "%secret1%");
        assert_eq!(bindings.placeholders(), vec!["%secret1%", "%secret2%"]);
    }
}
