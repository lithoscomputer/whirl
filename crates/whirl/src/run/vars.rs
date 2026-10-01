//! Variables (SPEC section 11): the layered, typed variable store, value
//! interpolation, the `--variables-file` parser, and secret masking.
//!
//! Layers, later overriding earlier: `--variables-file` entries, `--var`
//! flags, then captures as the file runs. `{{env.NAME}}` reads the
//! process environment at resolve time; every env-sourced value is
//! recorded in the masking registry and replaced with `***` in all
//! textual output.

use std::collections::HashMap;
use std::env;

use whirl_lang::ast::{Span, Value, ValueSegment};
use whirl_types::{Value as TypedValue, quote as quote_json};

use crate::run::act::Variables;

/// The replacement text for a masked secret (SPEC 11).
pub(crate) const MASK: &str = "***";

/// Registry of secret values to mask in textual output. Every value
/// resolved from `{{env.NAME}}` is recorded here; [`Masker::mask`]
/// replaces each occurrence with [`MASK`], longest secret first, so a
/// secret that contains another secret masks as one unit.
#[derive(Debug, Default)]
pub(crate) struct Masker {
    /// Recorded secrets, sorted longest first.
    secrets: Vec<String>,
}

impl Masker {
    /// Records a secret value. Empty values are ignored (masking every
    /// empty string would corrupt all output), and duplicates are kept
    /// once.
    pub(crate) fn record(&mut self, secret: &str) {
        if secret.is_empty() || self.secrets.iter().any(|known| known == secret) {
            return;
        }
        let position = self
            .secrets
            .partition_point(|known| known.len() >= secret.len());
        self.secrets.insert(position, secret.to_owned());
    }

    /// Every recorded secret, longest first.
    pub(crate) fn secrets(&self) -> &[String] {
        &self.secrets
    }

    /// Replaces every occurrence of every recorded secret with [`MASK`].
    /// At each position the longest matching secret wins.
    pub(crate) fn mask(&self, text: &str) -> String {
        if self.secrets.is_empty() {
            return text.to_owned();
        }
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        'outer: while !rest.is_empty() {
            for secret in &self.secrets {
                if let Some(after) = rest.strip_prefix(secret.as_str()) {
                    out.push_str(MASK);
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
        out
    }
}

/// A failed variable resolution (SPEC 11): the step that referenced it
/// fails, and the report needs the name and the value's source span.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum VarError {
    #[error("undefined variable '{name}'")]
    Undefined { name: String, span: Span },
    #[error("environment variable '{name}' is not set")]
    UnsetEnv { name: String, span: Span },
    #[error("the setup flow captured no '{name}'")]
    UndefinedSetup { name: String, span: Span },
}

/// The layered variable store. Base layers (`--variables-file`, then
/// `--var` flags) are loaded before the run; captures overwrite as the
/// file runs. Every variable keeps its type (SPEC 11). The store owns
/// the masking registry so `{{env.NAME}}` resolution and output masking
/// stay in step.
#[derive(Debug, Default)]
pub(crate) struct VarStore {
    values: HashMap<String, TypedValue>,
    masker: Masker,
}

/// A variable's text form (SPEC 9.3). The store never holds a node set,
/// the one type without one.
fn text_of(value: &TypedValue) -> String {
    value.text_form().unwrap_or_default()
}

impl VarStore {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Sets a typed variable: a capture. Later calls overwrite earlier
    /// ones, which gives the SPEC 11 layering when layers are applied in
    /// order: variables-file entries, `--var` flags, then captures.
    pub(crate) fn set(&mut self, name: impl Into<String>, value: TypedValue) {
        self.values.insert(name.into(), value);
    }

    /// Sets a variable from `--variables-file` or `--var` text, typed the
    /// way Hurl types `--variable` (SPEC 11).
    pub(crate) fn set_input(&mut self, name: impl Into<String>, text: &str) {
        self.set(name, TypedValue::infer(text));
    }

    /// The current text form of a variable, if defined.
    pub(crate) fn get_text(&self, name: &str) -> Option<String> {
        self.values.get(name).map(text_of)
    }

    /// Defines a `{{setup.name}}` value: a capture handed over from the
    /// file's setup flow (SPEC 11). Stored under `setup.name`, which no
    /// `{{name}}` reference can spell, so the namespaces never collide.
    pub(crate) fn set_setup(&mut self, name: &str, value: TypedValue) {
        self.values.insert(format!("setup.{name}"), value);
    }

    /// Records a secret for masking without defining a variable: the
    /// setup flow's secrets, so a dependent masks the same values.
    pub(crate) fn record_secret(&mut self, secret: &str) {
        self.masker.record(secret);
    }

    /// The masking registry, for rendering console output, reports, and
    /// step titles.
    pub(crate) fn masker(&self) -> &Masker {
        &self.masker
    }

    /// Replaces every recorded secret in `text` with [`MASK`].
    pub(crate) fn mask(&self, text: &str) -> String {
        self.masker.mask(text)
    }

    /// Resolves `{{env.NAME}}` textually for step-text rendering: reads
    /// the process environment and records the value for masking.
    pub(crate) fn resolve_env(&mut self, name: &str) -> Option<String> {
        let value = env::var(name).ok()?;
        self.masker.record(&value);
        Some(value)
    }

    /// Resolves a value to its final string (SPEC 11): literal segments
    /// pass through, `{{name}}` inserts the variable's text form, and
    /// `{{env.NAME}}` reads the process environment at call time and
    /// records the value in the masking registry.
    pub(crate) fn resolve(&mut self, value: &Value) -> Result<String, VarError> {
        self.resolve_with(value, |name| env::var(name).ok())
    }

    /// The typed value of an expected value that is exactly one bare
    /// `{{name}}` reference (SPEC 11), or `None` for any other value.
    pub(crate) fn resolve_typed(&mut self, value: &Value) -> Result<Option<TypedValue>, VarError> {
        if value.quoted {
            return Ok(None);
        }
        let [segment] = value.segments.as_slice() else {
            return Ok(None);
        };
        self.lookup(segment, value.span, &|name| env::var(name).ok())
    }

    /// Interpolates a JSON template: an HTTP JSON body or a JSON literal
    /// (SPEC 11). A `{{name}}` outside a string inserts the variable as
    /// JSON; inside a string it inserts the text form with JSON escapes.
    /// `\{{` writes a literal `{{`.
    pub(crate) fn resolve_json(&mut self, template: &str, span: Span) -> Result<String, VarError> {
        let chars: Vec<char> = template.chars().collect();
        let mut out = String::with_capacity(template.len());
        let mut in_string = false;
        let mut escaped = false;
        let mut pos = 0;
        while pos < chars.len() {
            let ch = chars[pos];
            // `\\{{` inside a string is an escaped backslash before a
            // reference, not the `\{{` escape.
            if ch == '\\'
                && !escaped
                && chars.get(pos + 1) == Some(&'{')
                && chars.get(pos + 2) == Some(&'{')
            {
                out.push_str("{{");
                pos += 3;
                continue;
            }
            if ch == '{' && chars.get(pos + 1) == Some(&'{') {
                let close = chars[pos + 2..]
                    .windows(2)
                    .position(|pair| pair == ['}', '}']);
                if let Some(close) = close {
                    let name: String = chars[pos + 2..pos + 2 + close].iter().collect();
                    let segment = reference_segment(&name);
                    let typed = self
                        .lookup(&segment, span, &|name| env::var(name).ok())?
                        .expect("a reference segment always looks up");
                    if in_string {
                        let quoted = quote_json(&text_of(&typed));
                        out.push_str(&quoted[1..quoted.len() - 1]);
                    } else {
                        out.push_str(&typed.to_json().unwrap_or_else(|| "null".to_owned()));
                    }
                    pos += close + 4;
                    continue;
                }
            }
            out.push(ch);
            if in_string {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    in_string = false;
                }
            } else if ch == '"' {
                in_string = true;
            }
            pos += 1;
        }
        Ok(out)
    }

    /// [`Self::resolve`] with an injected environment lookup, so tests
    /// exercise env resolution without mutating the process environment.
    fn resolve_with(
        &mut self,
        value: &Value,
        env_lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<String, VarError> {
        let mut out = String::new();
        for segment in &value.segments {
            match self.lookup(segment, value.span, &env_lookup)? {
                Some(typed) => out.push_str(&text_of(&typed)),
                None => {
                    if let ValueSegment::Literal(text) = segment {
                        out.push_str(text);
                    }
                }
            }
        }
        Ok(out)
    }

    /// The typed value of one reference segment, or `None` for a literal.
    /// An env value is typed like `--variable` and recorded for masking.
    fn lookup(
        &mut self,
        segment: &ValueSegment,
        span: Span,
        env_lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Option<TypedValue>, VarError> {
        Ok(Some(match segment {
            ValueSegment::Literal(_) => return Ok(None),
            ValueSegment::Var(name) => {
                self.values
                    .get(name)
                    .cloned()
                    .ok_or_else(|| VarError::Undefined {
                        name: name.clone(),
                        span,
                    })?
            }
            ValueSegment::EnvVar(name) => {
                let resolved = env_lookup(name).ok_or_else(|| VarError::UnsetEnv {
                    name: name.clone(),
                    span,
                })?;
                self.masker.record(&resolved);
                TypedValue::infer(&resolved)
            }
            ValueSegment::SetupVar(name) => self
                .values
                .get(&format!("setup.{name}"))
                .cloned()
                .ok_or_else(|| VarError::UndefinedSetup {
                    name: name.clone(),
                    span,
                })?,
        }))
    }
}

/// The store as the model-facing code sees it: `Instruction` resolves an
/// `ACT`, `GOAL`, `ai:`, `EXTRACT`, or `JUDGE` text through this.
impl Variables for VarStore {
    type Error = VarError;

    fn resolve(&mut self, value: &Value) -> Result<String, VarError> {
        Self::resolve(self, value)
    }

    fn mask(&self, text: &str) -> String {
        Self::mask(self, text)
    }

    fn secrets(&self) -> &[String] {
        self.masker.secrets()
    }
}

/// The segment a `{{...}}` reference name spells.
fn reference_segment(name: &str) -> ValueSegment {
    if let Some(env_name) = name.strip_prefix("env.") {
        ValueSegment::EnvVar(env_name.to_owned())
    } else if let Some(setup_name) = name.strip_prefix("setup.") {
        ValueSegment::SetupVar(setup_name.to_owned())
    } else {
        ValueSegment::Var(name.to_owned())
    }
}

/// A malformed variables file or `--var` flag. The CLI maps this to a
/// usage error (SPEC 13, exit 4).
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum VarsFileError {
    #[error("line {line}: expected 'name=value', got '{text}'")]
    MissingEquals { line: u32, text: String },
    #[error("line {line}: invalid variable name '{name}'")]
    InvalidName { line: u32, name: String },
}

/// True when `name` matches `[A-Za-z_][A-Za-z0-9_]*` (SPEC 10).
fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// Parses a variables file (SPEC 11): one `name=value` per line, `#`
/// comment lines and blank lines ignored. The value is everything after
/// the first `=`, verbatim. Entries are returned in file order.
pub(crate) fn parse_variables_file(source: &str) -> Result<Vec<(String, String)>, VarsFileError> {
    let mut entries = Vec::new();
    for (index, raw_line) in source.lines().enumerate() {
        let line = u32::try_from(index).unwrap_or(u32::MAX).saturating_add(1);
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let (name, value) =
            trimmed
                .split_once('=')
                .ok_or_else(|| VarsFileError::MissingEquals {
                    line,
                    text: trimmed.to_owned(),
                })?;
        let name = name.trim();
        if !is_valid_name(name) {
            return Err(VarsFileError::InvalidName {
                line,
                name: name.to_owned(),
            });
        }
        entries.push((name.to_owned(), value.to_owned()));
    }
    Ok(entries)
}

/// Parses one `--var name=value` flag (SPEC 13).
pub(crate) fn parse_var_flag(flag: &str) -> Result<(String, String), VarsFileError> {
    let entries = parse_variables_file(flag)?;
    entries
        .into_iter()
        .next()
        .ok_or_else(|| VarsFileError::MissingEquals {
            line: 1,
            text: flag.trim().to_owned(),
        })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use whirl_lang::ast::{ActionKind, File};
    use whirl_lang::parse_file;

    use super::*;

    fn parse(source: &str) -> File {
        parse_file(Path::new("test.whirl"), source)
            .unwrap_or_else(|error| panic!("fixture should parse:\n{error}"))
    }

    /// The FILL value of `FILL label:Email <value>` in a one-line flow.
    fn fill_value(raw: &str) -> Value {
        let file = parse(&format!("VISIT /\nFILL label:Email {raw}\n"));
        let ActionKind::Fill { value, .. } = &file.entries[0].actions[1].kind else {
            panic!("expected FILL");
        };
        value.clone()
    }

    /// A fake environment with one variable.
    fn fake_env(name: &'static str, value: &'static str) -> impl Fn(&str) -> Option<String> {
        move |queried: &str| (queried == name).then(|| value.to_owned())
    }

    #[test]
    fn setup_captures_resolve_under_their_own_namespace() {
        let mut vars = VarStore::new();
        vars.set_input("user_id", "from-var");
        vars.set_setup("user_id", TypedValue::String("from-setup".to_owned()));
        vars.record_secret("from-setup");
        let value = Value {
            segments: vec![
                ValueSegment::SetupVar("user_id".to_owned()),
                ValueSegment::Literal("/".to_owned()),
                ValueSegment::Var("user_id".to_owned()),
            ],
            span:     Span {
                line:   0,
                column: 0,
                len:    0,
            },
            quoted:   false,
        };
        assert_eq!(
            vars.resolve(&value).expect("resolves"),
            "from-setup/from-var"
        );
        assert_eq!(vars.mask("from-setup/from-var"), "***/from-var");
        let missing = Value {
            segments: vec![ValueSegment::SetupVar("nope".to_owned())],
            span:     Span {
                line:   0,
                column: 0,
                len:    0,
            },
            quoted:   false,
        };
        assert!(matches!(
            vars.resolve(&missing),
            Err(VarError::UndefinedSetup { name, .. }) if name == "nope"
        ));
    }

    #[test]
    fn later_layers_overwrite_earlier_ones() {
        let mut store = VarStore::new();
        for (name, value) in parse_variables_file("from_file=1\nshared=file").expect("file parses")
        {
            store.set_input(name, &value);
        }
        store.set_input("shared", "flag");
        store.set_input("captured", "cap");
        store.set_input("captured", "cap2");
        assert_eq!(store.get_text("from_file").as_deref(), Some("1"));
        assert_eq!(store.get_text("shared").as_deref(), Some("flag"));
        assert_eq!(store.get_text("captured").as_deref(), Some("cap2"));
    }

    #[test]
    fn resolves_literals_variables_and_env() {
        let mut store = VarStore::new();
        store.set_input("user", "alice");
        let value = fill_value("\"{{user}}:{{env.SECRET}}!\"");
        let resolved = store
            .resolve_with(&value, fake_env("SECRET", "s3cret"))
            .expect("resolves");
        assert_eq!(resolved, "alice:s3cret!");
    }

    #[test]
    fn env_values_are_recorded_for_masking() {
        let mut store = VarStore::new();
        let value = fill_value("{{env.SECRET}}");
        store
            .resolve_with(&value, fake_env("SECRET", "hunter2"))
            .expect("resolves");
        assert_eq!(
            store.mask("say hunter2 twice: hunter2"),
            "say *** twice: ***"
        );
    }

    #[test]
    fn the_same_secret_resolved_twice_is_recorded_once() {
        let mut store = VarStore::new();
        let value = fill_value("{{env.SECRET}}");
        store
            .resolve_with(&value, fake_env("SECRET", "dup"))
            .expect("resolves");
        store
            .resolve_with(&value, fake_env("SECRET", "dup"))
            .expect("resolves");
        assert_eq!(store.mask("dup dup"), "*** ***");
    }

    #[test]
    fn resolve_reads_the_real_process_environment() {
        // PATH is set in every cargo test environment.
        let expected = env::var("PATH").expect("PATH is set for tests");
        let mut store = VarStore::new();
        let value = fill_value("\"{{env.PATH}}\"");
        assert_eq!(store.resolve(&value).expect("resolves"), expected);
    }

    #[test]
    fn an_undefined_variable_reports_name_and_span() {
        let mut store = VarStore::new();
        let value = fill_value("{{missing}}");
        let error = store.resolve(&value).expect_err("undefined");
        let VarError::Undefined { name, span } = error else {
            panic!("expected Undefined, got {error:?}");
        };
        assert_eq!(name, "missing");
        assert_eq!(span.line, 2);
    }

    #[test]
    fn an_unset_env_variable_fails_the_step() {
        let mut store = VarStore::new();
        let value = fill_value("{{env.WHIRL_VARS_TEST_NEVER_SET}}");
        let error = store.resolve(&value).expect_err("unset env");
        assert!(matches!(error, VarError::UnsetEnv { .. }), "got {error:?}");
    }

    #[test]
    fn input_variables_are_typed_like_hurl_variables() {
        let mut store = VarStore::new();
        store.set_input("count", "42");
        store.set_input("zip", "007");
        store.set_input("flag", "true");
        assert!(matches!(store.values["count"], TypedValue::Number(_)));
        assert!(matches!(store.values["zip"], TypedValue::String(_)));
        assert!(matches!(store.values["flag"], TypedValue::Bool(true)));
    }

    #[test]
    fn a_bare_whole_reference_keeps_its_type() {
        let mut store = VarStore::new();
        store.set_input("count", "42");
        let bare = fill_value("{{count}}");
        assert!(matches!(
            store.resolve_typed(&bare).expect("resolves"),
            Some(TypedValue::Number(_))
        ));
        let quoted = fill_value("\"{{count}}\"");
        assert!(store.resolve_typed(&quoted).expect("resolves").is_none());
        let joined = fill_value("{{count}}x");
        assert!(store.resolve_typed(&joined).expect("resolves").is_none());
    }

    #[test]
    fn json_templates_insert_json_outside_strings_and_text_inside() {
        let mut store = VarStore::new();
        store.set_input("count", "42");
        store.set_input("name", "Ada \"Lovelace\"");
        let span = Span {
            line:   1,
            column: 1,
            len:    1,
        };
        let json = store
            .resolve_json(
                r#"{"n": {{count}}, "who": {{name}}, "hi": "Hi {{name}}", "raw": "\{{x}}"}"#,
                span,
            )
            .expect("resolves");
        assert_eq!(
            json,
            r#"{"n": 42, "who": "Ada \"Lovelace\"", "hi": "Hi Ada \"Lovelace\"", "raw": "{{x}}"}"#
        );
    }

    #[test]
    fn json_templates_keep_an_escaped_backslash_before_a_reference() {
        let mut store = VarStore::new();
        store.set_input("dir", "tmp");
        let span = Span {
            line:   1,
            column: 1,
            len:    1,
        };
        let json = store
            .resolve_json(r#"{"path": "C:\\{{dir}}", "raw": "\\\{{dir}}"}"#, span)
            .expect("resolves");
        assert_eq!(json, r#"{"path": "C:\\tmp", "raw": "\\{{dir}}"}"#);
    }

    #[test]
    fn masking_prefers_the_longest_secret_at_overlaps() {
        let mut masker = Masker::default();
        masker.record("abc");
        masker.record("abcdef");
        masker.record("def");
        assert_eq!(masker.mask("abcdef"), "***");
        assert_eq!(masker.mask("xabcdefx abc def"), "x***x *** ***");
    }

    #[test]
    fn empty_secrets_are_ignored() {
        let mut masker = Masker::default();
        masker.record("");
        assert_eq!(masker.mask("plain"), "plain");
    }

    #[test]
    fn variables_file_ignores_comments_and_blank_lines() {
        let entries = parse_variables_file("# header\n\nname=value\n  other = spaced \n")
            .expect("file parses");
        assert_eq!(entries, vec![
            ("name".to_owned(), "value".to_owned()),
            ("other".to_owned(), " spaced".to_owned()),
        ]);
    }

    #[test]
    fn variables_file_rejects_a_line_without_equals() {
        let error = parse_variables_file("name=1\nbroken\n").expect_err("no equals");
        assert_eq!(error, VarsFileError::MissingEquals {
            line: 2,
            text: "broken".to_owned(),
        });
    }

    #[test]
    fn variables_file_rejects_an_invalid_name() {
        let error = parse_variables_file("9lives=cat\n").expect_err("bad name");
        assert_eq!(error, VarsFileError::InvalidName {
            line: 1,
            name: "9lives".to_owned(),
        });
    }

    #[test]
    fn var_flag_parses_one_assignment() {
        assert_eq!(
            parse_var_flag("k=v").expect("flag parses"),
            ("k".to_owned(), "v".to_owned())
        );
        parse_var_flag("nope").expect_err("missing equals");
        parse_var_flag("").expect_err("empty flag");
    }
}
