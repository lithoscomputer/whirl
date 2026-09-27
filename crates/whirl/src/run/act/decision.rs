//! The model's answer (SPEC 7.4): Stagehand's act schema on the wire, and
//! the checked decision Whirl acts on.

use std::ops::RangeInclusive;

use serde::Deserialize;
use serde_json::{Value as Json, json};

use crate::lang::ast::MouseButton;
use crate::run::act::instruction::{Instruction, UnboundPlaceholder, same_text};
use crate::run::act::snapshot::{PageSnapshot, Target, quote};
use crate::run::shim::StepCommand;

/// The raw structured answer: Stagehand's `ActInferenceSchema`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ActInference {
    action:   Option<InferredAction>,
    two_step: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InferredAction {
    element_id:  String,
    description: String,
    method:      ActMethod,
    arguments:   Vec<String>,
}

impl ActInference {
    /// A one-step answer that another planner chose, in the model's
    /// terms, so it goes through [`PageSnapshot::decide`] like any answer.
    pub(crate) fn chosen(
        element_id: String,
        description: String,
        method: ActMethod,
        arguments: Vec<String>,
    ) -> Self {
        Self {
            action:   Some(InferredAction {
                element_id,
                description,
                method,
                arguments,
            }),
            two_step: false,
        }
    }

    /// The same answer, marked as the first of two steps.
    pub(crate) fn first_of_two(mut self) -> Self {
        self.two_step = true;
        self
    }
}

/// The methods the model may choose. The names are Stagehand's, so its
/// prompts carry over; each maps to one Whirl verb.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ActMethod {
    Click,
    DoubleClick,
    Fill,
    Type,
    Press,
    Hover,
    SelectOptionFromDropdown,
}

impl ActMethod {
    pub(crate) const ALL: &'static [Self] = &[
        Self::Click,
        Self::DoubleClick,
        Self::Fill,
        Self::Type,
        Self::Press,
        Self::Hover,
        Self::SelectOptionFromDropdown,
    ];

    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            Self::Click => "click",
            Self::DoubleClick => "doubleClick",
            Self::Fill => "fill",
            Self::Type => "type",
            Self::Press => "press",
            Self::Hover => "hover",
            Self::SelectOptionFromDropdown => "selectOptionFromDropdown",
        }
    }

    /// How many arguments the method takes. `click` takes an optional mouse
    /// button.
    fn arity(self) -> RangeInclusive<usize> {
        match self {
            Self::Click => 0..=1,
            Self::DoubleClick | Self::Hover => 0..=0,
            Self::Fill | Self::Type | Self::Press | Self::SelectOptionFromDropdown => 1..=1,
        }
    }
}

/// The JSON schema of [`ActInference`], sent with every model call.
pub(crate) fn inference_schema() -> Json {
    let methods: Vec<&str> = ActMethod::ALL
        .iter()
        .map(|method| method.wire_name())
        .collect();
    json!({
        "type": "object",
        "properties": {
            "action": {
                "anyOf": [
                    {
                        "type": "object",
                        "properties": {
                            "elementId": {
                                "type": "string",
                                "description": "The ref of the element, copied from the accessibility tree without brackets, such as e12."
                            },
                            "description": {
                                "type": "string",
                                "description": "A description of the element and its purpose."
                            },
                            "method": {
                                "type": "string",
                                "enum": methods,
                                "description": "The supported browser interaction method to execute."
                            },
                            "arguments": {
                                "type": "array",
                                "items": {"type": "string"},
                                "description": "The arguments to pass to the selected interaction method."
                            }
                        },
                        "required": ["elementId", "description", "method", "arguments"],
                        "additionalProperties": false
                    },
                    {"type": "null"}
                ],
                "description": "The element to act on, or null when no matching element exists."
            },
            "twoStep": {
                "type": "boolean",
                "description": "Whether the selected interaction requires a second action to finish the request."
            }
        },
        "required": ["action", "twoStep"],
        "additionalProperties": false
    })
}

/// What happens after the chosen action runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FollowUp {
    Done,
    /// A two-step action: plan again on a fresh snapshot.
    Replan,
}

/// The checked answer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ActDecision {
    Perform {
        action:      PlannedAction,
        description: String,
        then:        FollowUp,
    },
    NoMatch,
}

/// Text the model wrote for an action argument. It may hold placeholders,
/// which the decision checked are all bound.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ArgText(String);

/// One element action the model chose: a Whirl verb aimed at a snapshot
/// element.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PlannedAction {
    Click { target: Target, button: MouseButton },
    Dblclick(Target),
    Hover(Target),
    Fill { target: Target, text: ArgText },
    Type { target: Target, text: ArgText },
    Press { target: Target, key: ArgText },
    Select { target: Target, option: ArgText },
}

impl PlannedAction {
    fn new(
        method: ActMethod,
        target: Target,
        argument: Option<ArgText>,
    ) -> Result<Self, DecisionError> {
        let button = match (method, &argument) {
            (ActMethod::Click, Some(ArgText(name))) => {
                MouseButton::from_name(name).ok_or_else(|| DecisionError::Button {
                    given: name.clone(),
                })?
            }
            _ => MouseButton::Left,
        };
        let argument = || argument.expect("the arity check guarantees one argument");
        Ok(match method {
            ActMethod::Click => Self::Click { target, button },
            ActMethod::DoubleClick => Self::Dblclick(target),
            ActMethod::Hover => Self::Hover(target),
            ActMethod::Fill => Self::Fill {
                target,
                text: argument(),
            },
            ActMethod::Type => Self::Type {
                target,
                text: argument(),
            },
            ActMethod::Press => Self::Press {
                target,
                key: argument(),
            },
            ActMethod::SelectOptionFromDropdown => Self::Select {
                target,
                option: argument(),
            },
        })
    }

    fn parts(&self) -> (&'static str, &Target, Option<&ArgText>) {
        match self {
            Self::Click { target, button } => (button.keyword(), target, None),
            Self::Dblclick(target) => ("DBLCLICK", target, None),
            Self::Hover(target) => ("HOVER", target, None),
            Self::Fill { target, text } => ("FILL", target, Some(text)),
            Self::Type { target, text } => ("TYPE", target, Some(text)),
            Self::Press { target, key } => ("PRESS", target, Some(key)),
            Self::Select { target, option } => ("SELECT", target, Some(option)),
        }
    }

    /// The shim command for this action, with placeholders filled in.
    pub(crate) fn command(&self, instruction: &Instruction) -> StepCommand {
        let fill = |text: &ArgText| {
            instruction
                .bindings()
                .fill(&text.0)
                .expect("the decision checked every placeholder is bound")
        };
        match self {
            Self::Click { target, button } => StepCommand::Click {
                locator: target.locator_wire(),
                button:  button.name().to_owned(),
            },
            Self::Dblclick(target) => StepCommand::Dblclick {
                locator: target.locator_wire(),
            },
            Self::Hover(target) => StepCommand::Hover {
                locator: target.locator_wire(),
            },
            Self::Fill { target, text } => StepCommand::Fill {
                locator: target.locator_wire(),
                value:   fill(text),
            },
            Self::Type { target, text } => StepCommand::Type {
                locator: target.locator_wire(),
                text:    fill(text),
            },
            Self::Press { target, key } => StepCommand::Press {
                locator: Some(target.locator_wire()),
                key:     fill(key),
            },
            Self::Select { target, option } => StepCommand::SelectOption {
                locator: target.locator_wire(),
                label:   fill(option),
            },
        }
    }

    /// For a fill, the read that checks the field afterwards (SPEC 7.4).
    pub(crate) fn fill_read_back(&self, instruction: &Instruction) -> Option<FillReadBack> {
        let Self::Fill { target, text } = self else {
            return None;
        };
        let expected = instruction
            .bindings()
            .fill(&text.0)
            .expect("the decision checked every placeholder is bound");
        Some(FillReadBack {
            command: StepCommand::Read {
                subject: json!({"type": "element", "locator": target.locator_wire(), "extract": {"type": "value"}}),
            },
            shows_value: expected == text.0,
            expected,
        })
    }

    /// The action as a Whirl line, such as `CLICK role:button "Sign in"`.
    /// Placeholders stay placeholders, so no secret reaches a report.
    pub(crate) fn line(&self) -> String {
        let (verb, target, argument) = self.parts();
        match argument {
            Some(argument) => format!("{verb} {} {}", target.locator_text(), quote(&argument.0)),
            None => format!("{verb} {}", target.locator_text()),
        }
    }

    /// How the step-two prompt describes the first action.
    pub(crate) fn describe_for_model(&self, description: &str) -> String {
        let (method, argument) = match self {
            Self::Click {
                button: MouseButton::Left,
                ..
            } => (ActMethod::Click, ""),
            Self::Click { button, .. } => (ActMethod::Click, button.name()),
            Self::Dblclick(_) => (ActMethod::DoubleClick, ""),
            Self::Hover(_) => (ActMethod::Hover, ""),
            Self::Fill { text, .. } => (ActMethod::Fill, text.0.as_str()),
            Self::Type { text, .. } => (ActMethod::Type, text.0.as_str()),
            Self::Press { key, .. } => (ActMethod::Press, key.0.as_str()),
            Self::Select { option, .. } => (ActMethod::SelectOptionFromDropdown, option.0.as_str()),
        };
        format!(
            "method: {}, description: {description}, arguments: {argument}",
            method.wire_name(),
        )
    }
}

/// The check that a fill left its value in the field (SPEC 7.4).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FillReadBack {
    /// The non-waiting read of the field's value.
    pub(crate) command: StepCommand,
    /// The value the fill typed, with placeholders filled in.
    expected:           String,
    /// False when the value holds a masked value, which no message shows.
    shows_value:        bool,
}

impl FillReadBack {
    /// Whether the field holds the filled value. Case, spaces, and
    /// punctuation do not count, so a field that formats its value, such as
    /// a phone number, still matches.
    pub(crate) fn matches(&self, held: &str) -> bool {
        same_text(held, &self.expected)
    }

    /// Why the line fails when the field holds `held` instead.
    pub(crate) fn mismatch(&self, action: &PlannedAction, held: &str) -> String {
        if self.shows_value {
            format!("after {}, the field holds {}", action.line(), quote(held))
        } else {
            format!(
                "after {}, the field does not hold the filled value",
                action.line()
            )
        }
    }
}

/// Why an answer cannot become an action.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum DecisionError {
    #[error("the answer names element {element_id}, which is not in the page snapshot")]
    UnknownElement { element_id: String },
    #[error(
        "{method} takes {} argument(s), but the answer gave {actual}",
        arity_text(expected)
    )]
    Arguments {
        method:   &'static str,
        expected: RangeInclusive<usize>,
        actual:   usize,
    },
    #[error("click takes the button left, right, or middle, but the answer gave {given:?}")]
    Button { given: String },
    #[error(transparent)]
    Placeholder(#[from] UnboundPlaceholder),
}

fn arity_text(arity: &RangeInclusive<usize>) -> String {
    if arity.start() == arity.end() {
        arity.start().to_string()
    } else {
        format!("{} or {}", arity.start(), arity.end())
    }
}

impl PageSnapshot {
    /// Checks a model answer against this snapshot and the instruction's
    /// placeholders. This is the only way an answer becomes an action.
    pub(crate) fn decide(
        &self,
        inference: ActInference,
        instruction: &Instruction,
    ) -> Result<ActDecision, DecisionError> {
        let Some(action) = inference.action else {
            return Ok(ActDecision::NoMatch);
        };
        let target =
            self.target(&action.element_id)
                .ok_or_else(|| DecisionError::UnknownElement {
                    element_id: action.element_id.clone(),
                })?;
        let expected = action.method.arity();
        if !expected.contains(&action.arguments.len()) {
            return Err(DecisionError::Arguments {
                method: action.method.wire_name(),
                expected,
                actual: action.arguments.len(),
            });
        }
        let argument = action
            .arguments
            .into_iter()
            .next()
            .map(|text| match action.method {
                // Typed text keeps the author's characters; an option must
                // keep the page's.
                ActMethod::Fill | ActMethod::Type => ArgText(instruction.ground(&text).to_owned()),
                _ => ArgText(text),
            });
        if let Some(argument) = &argument {
            instruction.bindings().fill(&argument.0)?;
        }
        Ok(ActDecision::Perform {
            action:      PlannedAction::new(action.method, target, argument)?,
            description: action.description,
            then:        if inference.two_step {
                FollowUp::Replan
            } else {
                FollowUp::Done
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::ast::{Span, Value, ValueSegment};
    use crate::run::vars::VarStore;

    const SNAPSHOT: &str = "- textbox \"Email\" [ref=e4]\n- button \"Sign in\" [ref=e5]\n";

    fn snapshot() -> PageSnapshot {
        PageSnapshot::parse(SNAPSHOT)
    }

    fn instruction(text: &str) -> Instruction {
        let value = Value {
            segments: vec![ValueSegment::Literal(text.to_owned())],
            span:     Span {
                line:   1,
                column: 1,
                len:    1,
            },
            quoted:   true,
        };
        Instruction::try_new(&value, &mut VarStore::new()).expect("literal resolves")
    }

    fn inference(json: Json) -> ActInference {
        serde_json::from_value(json).expect("valid inference JSON")
    }

    #[test]
    fn a_valid_answer_becomes_a_planned_action() {
        let answer = inference(json!({
            "action": {"elementId": "e4", "description": "Email field", "method": "fill", "arguments": ["ada@example.com"]},
            "twoStep": false
        }));
        let decision = snapshot()
            .decide(answer, &instruction("enter the email"))
            .expect("valid");
        let ActDecision::Perform { action, then, .. } = decision else {
            panic!("expected an action, got {decision:?}");
        };
        assert_eq!(then, FollowUp::Done);
        assert_eq!(
            action.line(),
            r#"FILL role:textbox "Email" "ada@example.com""#
        );
        assert_eq!(action.command(&instruction("x")), StepCommand::Fill {
            locator: json!([{"type": "ref", "ref": "e4"}]),
            value:   "ada@example.com".to_owned(),
        });
    }

    fn perform(answer: Json, instruction: &Instruction) -> PlannedAction {
        match snapshot().decide(inference(answer), instruction) {
            Ok(ActDecision::Perform { action, .. }) => action,
            other => panic!("expected an action, got {other:?}"),
        }
    }

    fn fill(argument: &str) -> Json {
        json!({
            "action": {"elementId": "e4", "description": "Email field", "method": "fill", "arguments": [argument]},
            "twoStep": false
        })
    }

    #[test]
    fn a_fill_reads_its_field_back_ignoring_case_spaces_and_punctuation() {
        let instruction = instruction("enter the phone number");
        let action = perform(fill("5551234567"), &instruction);
        let read_back = action
            .fill_read_back(&instruction)
            .expect("a fill reads back");
        assert_eq!(read_back.command, StepCommand::Read {
            subject: json!({"type": "element", "locator": [{"type": "ref", "ref": "e4"}], "extract": {"type": "value"}}),
        });
        assert!(read_back.matches("5551234567"));
        assert!(read_back.matches("(555) 123-4567"));
        assert!(!read_back.matches("55512"));
        assert!(!read_back.matches(""));
        assert_eq!(
            read_back.mismatch(&action, "55512"),
            r#"after FILL role:textbox "Email" "5551234567", the field holds "55512""#
        );
        let click = perform(click_answer("e5"), &instruction);
        assert_eq!(click.fill_read_back(&instruction), None);
    }

    #[test]
    fn a_fill_mismatch_never_shows_a_masked_value() {
        let mut vars = VarStore::new();
        vars.record_secret("hunter2");
        vars.set_input("password", "hunter2");
        let value = Value {
            segments: vec![
                ValueSegment::Literal("type ".to_owned()),
                ValueSegment::Var("password".to_owned()),
            ],
            span:     Span {
                line:   1,
                column: 1,
                len:    1,
            },
            quoted:   true,
        };
        let instruction = Instruction::try_new(&value, &mut vars).expect("resolves");
        let action = perform(fill("%secret1%"), &instruction);
        let read_back = action
            .fill_read_back(&instruction)
            .expect("a fill reads back");
        assert!(read_back.matches("hunter2"));
        assert_eq!(
            read_back.mismatch(&action, "hunt"),
            r#"after FILL role:textbox "Email" "%secret1%", the field does not hold the filled value"#
        );
    }

    fn click_answer(element_id: &str) -> Json {
        json!({
            "action": {"elementId": element_id, "description": "", "method": "click", "arguments": []},
            "twoStep": false
        })
    }

    #[test]
    fn typed_text_copied_from_a_quoted_string_keeps_the_authors_characters() {
        let instruction = instruction("type \"AbC 123\" into the \"Search\" box");
        let action = perform(fill("abc 123"), &instruction);
        assert_eq!(action.line(), r#"FILL role:textbox "Email" "AbC 123""#);
        let select = json!({
            "action": {"elementId": "e4", "description": "", "method": "selectOptionFromDropdown", "arguments": ["abc 123"]},
            "twoStep": false
        });
        assert_eq!(
            perform(select, &instruction).line(),
            r#"SELECT role:textbox "Email" "abc 123""#
        );
    }

    #[test]
    fn a_null_action_is_no_match() {
        let answer = inference(json!({"action": null, "twoStep": false}));
        assert_eq!(
            snapshot().decide(answer, &instruction("x")),
            Ok(ActDecision::NoMatch)
        );
    }

    #[test]
    fn two_step_asks_for_a_replan() {
        let answer = inference(json!({
            "action": {"elementId": "e5", "description": "menu", "method": "click", "arguments": []},
            "twoStep": true
        }));
        let Ok(ActDecision::Perform { then, .. }) = snapshot().decide(answer, &instruction("x"))
        else {
            panic!("expected an action");
        };
        assert_eq!(then, FollowUp::Replan);
    }

    #[test]
    fn an_unknown_element_is_rejected() {
        let answer = inference(json!({
            "action": {"elementId": "e99", "description": "?", "method": "click", "arguments": []},
            "twoStep": false
        }));
        assert_eq!(
            snapshot().decide(answer, &instruction("x")),
            Err(DecisionError::UnknownElement {
                element_id: "e99".to_owned(),
            })
        );
    }

    fn click_with(arguments: &[&str]) -> ActInference {
        inference(json!({
            "action": {"elementId": "e5", "description": "Sign in", "method": "click", "arguments": arguments},
            "twoStep": false
        }))
    }

    #[test]
    fn a_click_argument_names_the_mouse_button() {
        for (arguments, line, button) in [
            (&[][..], r#"CLICK role:button "Sign in""#, "left"),
            (&["left"][..], r#"CLICK role:button "Sign in""#, "left"),
            (
                &["right"][..],
                r#"RIGHTCLICK role:button "Sign in""#,
                "right",
            ),
            (
                &["middle"][..],
                r#"MIDDLECLICK role:button "Sign in""#,
                "middle",
            ),
        ] {
            let Ok(ActDecision::Perform { action, .. }) =
                snapshot().decide(click_with(arguments), &instruction("x"))
            else {
                panic!("expected an action for {arguments:?}");
            };
            assert_eq!(action.line(), line);
            assert_eq!(action.command(&instruction("x")), StepCommand::Click {
                locator: json!([{"type": "ref", "ref": "e5"}]),
                button:  button.to_owned(),
            });
        }
    }

    #[test]
    fn step_two_hears_the_button_of_a_first_click() {
        let Ok(ActDecision::Perform { action, .. }) =
            snapshot().decide(click_with(&["right"]), &instruction("x"))
        else {
            panic!("expected an action");
        };
        assert_eq!(
            action.describe_for_model("the file"),
            "method: click, description: the file, arguments: right"
        );
    }

    #[test]
    fn an_unknown_mouse_button_is_rejected() {
        assert_eq!(
            snapshot().decide(click_with(&["sideways"]), &instruction("x")),
            Err(DecisionError::Button {
                given: "sideways".to_owned(),
            })
        );
    }

    #[test]
    fn arguments_must_fit_the_method() {
        let error = snapshot()
            .decide(click_with(&["right", "twice"]), &instruction("x"))
            .expect_err("two arguments");
        assert_eq!(error, DecisionError::Arguments {
            method:   "click",
            expected: 0..=1,
            actual:   2,
        });
        assert_eq!(
            error.to_string(),
            "click takes 0 or 1 argument(s), but the answer gave 2"
        );
        let answer = inference(json!({
            "action": {"elementId": "e4", "description": "", "method": "fill", "arguments": []},
            "twoStep": false
        }));
        assert!(snapshot().decide(answer, &instruction("x")).is_err());
    }

    #[test]
    fn an_unbound_placeholder_is_rejected() {
        let answer = inference(json!({
            "action": {"elementId": "e4", "description": "", "method": "fill", "arguments": ["%env.PASSWORD%"]},
            "twoStep": false
        }));
        assert!(matches!(
            snapshot().decide(answer, &instruction("x")),
            Err(DecisionError::Placeholder(_))
        ));
    }

    #[test]
    fn methods_outside_whirls_subset_do_not_parse() {
        for method in ["scrollTo", "dragAndDrop", "nextChunk"] {
            let answer = json!({
                "action": {"elementId": "e5", "description": "", "method": method, "arguments": []},
                "twoStep": false
            });
            assert!(
                serde_json::from_value::<ActInference>(answer).is_err(),
                "{method}"
            );
        }
    }

    #[test]
    fn the_schema_lists_every_method() {
        let schema = inference_schema();
        let methods = &schema["properties"]["action"]["anyOf"][0]["properties"]["method"]["enum"];
        assert_eq!(methods.as_array().map(Vec::len), Some(ActMethod::ALL.len()));
    }
}
