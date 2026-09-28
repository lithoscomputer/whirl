//! The `ACT` prompts (SPEC 7.4), ported from Stagehand's
//! `packages/extension/prompt.ts` (`buildActSystemPrompt`,
//! `buildActPrompt`, `buildStepTwoPrompt`, and `buildObserveUserMessage`),
//! the `ai:` target prompt (SPEC 6.3), ported from its
//! `buildObserveSystemPrompt`, the `EXTRACT` prompts (SPEC 7.6), ported from
//! its `buildExtractSystemPrompt` and `buildExtractUserPrompt`, the `JUDGE`
//! prompt (SPEC 9.8), adapted from the evidence rules of its verifier
//! (`packages/core/lib/v3/verifier/prompts/fusedOutcome.ts`) and the YES/NO
//! evaluator of `packages/core/lib/v3LegacyEvaluator.ts`, the `GOAL`
//! prompt (SPEC 7.7), adapted from its `buildOperatorSystemPrompt`, and the
//! text-argument prompt of its Jev path (browserbase/stagehand#2953).
//!
//! Whirl's changes: element IDs are Playwright AI-snapshot refs rather than
//! frame-and-node IDs, the method list is Whirl's, and rules are added for
//! dragging, scrolling the whole page, scrolling into view, and scrolling
//! sideways.
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

/// The system prompt of an `ai:` target call (SPEC 6.3). Whirl asks for
/// every match and applies its own strictness rule, so the prompt forbids
/// guessing and choosing among matches.
pub(crate) fn target_system_prompt() -> String {
    collapse_whitespace(
        "You are helping the user test a web page by finding the elements that a description          names.

         You will be given:
         1. a description of the elements to find
         2. a hierarchical accessibility tree showing the semantic structure of the page. The          tree is a hybrid of the DOM and the accessibility tree.

         Return an array of EVERY element that matches the description, otherwise return an          empty array. Do not choose one element when several match: return them all. Do not          fabricate or guess an element; if you are not sure an element matches, leave it out.          Describe each element you return in a few words.

         Each element in the accessibility tree has a ref in square brackets, like [ref=e12] or          [ref=f1e3]. Copy the ref value exactly into elementId, without the brackets or the          `ref=` prefix. For example, if the tree shows [ref=e12], return elementId \"e12\".",
    )
}

/// The system prompt of an `EXTRACT` call (SPEC 7.6).
pub(crate) fn extract_system_prompt() -> String {
    collapse_whitespace(
        "You are extracting content on behalf of a user. If a user asks you to extract a \
         'list' of information, or 'all' information, YOU MUST EXTRACT ALL OF THE INFORMATION \
         THAT THE USER REQUESTS.

         You will be given:
         1. An instruction
         2. A hierarchical accessibility tree of the page to extract from.

         Print the exact text from the tree with all symbols, characters, and endlines as is. \
         Print null if the page does not show the information.

         If a user is attempting to extract links or URLs, you MUST respond with ONLY the refs \
         of the link elements, such as e12, copied exactly from the [ref=...] marks. Do not \
         attempt to extract links directly from the text.",
    )
}

/// The system prompt of a `JUDGE` call (SPEC 9.8).
pub(crate) fn judge_system_prompt() -> String {
    collapse_whitespace(
        "You are an expert evaluator of a web page. You decide whether a claim about the page \
         holds, and you answer yes, no, or unsure with a concise reason.

         You will be given:
         1. a claim about the page
         2. a hierarchical accessibility tree of the page, or of one element of it
         3. a screenshot of the same part of the page

         Judge only from the accessibility tree and the screenshot. Do not use outside or \
         current-world knowledge to override what they show. Do not assume anything that they \
         do not show: an unseen redirect, a hidden element, or a value that is not on the page. \
         Ignore small differences that do not change what the claim means, such as \
         capitalization, spacing, or formatting. Answer yes when the evidence shows the claim \
         holds, and no when the evidence shows it does not. Answer unsure when the evidence is \
         missing, cut off, or ambiguous.",
    )
}

/// The system prompt of every `GOAL` call (SPEC 7.7). Whirl answers with
/// a schema instead of tools, and it has no navigation, so the prompt
/// names the three answers and leaves out the tools.
pub(crate) fn goal_system_prompt() -> String {
    collapse_whitespace(
        "You are a general-purpose agent whose job is to accomplish the user's goal across \
         multiple model calls by running actions on the page.

         You will be given a goal, a list of steps that have been taken so far, and a \
         hierarchical accessibility tree of the page as it is now. Your job is to determine if \
         either the user's goal has been completed or if there are still steps that need to be \
         taken.

         Answer with status act and the next action when steps remain, with status done when \
         the goal is complete, or with status impossible when the goal cannot be achieved on \
         this page. Give a short reason. Leave actions empty unless the status is act.

         Important guidelines:
         1. Break down complex actions into individual atomic steps.
         2. Each action is a single step, such as a single click on a specific element, \
         typing into a single input field, or selecting a single option.
         3. To fill in a form, give one action for each field that needs a value, in order, \
         in the same answer. A click, or any other step that changes the page, is an answer \
         of its own.
         4. If a step failed, look at the page as it is now and try another way.
         5. You cannot go to a URL, go back, or reload the page.
         6. Only answer done when the goal is genuinely complete, and impossible when it is \
         genuinely impossible to achieve.

         Each element in the accessibility tree has a ref in square brackets, like [ref=e12] or \
         [ref=f1e3]. Copy the ref value exactly into elementId, without the brackets or the \
         `ref=` prefix. For example, if the tree shows [ref=e12], return elementId \"e12\".",
    )
}

/// The user message of a `GOAL` call: the goal, how to answer an action,
/// the steps so far, and the snapshot.
pub(crate) fn goal_message(
    goal: &str,
    placeholders: &[String],
    steps: &[String],
    snapshot: &str,
) -> String {
    let methods = method_list(ActMethod::ALL);
    let mut message = format!(
        "Goal: {goal}

  For an action, provide the element and a method such as {methods}. Remember that to users, \
         buttons and links look the same in most cases.
  When choosing non-left click actions, provide right or middle as the argument
  {DRAG_RULE}
  {SCROLL_RULES}
  If the step is a key press, e.g., 'press enter', 'press a', 'press space', etc., always \
         choose the press method with the appropriate key as argument — e.g. 'a', 'Enter', \
         'Space'. Capitalize the first character like 'Enter', 'Tab', 'Escape' only for special \
         keys.
  To choose an option of a 'select' element, choose the selectOptionFromDropdown method with \
         the exact text of the option. To choose from any other dropdown, click it to open it \
         first, and choose the option in the next step.
"
    );
    message.push_str(&variables_prompt(placeholders));
    let steps = if steps.is_empty() {
        "none".to_owned()
    } else {
        steps
            .iter()
            .enumerate()
            .map(|(index, step)| format!("{}. {step}", index + 1))
            .collect::<Vec<_>>()
            .join("\n")
    };
    message + "\nSteps taken so far:\n" + &steps + "\nAccessibility Tree: \n" + snapshot + "\n"
}

/// The text of a `JUDGE` call's user message: the claim, its
/// placeholders, and the snapshot. The screenshot follows it.
pub(crate) fn judge_message(claim: &str, placeholders: &[String], snapshot: &str) -> String {
    let placeholders = if placeholders.is_empty() {
        String::new()
    } else {
        format!(
            "\nThe claim uses placeholders for hidden values: {}.",
            placeholders.join(", ")
        )
    };
    format!("Claim: {claim}{placeholders}\nAccessibility Tree: \n{snapshot}\nScreenshot:")
}

/// The user message of an `EXTRACT` call: the instruction, its
/// placeholders, and the snapshot.
pub(crate) fn extract_message(
    instruction: &str,
    placeholders: &[String],
    snapshot: &str,
) -> String {
    let placeholders = if placeholders.is_empty() {
        String::new()
    } else {
        format!(
            "\nThe instruction uses placeholders for hidden values: {}.",
            placeholders.join(", ")
        )
    };
    format!("Instruction: {instruction}{placeholders}\nDOM: {snapshot}\n")
}

/// The user message of an `ai:` target call: the description, its
/// placeholders, and the snapshot.
pub(crate) fn target_message(description: &str, placeholders: &[String], snapshot: &str) -> String {
    let placeholders = if placeholders.is_empty() {
        String::new()
    } else {
        format!(
            "\nThe description uses placeholders for hidden values: {}.",
            placeholders.join(", ")
        )
    };
    format!("description: {description}{placeholders}\nAccessibility Tree: \n{snapshot}\n")
}

/// The system prompt of the call that reads the text to type from an
/// instruction, for the `--jev` planner.
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
    When choosing non-left click actions, provide right or middle as the argument
    {DRAG_RULE}
    If the action is completely unrelated to a potential action to be taken on the page, or \
         no matching element exists, set `action` to null. Do not fabricate or guess an element.
    ONLY return one action. If multiple actions are relevant, return the most relevant one.
    {SCROLL_RULES}
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
  {DRAG_RULE}
  {SCROLL_RULES}
  If the action implies a key press, e.g., 'press enter', 'press a', 'press space', etc., \
         always choose the press method with the appropriate key as argument — e.g. 'a', \
         'Enter', 'Space'. Do not choose a click action on an on-screen keyboard. Capitalize the \
         first character like 'Enter', 'Tab', 'Escape' only for special keys.
  "
    );
    prompt.push_str(&variables_prompt(placeholders));
    prompt
}

/// How the model answers a drag, which names two elements.
const DRAG_RULE: &str = "To drag an element onto another element, choose the dragAndDrop method on \
                         the element to drag, and give the ref of the element to drop it on as \
                         the argument, such as e12.";

/// How the model answers a scroll.
const SCROLL_RULES: &str = "If the user is asking to scroll to a position on the page, e.g., \
                            'halfway' or 0.75, etc, you must return the argument formatted as \
                            the correct percentage, e.g., '50%' or '75%', etc.
    If the user is asking to scroll to the next chunk/previous chunk, choose the \
                            nextChunk/prevChunk method. No arguments are required here.
    To scroll the whole page, choose the root element of the tree. To scroll a list or \
                            panel, choose it or an element inside it. To bring an element into \
                            view, choose the scrollIntoView method on it. To scroll sideways by \
                            one chunk, choose the scrollLeft or scrollRight method.";

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

/// The system prompts are sent with their whitespace collapsed to single
/// spaces.
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
            "such as click, doubleClick, fill, type, press, hover, selectOptionFromDropdown, \
             dragAndDrop, scrollIntoView, scrollTo, nextChunk, prevChunk, scrollLeft, scrollRight."
        ));
        assert!(prompt.contains(DRAG_RULE));
        assert!(prompt.contains(SCROLL_RULES));
        assert!(
            prompt.contains("the following variables to be used in the action: %env.PASSWORD%")
        );
        assert!(prompt.contains(
            "When choosing non-left click actions, provide right or middle as the argument"
        ));
    }

    #[test]
    fn step_two_leaves_out_select_and_variables_when_there_are_none() {
        let prompt = step_two_prompt("choose Large", "method: click", &[]);
        assert!(prompt.contains(
            "such as click, doubleClick, fill, type, press, hover, dragAndDrop, scrollIntoView, \
             scrollTo, nextChunk, prevChunk, scrollLeft, scrollRight."
        ));
        assert!(prompt.contains(DRAG_RULE));
        assert!(prompt.contains(SCROLL_RULES));
        assert!(!prompt.contains("variables"));
    }

    #[test]
    fn the_goal_message_lists_the_steps_so_far_and_the_rules() {
        let message = goal_message(
            "sign in as %env.USER%",
            &["%env.USER%".to_owned()],
            &[
                "FILL role:textbox Email \"%env.USER%\"".to_owned(),
                "CLICK role:button Go (failed: timeout)".to_owned(),
            ],
            "- button \"Sign in\" [ref=e2]",
        );
        assert!(message.starts_with("Goal: sign in as %env.USER%\n"));
        assert!(message.contains(DRAG_RULE));
        assert!(message.contains("the following variables to be used in the action: %env.USER%"));
        assert!(message.contains(
            "Steps taken so far:\n1. FILL role:textbox Email \"%env.USER%\"\n2. CLICK role:button Go (failed: timeout)\n"
        ));
        assert!(message.ends_with("Accessibility Tree: \n- button \"Sign in\" [ref=e2]\n"));
        assert!(goal_message("x", &[], &[], "").contains("Steps taken so far:\nnone\n"));
        assert!(!goal_system_prompt().contains('\n'));
    }

    #[test]
    fn the_system_prompt_is_one_line_and_explains_refs() {
        let prompt = system_prompt();
        assert!(!prompt.contains('\n'));
        assert!(prompt.contains("return elementId \"e12\""));
    }
}
