//! Variables (SPEC section 11): the layered variable store, value
//! interpolation, the `--variables-file` parser, and secret masking.
//!
//! Layers, later overriding earlier: `--variables-file` entries, `--var`
//! flags, then captures as the file runs. `{{env.NAME}}` reads the
//! process environment at resolve time; every env-sourced value is
//! recorded in the masking registry and replaced with `***` in all
//! textual output.

use std::collections::HashMap;
use std::env;

use crate::lang::ast::{Span, Value, ValueSegment};

/// The replacement text for a masked secret (SPEC 11).
pub const MASK: &str = "***";

/// Registry of secret values to mask in textual output. Every value
/// resolved from `{{env.NAME}}` is recorded here; [`Masker::mask`]
/// replaces each occurrence with [`MASK`], longest secret first, so a
/// secret that contains another secret masks as one unit.
#[derive(Debug, Default)]
pub struct Masker {
    /// Recorded secrets, sorted longest first.
    secrets: Vec<String>,
}

impl Masker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a secret value. Empty values are ignored (masking every
    /// empty string would corrupt all output), and duplicates are kept
    /// once.
    pub fn record(&mut self, secret: &str) {
        if secret.is_empty() || self.secrets.iter().any(|known| known == secret) {
            return;
        }
        let position = self
            .secrets
            .partition_point(|known| known.len() >= secret.len());
        self.secrets.insert(position, secret.to_owned());
    }

    /// Every recorded secret, longest first.
    pub fn secrets(&self) -> &[String] {
        &self.secrets
    }

    /// Replaces every occurrence of every recorded secret with [`MASK`].
    /// At each position the longest matching secret wins.
    pub fn mask(&self, text: &str) -> String {
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
pub enum VarError {
    #[error("undefined variable '{name}'")]
    Undefined { name: String, span: Span },
    #[error("environment variable '{name}' is not set")]
    UnsetEnv { name: String, span: Span },
    #[error("the setup flow captured no '{name}'")]
    UndefinedSetup { name: String, span: Span },
}

impl VarError {
    /// The source span of the value that referenced the variable.
    pub fn span(&self) -> Span {
        match self {
            Self::Undefined { span, .. }
            | Self::UnsetEnv { span, .. }
            | Self::UndefinedSetup { span, .. } => *span,
        }
    }
}

/// The layered variable store. Base layers (`--variables-file`, then
/// `--var` flags) are loaded before the run; captures overwrite as the
/// file runs. The store owns the masking registry so `{{env.NAME}}`
/// resolution and output masking stay in step.
#[derive(Debug, Default)]
pub struct VarStore {
    values: HashMap<String, String>,
    masker: Masker,
}

impl VarStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets a variable. Later calls overwrite earlier ones, which gives
    /// the SPEC 11 layering when layers are applied in order:
    /// variables-file entries, `--var` flags, then captures.
    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.values.insert(name.into(), value.into());
    }

    /// The current value of a variable, if defined.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    /// Defines a `{{setup.name}}` value: a capture handed over from the
    /// file's setup flow (SPEC 11). Stored under `setup.name`, which no
    /// `{{name}}` reference can spell, so the namespaces never collide.
    pub fn set_setup(&mut self, name: &str, value: impl Into<String>) {
        self.values.insert(format!("setup.{name}"), value.into());
    }

    /// Records a secret for masking without defining a variable: the
    /// setup flow's secrets, so a dependent masks the same values.
    pub fn record_secret(&mut self, secret: &str) {
        self.masker.record(secret);
    }

    /// The masking registry, for rendering console output, reports, and
    /// step titles.
    pub fn masker(&self) -> &Masker {
        &self.masker
    }

    /// Replaces every recorded secret in `text` with [`MASK`].
    pub fn mask(&self, text: &str) -> String {
        self.masker.mask(text)
    }

    /// Resolves `{{env.NAME}}` textually for step-text rendering: reads
    /// the process environment and records the value for masking.
    pub fn resolve_env(&mut self, name: &str) -> Option<String> {
        let value = env::var(name).ok()?;
        self.masker.record(&value);
        Some(value)
    }

    /// Resolves a value to its final string (SPEC 11): literal segments
    /// pass through, `{{name}}` reads the store, and `{{env.NAME}}`
    /// reads the process environment at call time and records the value
    /// in the masking registry.
    pub fn resolve(&mut self, value: &Value) -> Result<String, VarError> {
        self.resolve_with(value, |name| env::var(name).ok())
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
            match segment {
                ValueSegment::Literal(text) => out.push_str(text),
                ValueSegment::Var(name) => {
                    let resolved = self.values.get(name).ok_or_else(|| VarError::Undefined {
                        name: name.clone(),
                        span: value.span,
                    })?;
                    out.push_str(resolved);
                }
                ValueSegment::EnvVar(name) => {
                    let resolved = env_lookup(name).ok_or_else(|| VarError::UnsetEnv {
                        name: name.clone(),
                        span: value.span,
                    })?;
                    self.masker.record(&resolved);
                    out.push_str(&resolved);
                }
                ValueSegment::SetupVar(name) => {
                    let resolved = self.values.get(&format!("setup.{name}")).ok_or_else(|| {
                        VarError::UndefinedSetup {
                            name: name.clone(),
                            span: value.span,
                        }
                    })?;
                    out.push_str(resolved);
                }
            }
        }
        Ok(out)
    }
}

/// A malformed variables file or `--var` flag. The CLI maps this to a
/// usage error (SPEC 13, exit 4).
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum VarsFileError {
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
pub fn parse_variables_file(source: &str) -> Result<Vec<(String, String)>, VarsFileError> {
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
pub fn parse_var_flag(flag: &str) -> Result<(String, String), VarsFileError> {
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

    use super::*;
    use crate::lang::ast::{ActionKind, File};
    use crate::lang::parse::parse_file;

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
        vars.set("user_id", "from-var");
        vars.set_setup("user_id", "from-setup");
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
            store.set(name, value);
        }
        store.set("shared", "flag");
        store.set("captured", "cap");
        store.set("captured", "cap2");
        assert_eq!(store.get("from_file"), Some("1"));
        assert_eq!(store.get("shared"), Some("flag"));
        assert_eq!(store.get("captured"), Some("cap2"));
    }

    #[test]
    fn resolves_literals_variables_and_env() {
        let mut store = VarStore::new();
        store.set("user", "alice");
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
    fn masking_prefers_the_longest_secret_at_overlaps() {
        let mut masker = Masker::new();
        masker.record("abc");
        masker.record("abcdef");
        masker.record("def");
        assert_eq!(masker.mask("abcdef"), "***");
        assert_eq!(masker.mask("xabcdefx abc def"), "x***x *** ***");
    }

    #[test]
    fn empty_secrets_are_ignored() {
        let mut masker = Masker::new();
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
