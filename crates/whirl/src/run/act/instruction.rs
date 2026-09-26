//! The instruction an `ACT` line sends to the model (SPEC 7.4, 11).
//!
//! Masked values never leave Whirl. Each `{{env.NAME}}` segment becomes a
//! `%env.NAME%` placeholder, and a recorded secret inside any other
//! variable becomes `%secretN%`. The model writes placeholders into its
//! arguments, and Whirl puts the values back only when it builds the shim
//! command.

use crate::lang::ast::{Value, ValueSegment};
use crate::run::vars::{VarError, VarStore};

/// Placeholder names and the secret values they stand for.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct SecretBindings {
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
    pub(crate) fn placeholders(&self) -> Vec<String> {
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
pub(crate) struct UnboundPlaceholder {
    pub(crate) name: String,
}

/// The instruction text the model sees, and the secrets its placeholders
/// stand for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Instruction {
    prompt:   String,
    bindings: SecretBindings,
}

impl Instruction {
    /// Resolves an `ACT` value (SPEC 11) with masked values replaced by
    /// placeholders. Resolution records env values for masking as usual.
    pub(crate) fn try_new(value: &Value, vars: &mut VarStore) -> Result<Self, VarError> {
        let mut prompt = String::new();
        let mut bindings = SecretBindings::default();
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
                ValueSegment::Var(_) | ValueSegment::SetupVar(_) => {
                    redact_into(
                        &mut prompt,
                        &resolved,
                        vars.masker().secrets(),
                        &mut bindings,
                    );
                }
            }
        }
        Ok(Self { prompt, bindings })
    }

    pub(crate) fn prompt(&self) -> &str {
        &self.prompt
    }

    pub(crate) fn bindings(&self) -> &SecretBindings {
        &self.bindings
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::ast::Span;

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
    fn plain_variables_reach_the_model_as_values() {
        let mut vars = VarStore::new();
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
        let mut vars = VarStore::new();
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

    #[test]
    fn equal_secrets_share_one_placeholder() {
        let mut bindings = SecretBindings::default();
        assert_eq!(bindings.bind_secret("a"), "%secret1%");
        assert_eq!(bindings.bind_secret("b"), "%secret2%");
        assert_eq!(bindings.bind_secret("a"), "%secret1%");
        assert_eq!(bindings.placeholders(), vec!["%secret1%", "%secret2%"]);
    }
}
