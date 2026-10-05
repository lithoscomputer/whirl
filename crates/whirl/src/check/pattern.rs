//! Regex literals for checks and filters: ECMAScript syntax in Unicode
//! mode, compiled with the `regress` crate (SPEC 3.1).

use std::fmt;
use std::ops::Range;

/// The `i`, `s`, and `m` flags a regex literal may carry. Unicode mode is
/// always on.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PatternFlags {
    pub(crate) ignore_case: bool,
    pub(crate) dot_all:     bool,
    pub(crate) multiline:   bool,
}

impl PatternFlags {
    /// The flags as ECMAScript flag letters, always with `u`.
    fn letters(self) -> String {
        let mut letters = String::from("u");
        if self.ignore_case {
            letters.push('i');
        }
        if self.dot_all {
            letters.push('s');
        }
        if self.multiline {
            letters.push('m');
        }
        letters
    }
}

/// A compiled regex literal.
#[derive(Clone)]
pub(crate) struct Pattern {
    source: String,
    flags:  PatternFlags,
    regex:  regress::Regex,
}

/// A pattern that is invalid in Unicode mode.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("invalid regex /{pattern}/: {reason}")]
pub(crate) struct PatternError {
    pub(crate) pattern: String,
    pub(crate) reason:  String,
}

impl Pattern {
    /// Compiles a regex literal as written between the slashes. The `\/`
    /// delimiter escape becomes a plain `/`; every other escape is passed
    /// to the engine unchanged.
    pub(crate) fn new(written: &str, flags: PatternFlags) -> Result<Self, PatternError> {
        let source = unescape_delimiter(written);
        let regex =
            regress::Regex::with_flags(&source, flags.letters().as_str()).map_err(|error| {
                PatternError {
                    pattern: written.to_owned(),
                    reason:  error.to_string(),
                }
            })?;
        Ok(Self {
            source,
            flags,
            regex,
        })
    }

    /// True when the pattern finds a match anywhere in `text`.
    pub(crate) fn is_match(&self, text: &str) -> bool {
        self.regex.find(text).is_some()
    }

    /// The `regex` filter's result: capture group 1 of the first match, or
    /// the whole match when the pattern has no group (SPEC 9.5). `None`
    /// when nothing matches or group 1 did not take part in the match.
    pub(crate) fn extract<'t>(&self, text: &'t str) -> Option<&'t str> {
        let found = self.regex.find(text)?;
        let range = if found.captures.is_empty() {
            found.range()
        } else {
            found.group(1)?
        };
        Some(&text[range])
    }

    /// Replaces every match, expanding `$1`, `$<name>`, `$&`, `` $` ``,
    /// `$'`, and `$$` in `replacement` as ECMAScript's
    /// `String.prototype.replace` does (SPEC 9.5).
    pub(crate) fn replace_all(&self, text: &str, replacement: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut last = 0;
        for found in self.regex.find_iter(text) {
            let range = found.range();
            out.push_str(&text[last..range.start]);
            expand(replacement, text, &found, &mut out);
            last = range.end;
        }
        out.push_str(&text[last..]);
        out
    }

    /// Checks a written pattern without keeping the compiled form.
    pub(crate) fn validate(written: &str, flags: PatternFlags) -> Result<(), PatternError> {
        Self::new(written, flags).map(|_| ())
    }
}

impl fmt::Debug for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "/{}/{}", self.source, self.flags.letters())
    }
}

/// Removes the `\/` delimiter escape. A `\\` pair stays intact, so `\\/`
/// keeps its escaped backslash.
fn unescape_delimiter(written: &str) -> String {
    let mut out = String::with_capacity(written.len());
    let mut chars = written.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('/') => out.push('/'),
                Some(next) => {
                    out.push('\\');
                    out.push(next);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// ECMAScript `GetSubstitution` for one match.
fn expand(replacement: &str, text: &str, found: &regress::Match, out: &mut String) {
    let range = found.range();
    let group = |index: usize| -> Option<Range<usize>> { found.group(index) };
    let bytes = replacement.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' || i + 1 >= bytes.len() {
            let ch = replacement[i..]
                .chars()
                .next()
                .expect("index is on a char boundary");
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        match bytes[i + 1] {
            b'$' => {
                out.push('$');
                i += 2;
            }
            b'&' => {
                out.push_str(&text[range.clone()]);
                i += 2;
            }
            b'`' => {
                out.push_str(&text[..range.start]);
                i += 2;
            }
            b'\'' => {
                out.push_str(&text[range.end..]);
                i += 2;
            }
            b'<' => match replacement[i + 2..].find('>') {
                Some(end) if found.named_groups().next().is_some() => {
                    let name = &replacement[i + 2..i + 2 + end];
                    if let Some(span) = found.named_group(name) {
                        out.push_str(&text[span]);
                    }
                    i += end + 3;
                }
                _ => {
                    out.push('$');
                    i += 1;
                }
            },
            b'0'..=b'9' => {
                let groups = found.captures.len();
                let one = usize::from(bytes[i + 1] - b'0');
                let two = bytes
                    .get(i + 2)
                    .filter(|byte| byte.is_ascii_digit())
                    .map(|byte| one * 10 + usize::from(byte - b'0'));
                match (two, one) {
                    (Some(index), _) if (1..=groups).contains(&index) => {
                        if let Some(span) = group(index) {
                            out.push_str(&text[span]);
                        }
                        i += 3;
                    }
                    (_, index) if (1..=groups).contains(&index) => {
                        if let Some(span) = group(index) {
                            out.push_str(&text[span]);
                        }
                        i += 2;
                    }
                    _ => {
                        out.push('$');
                        i += 1;
                    }
                }
            }
            _ => {
                out.push('$');
                i += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(written: &str) -> Pattern {
        Pattern::new(written, PatternFlags::default()).expect("valid pattern")
    }

    #[test]
    fn extracts_group_one_or_the_whole_match() {
        assert_eq!(
            pattern(r"Order #(\w+)").extract("Order #A42 done"),
            Some("A42")
        );
        assert_eq!(pattern(r"\d+").extract("total 1299 items"), Some("1299"));
        assert_eq!(pattern(r"(?<id>\d+)").extract("id 7"), Some("7"));
        assert_eq!(pattern(r"x").extract("abc"), None);
    }

    #[test]
    fn unescapes_only_the_delimiter() {
        assert!(pattern(r"a\/b").is_match("a/b"));
        assert!(pattern(r"\d\/\d").is_match("1/2"));
    }

    #[test]
    fn rejects_patterns_that_are_invalid_in_unicode_mode() {
        assert!(Pattern::new(r"a\-b", PatternFlags::default()).is_err());
        assert!(Pattern::new(r"(", PatternFlags::default()).is_err());
    }

    #[test]
    fn applies_flags() {
        let flags = PatternFlags {
            ignore_case: true,
            ..PatternFlags::default()
        };
        assert!(
            Pattern::new("hello", flags)
                .expect("valid")
                .is_match("HeLLo")
        );
        assert!(!pattern("hello").is_match("HELLO"));
    }

    #[test]
    fn expands_ecmascript_replacement_patterns() {
        let p = pattern(r"(\w+)@(?<host>\w+)");
        assert_eq!(p.replace_all("ada@lovelace x", "$2:$1"), "lovelace:ada x");
        assert_eq!(p.replace_all("ada@lovelace", "$<host>"), "lovelace");
        assert_eq!(p.replace_all("ada@lovelace", "[$&]"), "[ada@lovelace]");
        assert_eq!(p.replace_all("ada@lovelace", "$$1"), "$1");
        assert_eq!(p.replace_all("ada@lovelace", "$9"), "$9");
        assert_eq!(pattern(r"\d").replace_all("a1b22", "x"), "axbxx");
    }
}
