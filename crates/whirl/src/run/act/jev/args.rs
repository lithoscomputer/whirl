//! Arguments the Jev planner reads from the instruction itself. Jev cannot
//! write text, so an argument is a quoted string or a placeholder Jev
//! chooses, a key name, an option the page shows, or text a small model
//! call copies from the instruction.

use crate::run::act::instruction::{Instruction, quoted_strings, same_text};

/// The texts a fill could type: the placeholders and quoted strings in the
/// instruction, in order. One of them may name the field instead, as in
/// `type "Ada" into "Name"`; Jev's `fill_value` question tells them apart.
pub(crate) fn fill_values(instruction: &Instruction) -> Vec<String> {
    let prompt = instruction.prompt();
    let mut values: Vec<String> = instruction
        .bindings()
        .placeholders()
        .into_iter()
        .filter(|placeholder| prompt.contains(placeholder.as_str()))
        .collect();
    for quoted in quoted_strings(prompt) {
        if !values.iter().any(|value| value.contains(quoted)) {
            values.push(quoted.to_owned());
        }
    }
    values
}

/// True for a value that is a placeholder, such as `%env.PASSWORD%`.
pub(crate) fn is_placeholder(instruction: &Instruction, value: &str) -> bool {
    instruction
        .bindings()
        .placeholders()
        .iter()
        .any(|placeholder| placeholder == value)
}

/// The key a `press` instruction names, as Playwright spells it: `press
/// Enter`, `press the tab key`, or one character.
pub(crate) fn key(instruction: &str) -> Option<String> {
    const NAMED: &[(&str, &str)] = &[
        ("enter", "Enter"),
        ("return", "Enter"),
        ("tab", "Tab"),
        ("escape", "Escape"),
        ("esc", "Escape"),
        ("space", "Space"),
        ("backspace", "Backspace"),
        ("delete", "Delete"),
        ("arrowdown", "ArrowDown"),
        ("arrowup", "ArrowUp"),
    ];
    let lower = instruction.to_lowercase();
    let words: Vec<&str> = lower
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    let press = words
        .iter()
        .position(|&word| word == "press" || word == "hit")?;
    let word = words
        .get(press + 1)
        .filter(|&&word| word != "the")
        .or_else(|| words.get(press + 2))?;
    if let Some((_, key)) = NAMED.iter().find(|(name, _)| name == word) {
        return Some((*key).to_owned());
    }
    let mut chars = word.chars();
    match (chars.next(), chars.next()) {
        (Some(ch), None) => Some(ch.to_string()),
        _ => None,
    }
}

/// Whether the instruction says `label` as a run of whole words, ignoring
/// case and punctuation.
pub(crate) fn says(instruction: &str, label: &str) -> bool {
    let label = words(label);
    !label.is_empty()
        && words(instruction)
            .windows(label.len())
            .any(|window| window == label)
}

fn words(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// The one option of a native select that the instruction names, spelled
/// as the page spells it. The select's own name does not count.
pub(crate) fn option<'a>(
    instruction: &str,
    options: &[&'a str],
    select_name: Option<&str>,
) -> Option<&'a str> {
    let named: Vec<&str> = options
        .iter()
        .copied()
        .filter(|option| !select_name.is_some_and(|name| same_text(name, option)))
        .filter(|option| says(instruction, option))
        .collect();
    match named.as_slice() {
        [option] => Some(option),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::ast::{Span, Value, ValueSegment};
    use crate::run::vars::VarStore;

    fn instruction(segments: Vec<ValueSegment>, vars: &mut VarStore) -> Instruction {
        let value = Value {
            segments,
            span: Span {
                line:   1,
                column: 1,
                len:    1,
            },
            quoted: true,
        };
        Instruction::try_new(&value, vars).expect("resolves")
    }

    fn literal(text: &str) -> Instruction {
        instruction(
            vec![ValueSegment::Literal(text.to_owned())],
            &mut VarStore::new(),
        )
    }

    #[test]
    fn fill_values_are_the_quoted_strings() {
        let typed = literal("type \"Ada\" into the \"Name\" field");
        assert_eq!(fill_values(&typed), vec!["Ada", "Name"]);
        assert!(fill_values(&literal("type Ada into Name")).is_empty());
    }

    #[test]
    fn a_placeholder_is_a_fill_value() {
        let mut vars = VarStore::new();
        vars.record_secret("hunter2");
        vars.set_input("password", "hunter2");
        let typed = instruction(
            vec![
                ValueSegment::Literal("type ".to_owned()),
                ValueSegment::Var("password".to_owned()),
                ValueSegment::Literal(" into the password field".to_owned()),
            ],
            &mut vars,
        );
        assert_eq!(fill_values(&typed), vec!["%secret1%"]);
        assert!(is_placeholder(&typed, "%secret1%"));
    }

    #[test]
    fn a_label_is_said_as_whole_words() {
        assert!(says("choose United Kingdom, please", "united kingdom"));
        assert!(!says("choose Portugal", "Port"));
        assert!(!says("choose Canada", ""));
    }

    #[test]
    fn keys_are_read_from_press_instructions() {
        assert_eq!(key("press enter"), Some("Enter".to_owned()));
        assert_eq!(key("Press the Tab key"), Some("Tab".to_owned()));
        assert_eq!(key("press k"), Some("k".to_owned()));
        assert_eq!(key("press the big button"), None);
        assert_eq!(key("submit the form"), None);
    }

    #[test]
    fn the_option_is_the_one_the_instruction_names() {
        let sizes = ["Small", "Medium", "Large"];
        assert_eq!(
            option("choose Large from the size dropdown", &sizes, Some("Size")),
            Some("Large")
        );
        assert_eq!(option("choose a size", &sizes, Some("Size")), None);
        assert_eq!(option("pick small or large", &sizes, Some("Size")), None);
        let countries = ["United States", "United Kingdom"];
        assert_eq!(
            option("choose united kingdom", &countries, None),
            Some("United Kingdom")
        );
    }
}
