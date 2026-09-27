//! The `ACT` prompts (SPEC 7.4), ported from Stagehand's
//! `packages/extension/prompt.ts` (`buildActSystemPrompt`,
//! `buildActPrompt`, `buildStepTwoPrompt`, and `buildObserveUserMessage`).
//!
//! Whirl's changes: element IDs are Playwright AI-snapshot refs rather than
//! Stagehand's frame-and-node IDs, the method list is Whirl's subset, and
//! the scroll, chunk, and right/middle-click rules are gone until Whirl has
//! those actions.
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

use crate::run::act::decision::ActMethod;

/// The system prompt of every `ACT` model call.
pub(crate) fn system_prompt() -> String {
    collapse_whitespace(
        "You are helping the user automate the browser by finding elements based on what \
         action the user wants to take on the page

         You will be given:
         1. a user defined instruction about what action to take
         2. a hierarchical accessibility tree showing the semantic structure of the page. The \
         tree is a hybrid of the DOM and the accessibility tree.

         Return the element that matches the instruction if it exists. If no element on the page \
         matches the instruction, set `action` to null. Do not fabricate or guess an element — \
         empty strings or placeholder values for elementId/description/method are not \
         acceptable.

         Each element in the accessibility tree has a ref in square brackets, like [ref=e12] or \
         [ref=f1e3]. Copy the ref value exactly into elementId, without the brackets or the \
         `ref=` prefix. For example, if the tree shows [ref=e12], return elementId \"e12\".",
    )
}

/// The system prompt of the call that reads the text to type from an
/// instruction, for the `--jev` planner. Ported from Stagehand's
/// `actTextArgument` (browserbase/stagehand#2953).
pub(crate) fn text_argument_system_prompt() -> String {
    collapse_whitespace(
        "You extract one argument from a browser-automation instruction: the literal text the \
         user wants typed into a field. Copy it verbatim from the instruction; never paraphrase, \
         translate, or invent. Do not return the name of the field. If a declared %placeholder% \
         stands for the text, return it as written including the percent signs.",
    )
}

/// The user message of that call: the instruction and its placeholders,
/// never the page.
pub(crate) fn text_argument_message(instruction: &str, placeholders: &[String]) -> String {
    if placeholders.is_empty() {
        format!("instruction: {instruction}")
    } else {
        format!(
            "instruction: {instruction}\ndeclared placeholders: {}",
            placeholders.join(", ")
        )
    }
}

/// The user message: the instruction prompt plus the page snapshot.
pub(crate) fn user_message(instruction: &str, hint: Option<&str>, snapshot: &str) -> String {
    match hint {
        Some(hint) => {
            format!("instruction: {instruction}\n{hint}\nAccessibility Tree: \n{snapshot}\n")
        }
        None => format!("instruction: {instruction}\nAccessibility Tree: \n{snapshot}\n"),
    }
}

/// The first planning prompt for an `ACT` instruction.
pub(crate) fn act_prompt(action: &str, placeholders: &[String]) -> String {
    let methods = method_list(ActMethod::ALL);
    let mut prompt = format!(
        "Find the most relevant element to perform an action on given the following action: \
         {action}.
  IF AND ONLY IF the action EXPLICITLY includes the word 'dropdown' and implies \
         choosing/selecting an option from a dropdown, ignore the 'General Instructions' section, \
         and follow the 'Dropdown Specific Instructions' section carefully.

  General Instructions:
    Provide an action for this element such as {methods}. Remember that to users, buttons and \
         links look the same in most cases.
    If the action is completely unrelated to a potential action to be taken on the page, or \
         no matching element exists, set `action` to null. Do not fabricate or guess an element.
    ONLY return one action. If multiple actions are relevant, return the most relevant one.
    If the action implies a key press, e.g., 'press enter', 'press a', 'press space', etc., \
         always choose the press method with the appropriate key as argument — e.g. 'a', \
         'Enter', 'Space'. Do not choose a click action on an on-screen keyboard. Capitalize the \
         first character like 'Enter', 'Tab', 'Escape' only for special keys.

  Dropdown Specific Instructions:
    For interacting with dropdowns, there are two specific cases that you need to handle.

    CASE 1: the element is a 'select' element.
      - choose the selectOptionFromDropdown method,
      - set the argument to the exact text of the option that should be selected,
      - set twoStep to false.
    CASE 2: the element is NOT a 'select' element:
      - do not attempt to directly choose the element from the dropdown. You will need to \
         click to expand the dropdown first. You will achieve this by following these \
         instructions:
        - choose the node that most closely corresponds to the given instruction EVEN if it is \
         a 'StaticText' element, or otherwise does not appear to be interactable.
        - choose the 'click' method
        - set twoStep to true.
  "
    );
    prompt.push_str(&variables_prompt(placeholders));
    prompt
}

/// The planning prompt for the second step of a two-step action.
pub(crate) fn step_two_prompt(
    original_action: &str,
    previous_action: &str,
    placeholders: &[String],
) -> String {
    let step_two_methods: Vec<ActMethod> = ActMethod::ALL
        .iter()
        .copied()
        .filter(|method| *method != ActMethod::SelectOptionFromDropdown)
        .collect();
    let methods = method_list(&step_two_methods);
    let mut prompt = format!(
        "
  The original user action was: {original_action}.
  You have just taken the following action which completed step 1 of 2: {previous_action}.

  Now, you must find the most relevant element to perform an action on in order to complete \
         step 2 of 2.

  General Instructions:
  Provide an action for this element such as {methods}. Remember that to users, buttons and \
         links look the same in most cases.
  If the action is completely unrelated to a potential action to be taken on the page, or no \
         matching element exists, set `action` to null. Do not fabricate or guess an element.
  ONLY return one action. If multiple actions are relevant, return the most relevant one.
  If the action implies a key press, e.g., 'press enter', 'press a', 'press space', etc., \
         always choose the press method with the appropriate key as argument — e.g. 'a', \
         'Enter', 'Space'. Do not choose a click action on an on-screen keyboard. Capitalize the \
         first character like 'Enter', 'Tab', 'Escape' only for special keys.
  "
    );
    prompt.push_str(&variables_prompt(placeholders));
    prompt
}

/// Tells the model which placeholders stand in for values it never sees.
fn variables_prompt(placeholders: &[String]) -> String {
    if placeholders.is_empty() {
        return String::new();
    }
    let names = placeholders.join(", ");
    format!(
        " The user has provided the following variables to be used in the action: {names} \n
    Note that these are the variable names/keys, and not the actual variable values. \n
    To use the variables in the action, you must respond with the variable name inside the \
         'arguments' array. The variable name must be wrapped in percentage signs (eg, \
         %variableNameHere%) so that it can be replaced with the actual variable value before \
         the action is taken. \n"
    )
}

fn method_list(methods: &[ActMethod]) -> String {
    methods
        .iter()
        .map(|method| method.wire_name())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Stagehand collapses its system prompt's whitespace to single spaces.
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_act_prompt_lists_whirls_methods_and_placeholders() {
        let prompt = act_prompt("sign in", &["%env.PASSWORD%".to_owned()]);
        assert!(prompt.contains("given the following action: sign in."));
        assert!(prompt.contains(
            "such as click, doubleClick, fill, type, press, hover, selectOptionFromDropdown."
        ));
        assert!(
            prompt.contains("the following variables to be used in the action: %env.PASSWORD%")
        );
        assert!(
            !prompt.contains("scroll"),
            "no scroll rules until Whirl has scroll"
        );
        assert!(!prompt.contains("middle"), "no right or middle clicks yet");
    }

    #[test]
    fn step_two_leaves_out_select_and_variables_when_there_are_none() {
        let prompt = step_two_prompt("choose Large", "method: click", &[]);
        assert!(prompt.contains("such as click, doubleClick, fill, type, press, hover."));
        assert!(!prompt.contains("variables"));
    }

    #[test]
    fn the_system_prompt_is_one_line_and_explains_refs() {
        let prompt = system_prompt();
        assert!(!prompt.contains('\n'));
        assert!(prompt.contains("return elementId \"e12\""));
    }
}
