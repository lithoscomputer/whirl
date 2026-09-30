//! Options set on the command line (SPEC 5, 13): repeatable `-O key=value`
//! arguments and the flags that set one option, such as `--browser`.
//!
//! Settings resolve in this order: built-in defaults, then the file's
//! `[Options]` lines, then the command line. [`CliOptions::apply`] puts the
//! command line's lines in place of the file's lines for the same key, so
//! every later step (lint, the run, reports) reads one list of options.
//! Every command that takes `-O` shares this type.

use crate::lang::ast::snapshot::SnapshotOption;
use crate::lang::ast::{File, FileOption, OptionLine, OptionSource, Span};
use crate::lang::parse::{OPTION_KEYS, parse_command_line_option};

/// The option keys whose values form a list. More than one command-line
/// value for such a key forms a list that replaces the file's list, and an
/// empty value clears it (SPEC 13).
const LIST_KEYS: [&str; 2] = ["allow-hosts", "snapshot-mask"];

/// A flag that sets one option, such as `--browser NAME` (SPEC 13).
#[derive(Clone, Copy, Debug)]
pub(crate) struct OptionFlag<'a> {
    pub(crate) flag:  &'static str,
    pub(crate) key:   &'static str,
    pub(crate) value: Option<&'a str>,
}

/// A usage error in a command-line option (SPEC 13, exit 4).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct CliOptionError(String);

/// The options that the command line sets, validated. Each key holds the
/// lines that replace the file's lines for that key; no lines clear it.
#[derive(Clone, Debug, Default)]
pub(crate) struct CliOptions {
    settings: Vec<(&'static str, Vec<FileOption>)>,
}

/// The command-line values of one key, in order, with how the user wrote
/// the key and each value, for messages.
struct Written {
    key:    &'static str,
    origin: String,
    values: Vec<Argument>,
}

/// One command-line value: its text in `key: value` syntax, and the
/// argument as the user wrote it.
struct Argument {
    syntax: String,
    label:  String,
}

impl CliOptions {
    /// Validates `-O` arguments and option flags. `pairs` are the
    /// `key=value` texts of `-O` in order. A flag's value is one literal
    /// value, as if quoted; a `-O` value has the syntax of the value in a
    /// `key: value` line.
    pub(crate) fn try_new(
        pairs: &[String],
        flags: &[OptionFlag<'_>],
    ) -> Result<Self, CliOptionError> {
        let mut written: Vec<Written> = Vec::new();
        for pair in pairs {
            let Some((key, value)) = pair.split_once('=') else {
                return Err(CliOptionError(format!("-O {pair}: expected key=value")));
            };
            let Some(key) = OPTION_KEYS.iter().copied().find(|known| *known == key) else {
                return Err(CliOptionError(format!(
                    "-O {key}: unknown option key `{key}`"
                )));
            };
            if key == "setup" {
                return Err(CliOptionError(
                    "-O setup: a setup flow belongs to its file; set it in [Options]".to_owned(),
                ));
            }
            let value = Argument {
                syntax: value.to_owned(),
                label:  format!("-O {pair}"),
            };
            match written.iter_mut().find(|known| known.key == key) {
                Some(known) => known.values.push(value),
                None => written.push(Written {
                    key,
                    origin: format!("-O {key}"),
                    values: vec![value],
                }),
            }
        }
        for flag in flags {
            let Some(value) = flag.value else {
                continue;
            };
            if written.iter().any(|known| known.key == flag.key) {
                return Err(CliOptionError(format!(
                    "{} and -O {key} both set `{key}`; use one",
                    flag.flag,
                    key = flag.key
                )));
            }
            written.push(Written {
                key:    flag.key,
                origin: flag.flag.to_owned(),
                values: vec![Argument {
                    syntax: quote(value),
                    label:  format!("{} {value}", flag.flag),
                }],
            });
        }
        let settings = written
            .into_iter()
            .map(|written| Ok((written.key, lines_of(&written)?)))
            .collect::<Result<_, CliOptionError>>()?;
        Ok(Self { settings })
    }

    /// True when the command line sets `key`.
    pub(crate) fn sets(&self, key: &str) -> bool {
        self.settings.iter().any(|(known, _)| *known == key)
    }

    /// Puts the command line's lines in place of the file's lines for each
    /// key that the command line sets.
    pub(crate) fn apply(&self, file: &mut File) {
        for (key, options) in &self.settings {
            file.options.retain(|line| line.option.key() != *key);
            file.options.extend(options.iter().map(|option| OptionLine {
                option: option.clone(),
                line:   0,
                span:   Span {
                    line:   0,
                    column: 0,
                    len:    0,
                },
                source: OptionSource::CommandLine,
            }));
        }
    }
}

/// The lines that one key's command-line values stand for. A scalar key
/// takes its last value; every value is still validated.
fn lines_of(written: &Written) -> Result<Vec<FileOption>, CliOptionError> {
    let is_list = LIST_KEYS.contains(&written.key);
    let empty = written
        .values
        .iter()
        .filter(|value| value.syntax.is_empty())
        .count();
    if is_list && empty == written.values.len() {
        return Ok(Vec::new());
    }
    if is_list && empty > 0 {
        return Err(CliOptionError(format!(
            "{}: an empty value clears the list, so it cannot be combined with other values",
            written.origin
        )));
    }
    let mut options = Vec::with_capacity(written.values.len());
    for value in &written.values {
        let option = parse_command_line_option(written.key, &value.syntax)
            .map_err(|message| CliOptionError(format!("{}: {message}", value.label)))?;
        options.push(option);
    }
    if !is_list {
        options.drain(..options.len() - 1);
        return Ok(options);
    }
    let clears_masks = options
        .iter()
        .any(|option| matches!(option, FileOption::Snapshot(SnapshotOption::Mask(None))));
    if clears_masks && options.len() > 1 {
        return Err(CliOptionError(format!(
            "{}: `none` clears the masks, so it must be the only value",
            written.origin
        )));
    }
    // One `allow-hosts` line holds the whole list.
    if let [FileOption::AllowHosts(_), ..] = options.as_slice() {
        let hosts = options
            .into_iter()
            .flat_map(|option| match option {
                FileOption::AllowHosts(hosts) => hosts,
                _ => Vec::new(),
            })
            .collect();
        return Ok(vec![FileOption::AllowHosts(hosts)]);
    }
    Ok(options)
}

/// A flag's value as one quoted Whirl value, so that its text is taken
/// literally: no interpolation, comments, or list splitting (SPEC 3.1).
fn quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for ch in value.chars() {
        match ch {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '{' => quoted.push_str("\\{"),
            '\n' => quoted.push_str("\\n"),
            '\t' => quoted.push_str("\\t"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::lang::ast::{BrowserKind, OptionValue, Value};
    use crate::lang::parse::parse_file;

    fn options(pairs: &[&str]) -> Result<CliOptions, CliOptionError> {
        let pairs: Vec<String> = pairs.iter().map(|pair| (*pair).to_owned()).collect();
        CliOptions::try_new(&pairs, &[])
    }

    fn applied(source: &str, cli: &CliOptions) -> Vec<(FileOption, OptionSource)> {
        let mut file = parse_file(Path::new("flow.whirl"), source).expect("the flow parses");
        cli.apply(&mut file);
        file.options
            .into_iter()
            .map(|line| (line.option, line.source))
            .collect()
    }

    fn error(pairs: &[&str]) -> String {
        options(pairs)
            .expect_err("the options are invalid")
            .to_string()
    }

    #[test]
    fn a_scalar_takes_the_last_value_and_replaces_the_file_line() {
        let cli = options(&["browser=webkit", "browser=firefox"]).expect("valid");
        let lines = applied(
            "[Options]\nbrowser: chromium\nviewport: 800x600\nVISIT /\n",
            &cli,
        );
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].1, OptionSource::File);
        assert_eq!(
            lines[1],
            (
                FileOption::Browser(OptionValue::Literal(BrowserKind::Firefox)),
                OptionSource::CommandLine
            )
        );
    }

    #[test]
    fn list_values_replace_the_file_list_and_an_empty_value_clears_it() {
        let file = "[Options]\nallow-hosts: a.example\nVISIT /\n";
        let cli = options(&["allow-hosts=b.example *.b.example", "allow-hosts=c.example"])
            .expect("valid");
        let lines = applied(file, &cli);
        let [(FileOption::AllowHosts(hosts), OptionSource::CommandLine)] = lines.as_slice() else {
            panic!("expected one command-line allow-hosts line: {lines:?}");
        };
        let hosts: Vec<String> = hosts.iter().filter_map(Value::as_literal).collect();
        assert_eq!(hosts, ["b.example", "*.b.example", "c.example"]);

        let cleared = applied(file, &options(&["allow-hosts="]).expect("valid"));
        assert!(cleared.is_empty(), "{cleared:?}");
        assert!(error(&["allow-hosts=", "allow-hosts=a.example"]).contains("empty value clears"));
    }

    #[test]
    fn a_mask_value_keeps_its_locator_syntax() {
        let cli = options(&["snapshot-mask=text:\"Sign in\" >> nth:0"]).expect("valid");
        let lines = applied("[Options]\nsnapshot-mask: testid:clock\nVISIT /\n", &cli);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(matches!(
            &lines[0].0,
            FileOption::Snapshot(SnapshotOption::Mask(Some(locator))) if locator.segments.len() == 2
        ));
        assert!(error(&["snapshot-mask=none", "snapshot-mask=css:x"]).contains("only value"));
    }

    #[test]
    fn invalid_keys_values_and_forms_are_usage_errors() {
        assert_eq!(error(&["browser"]), "-O browser: expected key=value");
        assert_eq!(error(&["speed=2"]), "-O speed: unknown option key `speed`");
        assert!(
            error(&["browser=netscape"]).starts_with("-O browser=netscape: invalid option value")
        );
        assert!(error(&["step-timeout=soon"]).contains("a duration"));
        assert!(error(&["browser="]).contains("expected a value"));
        assert!(error(&["setup=login.whirl"]).starts_with("-O setup:"));
        assert!(error(&["base=http://a.example #x"]).contains("comment"));
    }

    #[test]
    fn a_flag_value_is_literal_and_conflicts_with_the_same_key() {
        let flag = OptionFlag {
            flag:  "--user-agent",
            key:   "user-agent",
            value: Some("Whirl/1 (test) {{x}} # \"q\""),
        };
        let cli = CliOptions::try_new(&[], &[flag]).expect("valid");
        let lines = applied("VISIT /\n", &cli);
        let [(FileOption::UserAgent(value), _)] = lines.as_slice() else {
            panic!("expected a user-agent line: {lines:?}");
        };
        assert_eq!(
            value.as_literal().as_deref(),
            Some("Whirl/1 (test) {{x}} # \"q\"")
        );

        let pairs = ["user-agent=chrome".to_owned()];
        let error = CliOptions::try_new(&pairs, &[flag]).expect_err("both forms conflict");
        assert_eq!(
            error.to_string(),
            "--user-agent and -O user-agent both set `user-agent`; use one"
        );
        let browser = OptionFlag {
            flag:  "--browser",
            key:   "browser",
            value: Some("netscape"),
        };
        let error = CliOptions::try_new(&[], &[browser]).expect_err("invalid flag value");
        assert!(
            error
                .to_string()
                .starts_with("--browser netscape: invalid option value"),
            "{error}"
        );
    }
}
