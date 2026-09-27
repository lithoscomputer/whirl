//! The first Jev request: what kind of action the instruction asks for,
//! and the details of that action that need no page. The families, their
//! descriptions, the extra questions, and the rule that merges a split vote
//! are ported from Stagehand's `packages/extension/services/jevAct/
//! pipeline.ts` (browserbase/stagehand#2953).
//!
//! Stagehand is distributed under this license:
//!
//! MIT License
//!
//! Copyright (c) 2024 Browserbase Inc.
//!
//! Permission is hereby granted, free of charge, to any person obtaining a
//! copy of this software and associated documentation files (the
//! "Software"), to deal in the Software without restriction, including
//! without limitation the rights to use, copy, modify, merge, publish,
//! distribute, sublicense, and/or sell copies of the Software, and to permit
//! persons to whom the Software is furnished to do so, subject to the
//! following conditions:
//!
//! The above copyright notice and this permission notice shall be included
//! in all copies or substantial portions of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
//! OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
//! MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
//! NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
//! DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR
//! OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE
//! USE OR OTHER DEALINGS IN THE SOFTWARE.

use std::collections::HashMap;

use serde_json::{Map, Value as Json, json};

use super::client::{JevAnswer, JevQuestion, choice};
use crate::lang::ast::MouseButton;

/// Each kind of action and how Jev reads it. Whirl has no scroll, drag, or
/// page-level actions, but naming them keeps such instructions out of the
/// families Whirl can act on.
pub(super) const FAMILIES: &[(&str, &str)] = &[
    (
        "click",
        "Click or tap an element: a button, link, checkbox, radio button, tab, menu item, calendar day, or a control that expands something. Includes opening, going to, or following something through a link or button on the page",
    ),
    ("double_click", "Explicitly double-click an element"),
    ("hover", "Hover the mouse over an element without clicking"),
    (
        "fill",
        "Type or fill text into an input field, search box, or text area",
    ),
    (
        "select",
        "Choose an option from a dropdown, combobox, or select menu",
    ),
    (
        "press",
        "Press a keyboard key such as Enter, Tab, or Escape",
    ),
    (
        "scroll",
        "Scroll the page or a container to a position or percentage",
    ),
    ("drag", "Drag one element and drop it onto another element"),
    (
        "not_an_action",
        "Not a request to interact with the page at all: a general knowledge question, chit-chat, or nonsense",
    ),
    (
        "unsupported",
        "A browser task outside the other kinds: reading or extracting information, loading a typed URL in the address bar, or uploading a file",
    ),
];

/// The keys the `key` question offers.
const KEYS: &[&str] = &[
    "Enter",
    "Tab",
    "Escape",
    "Space",
    "Backspace",
    "Delete",
    "ArrowUp",
    "ArrowDown",
    "ArrowLeft",
    "ArrowRight",
    "PageUp",
    "PageDown",
    "Home",
    "End",
];

/// Two families that start with the same step, and which one wins when
/// together they are sure. `None` means the more likely of the two.
const MERGED: &[(&str, &str, Option<&str>)] = &[
    ("click", "select", None),
    ("click", "double_click", Some("click")),
    // "Run the search": a button click and Enter do the same thing.
    ("click", "press", None),
];
/// The combined probability a merged pair needs.
const MERGED_MASS: f64 = 0.85;

/// What Jev said about the texts a fill could type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FillValue {
    /// The instruction offered no texts to choose among.
    NotAsked,
    /// The text at this index.
    Chosen(usize),
    /// None of the offered texts is the text to type.
    NoneOfThese,
    /// Jev could not decide.
    Unsure,
}

/// The key of the `fill_value` question's none-of-these option.
const NO_VALUE: &str = "none_match";

/// What the first request found.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Intent {
    pub(super) family:          &'static str,
    /// True when a click also fits: the family is `click`, or a split vote
    /// between `click` and another family was merged.
    pub(super) click_fits:      bool,
    /// The key to press, when Jev named one.
    pub(super) key:             Option<&'static str>,
    /// The mouse button a click presses; left unless Jev is sure of
    /// another.
    pub(super) button:          MouseButton,
    /// The end state a checkbox or switch must reach, when named.
    pub(super) toggle:          Option<bool>,
    /// True when typing must be followed by choosing a suggestion.
    pub(super) pick_suggestion: bool,
    pub(super) fill_value:      FillValue,
}

/// The questions of the first request. `fill_values` are the quoted
/// strings and placeholders a fill could type, each marked when it is a
/// placeholder; with any, Jev also says which one is the text to type.
pub(super) fn questions(
    instruction: &str,
    fill_values: &[(String, bool)],
) -> Vec<(&'static str, JevQuestion)> {
    let options = |pairs: &[(&str, &str)]| -> Map<String, Json> {
        pairs
            .iter()
            .map(|(name, meaning)| ((*name).to_owned(), json!(meaning)))
            .collect()
    };
    let mut keys: Map<String, Json> = KEYS
        .iter()
        .map(|key| ((*key).to_owned(), json!(format!("The {key} key"))))
        .collect();
    keys.insert(
        "other".to_owned(),
        json!("Some other key, or the instruction is not a key press"),
    );
    let mut questions = vec![
        (
            "family",
            choice(
                json!({
                    "question": "Which kind of browser action does the instruction ask for?",
                    "instruction": instruction,
                }),
                options(FAMILIES),
            ),
        ),
        (
            "mouse_button",
            choice(
                json!("If the instruction is a click, which mouse button does it ask for?"),
                options(&[
                    (
                        "left",
                        "A normal click, or no button mentioned, or not a click",
                    ),
                    ("right", "A right click or context-menu click"),
                    ("middle", "A middle click"),
                ]),
            ),
        ),
        (
            "toggle_state",
            choice(
                json!(
                    "If the instruction is about a checkbox, switch, or toggle, which END STATE does it ask for?"
                ),
                options(&[
                    (
                        "on",
                        "It must end up checked, enabled, selected, or turned on",
                    ),
                    (
                        "off",
                        "It must end up unchecked, disabled, deselected, or turned off",
                    ),
                    (
                        "unspecified",
                        "It only says to click or toggle it, or the instruction is not about a checkbox or switch",
                    ),
                ]),
            ),
        ),
        (
            "after_typing",
            choice(
                json!("If the instruction types text, what else does it ask for after typing?"),
                options(&[
                    (
                        "nothing",
                        "Only typing the text, or the instruction does not type anything",
                    ),
                    (
                        "pick_suggestion",
                        "It also asks to choose one of the suggestions or autocomplete options that appear while typing",
                    ),
                ]),
            ),
        ),
        (
            "key",
            choice(
                json!("If the instruction asks to press a keyboard key, which key?"),
                keys,
            ),
        ),
    ];
    if !fill_values.is_empty() {
        let mut values: Map<String, Json> = fill_values
            .iter()
            .enumerate()
            .map(|(index, (value, placeholder))| {
                let described = if *placeholder {
                    json!({ "variable_placeholder": value })
                } else {
                    json!({ "quoted_text": value })
                };
                (format!("value_{index}"), described)
            })
            .collect();
        values.insert(
            NO_VALUE.to_owned(),
            json!("None of these is the text to type; the text to type is not among them"),
        );
        questions.push((
            "fill_value",
            choice(
                json!(
                    "Which of these is the literal text the instruction wants typed into the field? A quoted field name or label is NOT the text to type. A %variable% placeholder stands for the text to type."
                ),
                values,
            ),
        ));
    }
    questions
}

/// Reads the first request's answers. `None` when Jev is not sure of the
/// family.
pub(super) fn read(answers: &HashMap<String, JevAnswer>, accept: f64) -> Option<Intent> {
    let family_answer = answers.get("family")?;
    let family = sure_choice(family_answer, accept, true)?;
    let family = FAMILIES
        .iter()
        .map(|(name, _)| *name)
        .find(|name| *name == family)?;
    let click_fits = family == "click"
        || matches!(family_answer, JevAnswer::Choice { confidence, .. } if *confidence < accept);
    let named = |id: &str| {
        answers
            .get(id)
            .and_then(|answer| sure_choice(answer, accept, false))
    };
    Some(Intent {
        family,
        click_fits,
        key: named("key").and_then(|key| KEYS.iter().copied().find(|known| *known == key)),
        button: named("mouse_button")
            .as_deref()
            .and_then(MouseButton::from_name)
            .unwrap_or(MouseButton::Left),
        toggle: match named("toggle_state").as_deref() {
            Some("on") => Some(true),
            Some("off") => Some(false),
            _ => None,
        },
        pick_suggestion: named("after_typing").as_deref() == Some("pick_suggestion"),
        fill_value: fill_value(answers.get("fill_value"), accept),
    })
}

/// Reads the `fill_value` answer. "None of these" counts whatever its
/// confidence: the text is then read another way.
fn fill_value(answer: Option<&JevAnswer>, accept: f64) -> FillValue {
    let Some(JevAnswer::Choice {
        choice, confidence, ..
    }) = answer
    else {
        return FillValue::NotAsked;
    };
    if choice == NO_VALUE {
        return FillValue::NoneOfThese;
    }
    match choice.strip_prefix("value_").map(str::parse::<usize>) {
        Some(Ok(index)) if *confidence >= accept => FillValue::Chosen(index),
        _ => FillValue::Unsure,
    }
}

/// A choice Jev is sure of. For the family, two families that start with
/// the same step count together (see [`MERGED`]).
fn sure_choice(answer: &JevAnswer, accept: f64, merge: bool) -> Option<String> {
    let JevAnswer::Choice {
        choice,
        confidence,
        probabilities,
    } = answer
    else {
        return None;
    };
    if *confidence >= accept {
        return Some(choice.clone());
    }
    if !merge {
        return None;
    }
    let probability = |name: &str| probabilities.get(name).copied().unwrap_or(0.0);
    let top = probabilities
        .iter()
        .max_by(|left, right| left.1.total_cmp(right.1))
        .map(|(name, _)| name.as_str())?;
    MERGED.iter().find_map(|&(first, second, winner)| {
        let sure = probability(first) + probability(second) >= MERGED_MASS
            && (top == first || top == second);
        sure.then(|| {
            winner
                .unwrap_or(if probability(first) >= probability(second) {
                    first
                } else {
                    second
                })
                .to_owned()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(choice: &str, confidence: f64, probabilities: &[(&str, f64)]) -> JevAnswer {
        JevAnswer::Choice {
            choice: choice.to_owned(),
            confidence,
            probabilities: probabilities
                .iter()
                .map(|(name, probability)| ((*name).to_owned(), *probability))
                .collect(),
        }
    }

    fn answers(family: JevAnswer) -> HashMap<String, JevAnswer> {
        HashMap::from([
            ("family".to_owned(), family),
            (
                "mouse_button".to_owned(),
                answer("left", 0.95, &[("left", 0.95)]),
            ),
            ("toggle_state".to_owned(), answer("on", 0.9, &[("on", 0.9)])),
            (
                "after_typing".to_owned(),
                answer("nothing", 0.9, &[("nothing", 0.9)]),
            ),
            ("key".to_owned(), answer("Enter", 0.9, &[("Enter", 0.9)])),
        ])
    }

    #[test]
    fn a_sure_family_and_its_details_are_read() {
        let intent = read(&answers(answer("click", 0.9, &[("click", 0.9)])), 0.7).expect("sure");
        assert_eq!(intent, Intent {
            family:          "click",
            click_fits:      true,
            key:             Some("Enter"),
            button:          MouseButton::Left,
            toggle:          Some(true),
            pick_suggestion: false,
            fill_value:      FillValue::NotAsked,
        });
    }

    #[test]
    fn a_sure_right_or_middle_button_is_read_and_an_unsure_one_is_left() {
        let button = |choice: &str, confidence: f64| {
            let mut answers = answers(answer("click", 0.9, &[("click", 0.9)]));
            answers.insert(
                "mouse_button".to_owned(),
                answer(choice, confidence, &[(choice, confidence)]),
            );
            read(&answers, 0.7).map(|intent| intent.button)
        };
        assert_eq!(button("right", 0.9), Some(MouseButton::Right));
        assert_eq!(button("middle", 0.9), Some(MouseButton::Middle));
        assert_eq!(button("right", 0.5), Some(MouseButton::Left));
    }

    #[test]
    fn a_split_between_click_and_select_merges() {
        let split = answer("click", 0.4, &[
            ("click", 0.5),
            ("select", 0.4),
            ("fill", 0.1),
        ]);
        assert_eq!(
            read(&answers(split), 0.7).map(|intent| intent.family),
            Some("click")
        );
        let select = answer("select", 0.45, &[("select", 0.51), ("click", 0.45)]);
        let merged = read(&answers(select), 0.7).expect("merged");
        assert_eq!((merged.family, merged.click_fits), ("select", true));
        let double = answer("double_click", 0.4, &[
            ("double_click", 0.5),
            ("click", 0.4),
        ]);
        assert_eq!(
            read(&answers(double), 0.7).map(|intent| intent.family),
            Some("click")
        );
        let unsure = answer("click", 0.3, &[
            ("click", 0.5),
            ("fill", 0.3),
            ("select", 0.2),
        ]);
        assert_eq!(read(&answers(unsure), 0.7), None);
    }

    #[test]
    fn every_question_is_asked_in_one_request() {
        let ids: Vec<&str> = questions("x", &[]).into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, [
            "family",
            "mouse_button",
            "toggle_state",
            "after_typing",
            "key"
        ]);
        let with_values = questions("x", &[("Ada".to_owned(), false)]);
        assert_eq!(with_values.last().map(|(id, _)| *id), Some("fill_value"));
    }

    #[test]
    fn the_fill_value_answer_names_a_text_or_none() {
        let pick = |choice: &str, confidence: f64| {
            fill_value(
                Some(&answer(choice, confidence, &[(choice, confidence)])),
                0.7,
            )
        };
        assert_eq!(pick("value_1", 0.9), FillValue::Chosen(1));
        assert_eq!(pick("value_1", 0.5), FillValue::Unsure);
        assert_eq!(pick("none_match", 0.4), FillValue::NoneOfThese);
        assert_eq!(fill_value(None, 0.7), FillValue::NotAsked);
    }
}
