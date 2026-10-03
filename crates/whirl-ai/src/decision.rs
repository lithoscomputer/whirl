//! The model's answer (SPEC 7.4): the act schema on the wire, and the
//! checked decision Whirl acts on.

use std::ops::RangeInclusive;

use serde::Deserialize;
use serde_json::{Value as Json, json};
use whirl_lang::ast::{MouseButton, Percent, ScrollDirection, ScrollMotion};
use whirl_shim::{StepCommand, wire};

use crate::instruction::{Instruction, UnboundPlaceholder, same_text};
use crate::snapshot::{PageSnapshot, Target, quote};

/// The raw structured answer that [`inference_schema`] describes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActInference {
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

/// How one `GOAL` answer goes on (SPEC 7.7).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum GoalStatus {
    Act,
    Done,
    Impossible,
}

/// The raw structured answer that [`goal_schema`] describes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GoalInference {
    pub status: GoalStatus,
    pub reason: String,
    actions:    Vec<InferredAction>,
}

impl GoalInference {
    /// The answer's actions, each as a one-step `ACT` answer, so each goes
    /// through [`PageSnapshot::decide`] like any answer.
    pub fn into_acts(self) -> Vec<ActInference> {
        self.actions
            .into_iter()
            .map(|action| ActInference {
                action:   Some(action),
                two_step: false,
            })
            .collect()
    }
}

/// The methods the model may choose. Each maps to one Whirl verb.
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
    DragAndDrop,
    ScrollIntoView,
    ScrollTo,
    NextChunk,
    PrevChunk,
    ScrollLeft,
    ScrollRight,
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
        Self::DragAndDrop,
        Self::ScrollIntoView,
        Self::ScrollTo,
        Self::NextChunk,
        Self::PrevChunk,
        Self::ScrollLeft,
        Self::ScrollRight,
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
            Self::DragAndDrop => "dragAndDrop",
            Self::ScrollIntoView => "scrollIntoView",
            Self::ScrollTo => "scrollTo",
            Self::NextChunk => "nextChunk",
            Self::PrevChunk => "prevChunk",
            Self::ScrollLeft => "scrollLeft",
            Self::ScrollRight => "scrollRight",
        }
    }

    /// How many arguments the method takes. `click` takes an optional mouse
    /// button.
    fn arity(self) -> RangeInclusive<usize> {
        match self {
            Self::Click => 0..=1,
            Self::DoubleClick
            | Self::Hover
            | Self::ScrollIntoView
            | Self::NextChunk
            | Self::PrevChunk
            | Self::ScrollLeft
            | Self::ScrollRight => 0..=0,
            Self::Fill
            | Self::Type
            | Self::Press
            | Self::SelectOptionFromDropdown
            | Self::DragAndDrop
            | Self::ScrollTo => 1..=1,
        }
    }
}

/// The JSON schema of [`ActInference`], sent with every model call.
pub(crate) fn inference_schema() -> Json {
    json!({
        "type": "object",
        "properties": {
            "action": action_schema("The element to act on, or null when no matching element exists."),
            "twoStep": {
                "type": "boolean",
                "description": "Whether the selected interaction requires a second action to finish the request."
            }
        },
        "required": ["action", "twoStep"],
        "additionalProperties": false
    })
}

/// The schema of one `GOAL` answer (SPEC 7.7): go on with one action, or
/// end the goal.
pub(crate) fn goal_schema() -> Json {
    json!({
        "type": "object",
        "properties": {
            "status": {
                "type": "string",
                "enum": ["act", "done", "impossible"],
                "description": "act to run one more action, done when the goal is complete, or impossible when it cannot be reached."
            },
            "reason": {
                "type": "string",
                "description": "A short reason for the answer."
            },
            "actions": {
                "type": "array",
                "items": action_object_schema(),
                "description": "When status is act, the next action. Give several only when each fills in or chooses a value in a different field of the same form and none of them changes the page; they run in order. Empty unless status is act."
            }
        },
        "required": ["status", "reason", "actions"],
        "additionalProperties": false
    })
}

/// The wire schema of one element action, or null.
fn action_schema(description: &str) -> Json {
    json!({
        "anyOf": [action_object_schema(), {"type": "null"}],
        "description": description
    })
}

/// The wire schema of one element action.
fn action_object_schema() -> Json {
    let methods: Vec<&str> = ActMethod::ALL
        .iter()
        .map(|method| method.wire_name())
        .collect();
    json!({
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
    })
}

/// What happens after the chosen action runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FollowUp {
    Done,
    /// A two-step action: plan again on a fresh snapshot.
    Replan,
}

/// The checked answer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActDecision {
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
pub struct ArgText(String);

/// One element action the model chose: a Whirl verb aimed at a snapshot
/// element.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlannedAction {
    Click {
        target: Target,
        button: MouseButton,
    },
    Dblclick(Target),
    Hover(Target),
    Drag {
        source: Target,
        target: Target,
    },
    ScrollIntoView(Target),
    /// Scrolls the target's scroll box, or the page without a target.
    Scroll {
        target: Option<Target>,
        motion: ScrollMotion,
    },
    Fill {
        target: Target,
        text:   ArgText,
    },
    Type {
        target: Target,
        text:   ArgText,
    },
    Press {
        target: Target,
        key:    ArgText,
    },
    Select {
        target: Target,
        option: ArgText,
    },
}

impl PlannedAction {
    fn new(
        method: ActMethod,
        target: Target,
        argument: Option<ArgText>,
        snapshot: &PageSnapshot,
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
            ActMethod::DragAndDrop => {
                let ArgText(element_id) = argument();
                let drop = snapshot
                    .target(&element_id)
                    .ok_or(DecisionError::UnknownElement { element_id })?;
                if drop == target {
                    return Err(DecisionError::DropOnItself {
                        element_id: target.element_ref().to_owned(),
                    });
                }
                Self::Drag {
                    source: target,
                    target: drop,
                }
            }
            ActMethod::ScrollIntoView => Self::ScrollIntoView(target),
            ActMethod::ScrollTo => {
                let ArgText(given) = argument();
                let percent = Percent::parse(&given).ok_or(DecisionError::Percent { given })?;
                Self::scroll(target, ScrollMotion::To(percent))
            }
            ActMethod::NextChunk => {
                Self::scroll(target, ScrollMotion::Chunk(ScrollDirection::Down))
            }
            ActMethod::PrevChunk => Self::scroll(target, ScrollMotion::Chunk(ScrollDirection::Up)),
            ActMethod::ScrollLeft => {
                Self::scroll(target, ScrollMotion::Chunk(ScrollDirection::Left))
            }
            ActMethod::ScrollRight => {
                Self::scroll(target, ScrollMotion::Chunk(ScrollDirection::Right))
            }
        })
    }

    /// A scroll of the target's box; the page's `<body>` stands for the
    /// page, which scrolls without a locator.
    fn scroll(target: Target, motion: ScrollMotion) -> Self {
        Self::Scroll {
            target: (!target.is_page()).then_some(target),
            motion,
        }
    }

    /// The shim command for this action, with placeholders filled in.
    pub fn command(&self, instruction: &Instruction) -> StepCommand {
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
            Self::Drag { source, target } => StepCommand::Drag {
                locator: source.locator_wire(),
                target:  target.locator_wire(),
            },
            Self::ScrollIntoView(target) => StepCommand::Scroll {
                locator: Some(target.locator_wire()),
                motion:  wire::scroll_motion_wire(None),
            },
            Self::Scroll { target, motion } => StepCommand::Scroll {
                locator: target.as_ref().map(Target::locator_wire),
                motion:  wire::scroll_motion_wire(Some(motion)),
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
    pub fn fill_read_back(&self, instruction: &Instruction) -> Option<FillReadBack> {
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

    /// The action as a Whirl line, such as `CLICK button:"Sign in"`.
    /// Placeholders stay placeholders, so no secret reaches a report.
    pub fn line(&self) -> String {
        let with = |verb: &str, target: &Target, argument: &ArgText| {
            format!("{verb} {} {}", target.locator_text(), quote(&argument.0))
        };
        match self {
            Self::Click { target, button } => {
                format!("{} {}", button.keyword(), target.locator_text())
            }
            Self::Dblclick(target) => format!("DBLCLICK {}", target.locator_text()),
            Self::Hover(target) => format!("HOVER {}", target.locator_text()),
            Self::Drag { source, target } => format!(
                "DRAG {} to {}",
                source.locator_text(),
                target.locator_text()
            ),
            Self::ScrollIntoView(target) => format!("SCROLL {}", target.locator_text()),
            Self::Scroll { target, motion } => {
                let motion = match motion {
                    ScrollMotion::Chunk(direction) => direction.keyword().to_owned(),
                    ScrollMotion::To(percent) => format!("to {percent}"),
                };
                match target {
                    Some(target) => format!("SCROLL {} {motion}", target.locator_text()),
                    None => format!("SCROLL {motion}"),
                }
            }
            Self::Fill { target, text } => with("FILL", target, text),
            Self::Type { target, text } => with("TYPE", target, text),
            Self::Press { target, key } => with("PRESS", target, key),
            Self::Select { target, option } => with("SELECT", target, option),
        }
    }

    /// The elements the action targets, in line order.
    pub fn targets(&self) -> Vec<&Target> {
        match self {
            Self::Click { target, .. }
            | Self::Dblclick(target)
            | Self::Hover(target)
            | Self::ScrollIntoView(target)
            | Self::Fill { target, .. }
            | Self::Type { target, .. }
            | Self::Press { target, .. }
            | Self::Select { target, .. } => vec![target],
            Self::Drag { source, target } => vec![source, target],
            Self::Scroll { target, .. } => target.iter().collect(),
        }
    }

    /// The action as the AI cache writes it (SPEC 12.1): `locators` are
    /// the generated locators of [`Self::targets`], and `value` writes an
    /// argument, or gives `None` when the cache cannot hold it.
    pub fn cache_line(
        &self,
        locators: &[String],
        value: impl Fn(&str) -> Option<String>,
    ) -> Option<String> {
        let first = || locators.first().cloned().unwrap_or_default();
        let with = |verb: &str, argument: &ArgText| -> Option<String> {
            Some(format!("{verb} {} {}", first(), value(&argument.0)?))
        };
        Some(match self {
            Self::Click { button, .. } => format!("{} {}", button.keyword(), first()),
            Self::Dblclick(_) => format!("DBLCLICK {}", first()),
            Self::Hover(_) => format!("HOVER {}", first()),
            Self::Drag { .. } => format!("DRAG {} to {}", first(), locators.get(1)?),
            Self::ScrollIntoView(_) => format!("SCROLL {}", first()),
            Self::Scroll { target, motion } => {
                let motion = match motion {
                    ScrollMotion::Chunk(direction) => direction.keyword().to_owned(),
                    ScrollMotion::To(percent) => format!("to {percent}"),
                };
                match target {
                    Some(_) => format!("SCROLL {} {motion}", first()),
                    None => format!("SCROLL {motion}"),
                }
            }
            Self::Fill { text, .. } => with("FILL", text)?,
            Self::Type { text, .. } => with("TYPE", text)?,
            Self::Press { key, .. } => with("PRESS", key)?,
            Self::Select { option, .. } => with("SELECT", option)?,
        })
    }

    /// How the step-two prompt describes the first action.
    pub fn describe_for_model(&self, description: &str) -> String {
        let (method, argument) = match self {
            Self::Click {
                button: MouseButton::Left,
                ..
            } => (ActMethod::Click, String::new()),
            Self::Click { button, .. } => (ActMethod::Click, button.name().to_owned()),
            Self::Dblclick(_) => (ActMethod::DoubleClick, String::new()),
            Self::Hover(_) => (ActMethod::Hover, String::new()),
            Self::Drag { target, .. } => (ActMethod::DragAndDrop, target.element_ref().to_owned()),
            Self::ScrollIntoView(_) => (ActMethod::ScrollIntoView, String::new()),
            Self::Scroll { motion, .. } => match motion {
                ScrollMotion::To(percent) => (ActMethod::ScrollTo, percent.to_string()),
                ScrollMotion::Chunk(ScrollDirection::Down) => (ActMethod::NextChunk, String::new()),
                ScrollMotion::Chunk(ScrollDirection::Up) => (ActMethod::PrevChunk, String::new()),
                ScrollMotion::Chunk(ScrollDirection::Left) => {
                    (ActMethod::ScrollLeft, String::new())
                }
                ScrollMotion::Chunk(ScrollDirection::Right) => {
                    (ActMethod::ScrollRight, String::new())
                }
            },
            Self::Fill { text, .. } => (ActMethod::Fill, text.0.clone()),
            Self::Type { text, .. } => (ActMethod::Type, text.0.clone()),
            Self::Press { key, .. } => (ActMethod::Press, key.0.clone()),
            Self::Select { option, .. } => (ActMethod::SelectOptionFromDropdown, option.0.clone()),
        };
        format!(
            "method: {}, description: {description}, arguments: {argument}",
            method.wire_name(),
        )
    }
}

/// The check that a fill left its value in the field (SPEC 7.4).
#[derive(Clone, Debug, PartialEq)]
pub struct FillReadBack {
    /// The non-waiting read of the field's value.
    pub command: StepCommand,
    /// The value the fill typed, with placeholders filled in.
    expected:    String,
    /// False when the value holds a masked value, which no message shows.
    shows_value: bool,
}

impl FillReadBack {
    /// Whether the field holds the filled value. Case, spaces, and
    /// punctuation do not count, so a field that formats its value, such as
    /// a phone number, still matches.
    pub fn matches(&self, held: &str) -> bool {
        same_text(held, &self.expected)
    }

    /// Why the line fails when the field holds `held` instead.
    pub fn mismatch(&self, action: &PlannedAction, held: &str) -> String {
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
pub enum DecisionError {
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
    #[error("the answer drags element {element_id} onto itself")]
    DropOnItself { element_id: String },
    #[error("scrollTo takes a percent from 0% to 100%, but the answer gave {given:?}")]
    Percent { given: String },
    #[error(transparent)]
    Placeholder(#[from] UnboundPlaceholder),
}

/// The arguments a method can use (SPEC 7.4). A method that takes none
/// drops what the answer gave, and a click keeps only an argument that
/// names a mouse button, so an empty string or the element's text is a
/// left click. Other methods keep every argument and are checked strictly.
fn usable_arguments(method: ActMethod, arguments: Vec<String>) -> Vec<String> {
    match method {
        ActMethod::Click => arguments
            .into_iter()
            .find(|argument| MouseButton::from_name(argument).is_some())
            .into_iter()
            .collect(),
        _ if *method.arity().end() == 0 => Vec::new(),
        _ => arguments,
    }
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
    pub fn decide(
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
        let arguments = usable_arguments(action.method, action.arguments);
        let action = InferredAction {
            arguments,
            ..action
        };
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
            action:      PlannedAction::new(action.method, target, argument, self)?,
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
    use whirl_lang::ast::{Span, Value, ValueSegment};

    use super::*;
    use crate::instruction::testing::TestVars;

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
        Instruction::try_new(&value, &mut TestVars::new()).expect("literal resolves")
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
        assert_eq!(action.line(), r#"FILL textbox:"Email" "ada@example.com""#);
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
            r#"after FILL textbox:"Email" "5551234567", the field holds "55512""#
        );
        let click = perform(click_answer("e5"), &instruction);
        assert_eq!(click.fill_read_back(&instruction), None);
    }

    #[test]
    fn a_fill_mismatch_never_shows_a_masked_value() {
        let mut vars = TestVars::new();
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
            r#"after FILL textbox:"Email" "%secret1%", the field does not hold the filled value"#
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
        assert_eq!(action.line(), r#"FILL textbox:"Email" "AbC 123""#);
        let select = json!({
            "action": {"elementId": "e4", "description": "", "method": "selectOptionFromDropdown", "arguments": ["abc 123"]},
            "twoStep": false
        });
        assert_eq!(
            perform(select, &instruction).line(),
            r#"SELECT textbox:"Email" "abc 123""#
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
            (&[][..], r#"CLICK button:"Sign in""#, "left"),
            (&["left"][..], r#"CLICK button:"Sign in""#, "left"),
            (&["right"][..], r#"RIGHTCLICK button:"Sign in""#, "right"),
            (&["middle"][..], r#"MIDDLECLICK button:"Sign in""#, "middle"),
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

    fn drag_to(target: &str) -> ActInference {
        inference(json!({
            "action": {"elementId": "e4", "description": "the email", "method": "dragAndDrop", "arguments": [target]},
            "twoStep": false
        }))
    }

    #[test]
    fn a_drag_names_its_drop_target_by_ref() {
        let Ok(ActDecision::Perform { action, .. }) =
            snapshot().decide(drag_to("e5"), &instruction("x"))
        else {
            panic!("expected an action");
        };
        assert_eq!(action.line(), r#"DRAG textbox:"Email" to button:"Sign in""#);
        assert_eq!(action.command(&instruction("x")), StepCommand::Drag {
            locator: json!([{"type": "ref", "ref": "e4"}]),
            target:  json!([{"type": "ref", "ref": "e5"}]),
        });
        assert_eq!(
            action.describe_for_model("the email"),
            "method: dragAndDrop, description: the email, arguments: e5"
        );
    }

    #[test]
    fn a_drop_target_must_be_another_element_in_the_snapshot() {
        assert_eq!(
            snapshot().decide(drag_to("e99"), &instruction("x")),
            Err(DecisionError::UnknownElement {
                element_id: "e99".to_owned(),
            })
        );
        assert_eq!(
            snapshot().decide(drag_to("e4"), &instruction("x")),
            Err(DecisionError::DropOnItself {
                element_id: "e4".to_owned(),
            })
        );
    }

    fn scroll(method: &str, arguments: &[&str]) -> ActInference {
        inference(json!({
            "action": {"elementId": "e2", "description": "the feed", "method": method, "arguments": arguments},
            "twoStep": false
        }))
    }

    /// A whole-page snapshot, where `e1` is the page's `<body>`.
    fn page_snapshot() -> PageSnapshot {
        PageSnapshot::parse("- generic [ref=e1]:\n  - list \"Feed\" [ref=e2]\n").of_page()
    }

    fn scroll_action(answer: ActInference) -> PlannedAction {
        match page_snapshot().decide(answer, &instruction("x")) {
            Ok(ActDecision::Perform { action, .. }) => action,
            other => panic!("expected an action, got {other:?}"),
        }
    }

    #[test]
    fn scroll_methods_run_as_scroll_lines() {
        for (method, arguments, line, motion) in [
            (
                "scrollIntoView",
                &[][..],
                r#"SCROLL list:"Feed""#,
                json!({"type": "intoView"}),
            ),
            (
                "scrollTo",
                &["50%"][..],
                r#"SCROLL list:"Feed" to 50%"#,
                json!({"type": "position", "percent": 50.0}),
            ),
            (
                "nextChunk",
                &[][..],
                r#"SCROLL list:"Feed" down"#,
                json!({"type": "chunk", "direction": "down"}),
            ),
            (
                "prevChunk",
                &[][..],
                r#"SCROLL list:"Feed" up"#,
                json!({"type": "chunk", "direction": "up"}),
            ),
            (
                "scrollLeft",
                &[][..],
                r#"SCROLL list:"Feed" left"#,
                json!({"type": "chunk", "direction": "left"}),
            ),
            (
                "scrollRight",
                &[][..],
                r#"SCROLL list:"Feed" right"#,
                json!({"type": "chunk", "direction": "right"}),
            ),
        ] {
            let action = scroll_action(scroll(method, arguments));
            assert_eq!(action.line(), line, "{method}");
            assert_eq!(
                action.command(&instruction("x")),
                StepCommand::Scroll {
                    locator: Some(json!([{"type": "ref", "ref": "e2"}])),
                    motion,
                },
                "{method}"
            );
        }
        assert_eq!(
            scroll_action(scroll("scrollTo", &["75%"])).describe_for_model("the feed"),
            "method: scrollTo, description: the feed, arguments: 75%"
        );
    }

    #[test]
    fn the_pages_body_scrolls_the_page() {
        let answer = inference(json!({
            "action": {"elementId": "e1", "description": "the page", "method": "nextChunk", "arguments": []},
            "twoStep": false
        }));
        let action = scroll_action(answer);
        assert_eq!(action.line(), "SCROLL down");
        assert_eq!(action.command(&instruction("x")), StepCommand::Scroll {
            locator: None,
            motion:  json!({"type": "chunk", "direction": "down"}),
        });
    }

    #[test]
    fn scroll_to_needs_a_percent_from_0_to_100() {
        for given in ["150%", "0.5", "halfway"] {
            assert_eq!(
                page_snapshot().decide(scroll("scrollTo", &[given]), &instruction("x")),
                Err(DecisionError::Percent {
                    given: given.to_owned(),
                }),
                "{given}"
            );
        }
    }

    #[test]
    fn a_click_ignores_an_argument_that_names_no_button() {
        for arguments in [&["sideways"][..], &[""], &["Blue"], &["right", "twice"]] {
            let answer = json!({
                "action": {"elementId": "e5", "description": "", "method": "click", "arguments": arguments},
                "twoStep": false
            });
            let action = perform(answer, &instruction("x"));
            let expected = if arguments.contains(&"right") {
                "RIGHTCLICK button:\"Sign in\""
            } else {
                "CLICK button:\"Sign in\""
            };
            assert_eq!(action.line(), expected, "{arguments:?}");
        }
        let answer = json!({
            "action": {"elementId": "e5", "description": "", "method": "hover", "arguments": ["now"]},
            "twoStep": false
        });
        assert_eq!(
            perform(answer, &instruction("x")).line(),
            "HOVER button:\"Sign in\""
        );
    }

    #[test]
    fn arguments_must_fit_the_method() {
        let answer = inference(json!({
            "action": {"elementId": "e4", "description": "", "method": "fill", "arguments": ["a", "b"]},
            "twoStep": false
        }));
        let error = snapshot()
            .decide(answer, &instruction("x"))
            .expect_err("two arguments");
        assert_eq!(error, DecisionError::Arguments {
            method:   "fill",
            expected: 1..=1,
            actual:   2,
        });
        assert_eq!(
            error.to_string(),
            "fill takes 1 argument(s), but the answer gave 2"
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
        for method in ["scroll", "mouse.wheel"] {
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
