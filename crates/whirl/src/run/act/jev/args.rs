//! Arguments the Jev planner reads from the instruction itself. Jev cannot
//! write text, so an argument must be a quoted string, a placeholder, a key
//! name, or an option the page shows. Anything else goes to the language
//! model.

use crate::run::act::instruction::{Instruction, quoted_strings, same_text};

/// The text to fill: the one quoted string or placeholder in the
/// instruction that is not the field's own name, which an instruction
/// such as `type "Ada" into "Name"` also quotes.
pub(crate) fn fill_value(instruction: &Instruction, field_name: Option<&str>) -> Option<String> {
    let prompt = instruction.prompt();
    let mut values: Vec<String> = instruction
        .bindings()
        .placeholders()
        .into_iter()
        .filter(|placeholder| prompt.contains(placeholder.as_str()))
        .collect();
    for quoted in quoted_strings(prompt) {
        let is_field = field_name.is_some_and(|name| same_text(name, quoted));
        let in_placeholder = values.iter().any(|value| value.contains(quoted));
        if !is_field && !in_placeholder && !values.iter().any(|value| value == quoted) {
            values.push(quoted.to_owned());
        }
    }
    match values.as_slice() {
        [value] => Some(value.clone()),
        _ => None,
    }
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

/// The one option of a native select that the instruction names, spelled
/// as the page spells it. The select's own name does not count.
pub(crate) fn option<'a>(
    instruction: &str,
    options: &[&'a str],
    select_name: Option<&str>,
) -> Option<&'a str> {
    let words = |text: &str| -> Vec<String> {
        text.split(|ch: char| !ch.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .map(str::to_lowercase)
            .collect()
    };
    let said = words(instruction);
    let named: Vec<&str> = options
        .iter()
        .copied()
        .filter(|option| !select_name.is_some_and(|name| same_text(name, option)))
        .filter(|option| {
            let option = words(option);
            !option.is_empty() && said.windows(option.len()).any(|window| window == option)
        })
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
    fn the_fill_value_is_the_quoted_text_that_is_not_the_field() {
        let typed = literal("type \"Ada\" into the \"Name\" field");
        assert_eq!(fill_value(&typed, Some("Name")), Some("Ada".to_owned()));
        assert_eq!(fill_value(&typed, Some("First name")), None);
        assert_eq!(
            fill_value(&literal("type Ada into Name"), Some("Name")),
            None
        );
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
        assert_eq!(
            fill_value(&typed, Some("Password")),
            Some("%secret1%".to_owned())
        );
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
