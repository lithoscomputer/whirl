//! The model's answer (SPEC 7.4): Stagehand's act schema on the wire, and
//! the checked decision Whirl acts on.

use serde::Deserialize;
use serde_json::{Value as Json, json};

use crate::run::act::instruction::{Instruction, UnboundPlaceholder};
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

    /// How many arguments the method takes.
    fn arity(self) -> usize {
        match self {
            Self::Click | Self::DoubleClick | Self::Hover => 0,
            Self::Fill | Self::Type | Self::Press | Self::SelectOptionFromDropdown => 1,
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
    Click(Target),
    Dblclick(Target),
    Hover(Target),
    Fill { target: Target, text: ArgText },
    Type { target: Target, text: ArgText },
    Press { target: Target, key: ArgText },
    Select { target: Target, option: ArgText },
}

impl PlannedAction {
    fn new(method: ActMethod, target: Target, argument: Option<ArgText>) -> Self {
        let argument = || argument.expect("the arity check guarantees one argument");
        match method {
            ActMethod::Click => Self::Click(target),
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
        }
    }

    fn parts(&self) -> (&'static str, &Target, Option<&ArgText>) {
        match self {
            Self::Click(target) => ("CLICK", target, None),
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
            Self::Click(target) => StepCommand::Click {
                locator: target.locator_wire(),
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
        let (_, _, argument) = self.parts();
        let method = match self {
            Self::Click(_) => ActMethod::Click,
            Self::Dblclick(_) => ActMethod::DoubleClick,
            Self::Hover(_) => ActMethod::Hover,
            Self::Fill { .. } => ActMethod::Fill,
            Self::Type { .. } => ActMethod::Type,
            Self::Press { .. } => ActMethod::Press,
            Self::Select { .. } => ActMethod::SelectOptionFromDropdown,
        };
        format!(
            "method: {}, description: {description}, arguments: {}",
            method.wire_name(),
            argument.map_or("", |argument| argument.0.as_str())
        )
    }
}

/// Why an answer cannot become an action.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum DecisionError {
    #[error("the answer names element {element_id}, which is not in the page snapshot")]
    UnknownElement { element_id: String },
    #[error("{method} takes {expected} argument(s), but the answer gave {actual}")]
    Arguments {
        method:   &'static str,
        expected: usize,
        actual:   usize,
    },
    #[error(transparent)]
    Placeholder(#[from] UnboundPlaceholder),
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
        if action.arguments.len() != expected {
            return Err(DecisionError::Arguments {
                method: action.method.wire_name(),
                expected,
                actual: action.arguments.len(),
            });
        }
        let argument = action.arguments.into_iter().next().map(ArgText);
        if let Some(argument) = &argument {
            instruction.bindings().fill(&argument.0)?;
        }
        Ok(ActDecision::Perform {
            action:      PlannedAction::new(action.method, target, argument),
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

    #[test]
    fn arguments_must_fit_the_method() {
        let answer = inference(json!({
            "action": {"elementId": "e5", "description": "", "method": "click", "arguments": ["right"]},
            "twoStep": false
        }));
        assert_eq!(
            snapshot().decide(answer, &instruction("x")),
            Err(DecisionError::Arguments {
                method:   "click",
                expected: 0,
                actual:   1,
            })
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
