//! Checks and captures with a subject (SPEC 9, 10; ADR
//! `evaluate-checks-in-rust`): resolve the line into the check engine's
//! types, read the subject through the shim, and evaluate in Rust. Page
//! reads retry on Playwright's schedule until the check passes or the
//! step budget ends; response reads happen once.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use serde_json::Value as Json;
use tokio::time::sleep;
use whirl_types::{FilterKind, Number, Value};

use super::act_step::{ActBudget, ActLine};
use super::ai_step::{self, AiSpend, AiTarget, Found};
use super::{
    BuildError, EntryState, FlowExec, StepBudget, StepEnd, StepNode, entry_timeout_error,
    step_error,
};
use crate::check::{
    self, Charset, Check, DateFormat, Expected, Filter, JsonQuery, Markup, Missing, Pattern,
    PatternFlags, Predicate, Read, ReadContext, XpathQuery,
};
use crate::lang::ast::{
    self, Extractor, FilterArg, FilterSpec, Operand, PredicateSpec, RequestField, ResponseField,
    Subject,
};
use crate::report::model::{CaptureValue, StepError};
use crate::run::act::PlanUsage;
use crate::run::shim::{
    MissingReason, ReadResult, RequestReadResult, ResponseReadResult, ShimClient, StepCommand,
    StepOutcome, StepRequest, wire,
};
use crate::run::vars::{MASK, VarStore};

/// Playwright's poll schedule for retried checks (ADR
/// `evaluate-checks-in-rust` §1.3): 100, 250, 500, then 1000 ms.
const POLL_INTERVALS_MS: [u64; 4] = [100, 250, 500, 1000];

/// A retry needs at least this much budget. A read with less time can
/// time out inside the shim and hide the last real result.
const MIN_RETRY_MS: u64 = 50;

/// Shim error kinds that mean "not passing yet" for a page read (SPEC
/// 9.7): page churn mid-read, and `eval` scripts that throw or return a
/// value outside the result contract.
const RETRYABLE_KINDS: [&str; 3] = ["read", "eval", "eval-result"];

/// A response read once and kept for the flow: responses never change.
#[derive(Clone, Debug)]
pub(super) struct ResponseData {
    status:              u16,
    url:                 String,
    headers:             Vec<(String, String)>,
    /// `None` until a check needs the body; then the bytes, or why they
    /// could not be read.
    body:                Option<Result<Vec<u8>, String>>,
    /// The browser may have returned a text body decoded (protocol 4.5).
    body_may_be_decoded: bool,
}

/// Where a line reads its value.
enum Source {
    /// A page subject: the wire read subject, and the attribute name when
    /// the subject reads one.
    Page {
        subject: Json,
        attr:    Option<String>,
        /// False for a capture that never waits: `count` and `eval`.
        retry:   bool,
        /// The subject's `ai:` target, which resolves while the line reads
        /// (SPEC 6.3).
        ai:      Option<Box<AiRead>>,
    },
    Response {
        name:  String,
        field: ResponseRead,
    },
    /// The request that a `RESPONSE` name selected (SPEC 9.2).
    Request {
        name:  String,
        field: RequestRead,
    },
    /// The value an `EXTRACT` line read (SPEC 7.6).
    Extract {
        name: String,
    },
}

/// A page subject whose locator ends in an `ai:` target.
#[derive(Debug)]
pub(super) struct AiRead {
    /// The authored subject.
    subject:    Subject,
    /// True for `not exists`: no element passes (SPEC 6.3).
    absence_ok: bool,
    /// The target, made at the first read, which knows the line.
    target:     Option<AiTarget>,
    usage:      PlanUsage,
    /// True when the subject needs its element found (again).
    stale:      bool,
}

/// The part of a request a line reads.
enum RequestRead {
    Method,
    Url,
    Header(String),
    Body,
    Bytes,
}

/// A request read once and kept for the flow.
#[derive(Clone, Debug)]
pub(super) struct RequestData {
    method:  String,
    url:     String,
    headers: Vec<(String, String)>,
    /// The body bytes, or why they could not be read.
    body:    Result<Vec<u8>, String>,
}

/// The part of a response a line reads.
enum ResponseRead {
    Status,
    Header(String),
    Location,
    Body,
    Bytes,
}

/// A check line resolved into the engine's types.
pub(super) struct PreparedCheck {
    source: Source,
    check:  Check,
}

impl PreparedCheck {
    /// The subject's `ai:` target, when it has one.
    pub(super) fn take_ai(&mut self) -> Option<AiRead> {
        take_ai(&mut self.source)
    }
}

impl PreparedCapture {
    /// The subject's `ai:` target, when it has one.
    pub(super) fn take_ai(&mut self) -> Option<AiRead> {
        take_ai(&mut self.source)
    }
}

fn take_ai(source: &mut Source) -> Option<AiRead> {
    let Source::Page { ai, .. } = source else {
        return None;
    };
    ai.take().map(|ai| *ai)
}

impl FlowExec<'_> {
    /// The `ai` report and warnings of a check or capture that read an
    /// `ai:` target, after it keeps the target's cache entry (SPEC 12.1).
    pub(super) fn finish_ai_read(
        &mut self,
        node: StepNode<'_>,
        ai: Option<AiRead>,
        passed: bool,
    ) -> AiSpend {
        let mut spend = AiSpend::default();
        let Some(AiRead { target, usage, .. }) = ai else {
            return spend;
        };
        spend.usage = usage;
        let Some(target) = target else {
            return spend;
        };
        if passed {
            self.finish_target(node, target, &mut spend);
        } else {
            spend.targets.push(target.report_owned());
        }
        spend
    }
}

/// A capture line resolved into the engine's types.
pub(super) struct PreparedCapture {
    source:  Source,
    filters: Vec<Filter>,
    name:    String,
}

/// The budget of one check or capture line.
#[derive(Clone, Copy, Debug)]
pub(super) struct LineBudget {
    pub(super) timeout_ms:      u64,
    pub(super) entry_capped:    bool,
    pub(super) entry_budget_ms: u64,
}

/// One attempt to read a subject.
enum Attempt {
    Read(Read),
    /// A failure that a later read can clear.
    Retry(StepError),
    /// A failure that ends the line.
    End(StepEnd),
}

fn filter_error(message: String) -> BuildError {
    BuildError::Check(message)
}

impl FlowExec<'_> {
    /// Resolves a check line's subject, filters, and predicate (SPEC 9.6).
    pub(super) fn prepare_check(
        &mut self,
        line: &ast::CheckLine,
        implicit_response: Option<&str>,
    ) -> Result<PreparedCheck, BuildError> {
        let (source, filters) = self.prepare_source(
            &line.subject,
            &line.filters,
            implicit_response,
            true,
            ai_step::checks_absence(line),
        )?;
        let text_compare = ast::chain_type(&line.subject, &line.filters)
            .is_ok_and(|value_type| value_type.is_string());
        let predicate = match &line.predicate {
            PredicateSpec::Compare { kind, expected } => {
                Predicate::Compare(*kind, self.expected(expected, text_compare)?)
            }
            PredicateSpec::Matches(regex) => Predicate::Matches(compile(regex)?),
            PredicateSpec::Word(kind) => Predicate::Word(*kind),
        };
        let check = Check {
            filters,
            negated: line.negated,
            predicate,
        };
        Ok(PreparedCheck { source, check })
    }

    /// Resolves a capture line's subject and filters.
    pub(super) fn prepare_capture(
        &mut self,
        capture: &ast::Capture,
        implicit_response: Option<&str>,
    ) -> Result<PreparedCapture, BuildError> {
        let (source, filters) = self.prepare_source(
            &capture.subject,
            &capture.filters,
            implicit_response,
            false,
            false,
        )?;
        Ok(PreparedCapture {
            source,
            filters,
            name: capture.name.text.clone(),
        })
    }

    fn prepare_source(
        &mut self,
        subject: &Subject,
        filter_specs: &[FilterSpec],
        implicit_response: Option<&str>,
        in_check: bool,
        absence_ok: bool,
    ) -> Result<(Source, Vec<Filter>), BuildError> {
        let mut filters = Vec::with_capacity(filter_specs.len() + 1);
        let source = match subject {
            Subject::Response { name, field } => {
                let name = match name {
                    Some(name) => name.text.clone(),
                    None => implicit_response
                        .expect("an implicit response subject belongs to an HTTP entry")
                        .to_owned(),
                };
                let field = match field {
                    ResponseField::Status => ResponseRead::Status,
                    ResponseField::Header(header) => ResponseRead::Header(self.resolve(header)?),
                    ResponseField::Location => ResponseRead::Location,
                    ResponseField::Body => ResponseRead::Body,
                    ResponseField::Bytes => ResponseRead::Bytes,
                    ResponseField::Json(path) => {
                        let path = self.resolve(path)?;
                        filters.push(Filter::Json(JsonQuery::parse(&path).map_err(filter_error)?));
                        ResponseRead::Body
                    }
                    ResponseField::Xpath(expression) => {
                        let expression = self.resolve(expression)?;
                        filters.push(Filter::Xpath(
                            XpathQuery::parse(&expression).map_err(filter_error)?,
                        ));
                        ResponseRead::Body
                    }
                };
                // `bytes xpath:` parses the body like `body xpath:`: decoded
                // with the response charset (SPEC 9.5).
                let field = match (field, filter_specs.first()) {
                    (ResponseRead::Bytes, Some(first)) if first.kind == FilterKind::Xpath => {
                        ResponseRead::Body
                    }
                    (field, _) => field,
                };
                Source::Response { name, field }
            }
            Subject::Request { name, field } => {
                let field = match field {
                    RequestField::Method => RequestRead::Method,
                    RequestField::Url => RequestRead::Url,
                    RequestField::Header(header) => RequestRead::Header(self.resolve(header)?),
                    RequestField::Body => RequestRead::Body,
                    RequestField::Bytes => RequestRead::Bytes,
                    RequestField::Json(path) => {
                        let path = self.resolve(path)?;
                        filters.push(Filter::Json(JsonQuery::parse(&path).map_err(filter_error)?));
                        RequestRead::Body
                    }
                    RequestField::Xpath(expression) => {
                        let expression = self.resolve(expression)?;
                        filters.push(Filter::Xpath(
                            XpathQuery::parse(&expression).map_err(filter_error)?,
                        ));
                        RequestRead::Body
                    }
                };
                // `bytes xpath:` parses the body like `body xpath:` (SPEC 9.5).
                let field = match (field, filter_specs.first()) {
                    (RequestRead::Bytes, Some(first)) if first.kind == FilterKind::Xpath => {
                        RequestRead::Body
                    }
                    (field, _) => field,
                };
                Source::Request {
                    name: name.text.clone(),
                    field,
                }
            }
            Subject::Extract { name, .. } => Source::Extract {
                name: name.text.clone(),
            },
            page => {
                let ai = match page {
                    Subject::Element { locator, .. } if locator.ai_description().is_some() => {
                        Some(Box::new(AiRead {
                            subject: page.clone(),
                            absence_ok,
                            target: None,
                            usage: PlanUsage::default(),
                            stale: true,
                        }))
                    }
                    _ => None,
                };
                let subject = if ai.is_some() {
                    Json::Null
                } else {
                    let vars = &mut self.vars;
                    wire::read_subject_wire(page, &mut |value| vars.resolve(value))?
                        .expect("a page subject always has a read subject")
                };
                let attr = match page {
                    Subject::Element {
                        extractor: Extractor::Attr(name),
                        ..
                    } => Some(name.clone()),
                    _ => None,
                };
                let waits = !matches!(
                    page,
                    Subject::Eval(_)
                        | Subject::Element {
                            extractor: Extractor::Count,
                            ..
                        }
                );
                Source::Page {
                    subject,
                    attr,
                    retry: in_check || waits,
                    ai,
                }
            }
        };
        for spec in filter_specs {
            filters.push(self.filter(spec)?);
        }
        Ok((source, filters))
    }

    /// Resolves one filter's arguments (SPEC 9.5).
    fn filter(&mut self, spec: &FilterSpec) -> Result<Filter, BuildError> {
        let mut values = Vec::new();
        let mut regexes = Vec::new();
        let mut index = 0;
        for arg in &spec.args {
            match arg {
                FilterArg::Value(value) => values.push(self.resolve(value)?),
                FilterArg::Regex(regex) => regexes.push(compile(regex)?),
                FilterArg::Index(value) => index = *value,
            }
        }
        let mut values = values.into_iter();
        let mut value = || values.next().unwrap_or_default();
        let regex = regexes.into_iter().next();
        let pattern = || regex.clone().expect("the parser requires a regex argument");
        Ok(match spec.kind {
            FilterKind::Count => Filter::Count,
            FilterKind::First => Filter::First,
            FilterKind::Last => Filter::Last,
            FilterKind::Nth => Filter::Nth(index),
            FilterKind::Split => Filter::Split(value()),
            FilterKind::Regex => Filter::Regex(pattern()),
            FilterKind::Replace => Filter::Replace {
                old: value(),
                new: value(),
            },
            FilterKind::ReplaceRegex => Filter::ReplaceRegex {
                pattern:     pattern(),
                replacement: value(),
            },
            FilterKind::ToString => Filter::ToString,
            FilterKind::ToInt => Filter::ToInt,
            FilterKind::ToFloat => Filter::ToFloat,
            FilterKind::ToHex => Filter::ToHex,
            FilterKind::ToDate => Filter::ToDate(DateFormat::new(&value()).map_err(filter_error)?),
            FilterKind::DateFormat => {
                Filter::DateFormat(DateFormat::new(&value()).map_err(filter_error)?)
            }
            FilterKind::DaysAfterNow => Filter::DaysAfterNow,
            FilterKind::DaysBeforeNow => Filter::DaysBeforeNow,
            FilterKind::Base64Decode => Filter::Base64Decode,
            FilterKind::Base64Encode => Filter::Base64Encode,
            FilterKind::Base64UrlSafeDecode => Filter::Base64UrlSafeDecode,
            FilterKind::Base64UrlSafeEncode => Filter::Base64UrlSafeEncode,
            FilterKind::Utf8Decode => Filter::Utf8Decode,
            FilterKind::Utf8Encode => Filter::Utf8Encode,
            FilterKind::CharsetDecode => {
                Filter::CharsetDecode(Charset::from_label(&value()).map_err(filter_error)?)
            }
            FilterKind::UrlQueryParam => Filter::UrlQueryParam(value()),
            FilterKind::UrlEncode => Filter::UrlEncode,
            FilterKind::UrlDecode => Filter::UrlDecode,
            FilterKind::HtmlEscape => Filter::HtmlEscape,
            FilterKind::HtmlUnescape => Filter::HtmlUnescape,
            FilterKind::Json => Filter::Json(JsonQuery::parse(&value()).map_err(filter_error)?),
            FilterKind::Xpath => Filter::Xpath(XpathQuery::parse(&value()).map_err(filter_error)?),
        })
    }

    /// Reads the expected value (SPEC 9.6): text when the value under
    /// test is always a string, otherwise typed.
    fn expected(&mut self, operand: &Operand, text_compare: bool) -> Result<Expected, BuildError> {
        match operand {
            Operand::Value(value) => {
                if !text_compare && let Some(typed) = self.vars.resolve_typed(value)? {
                    // A bare whole `{{name}}` keeps the variable's type (SPEC 11).
                    return Ok(Expected::Typed(typed));
                }
                let resolved = self.resolve(value)?;
                if text_compare {
                    return Ok(Expected::Text(resolved));
                }
                if value.quoted {
                    return Ok(Expected::Typed(Value::String(resolved)));
                }
                Expected::bare(resolved).map_err(filter_error)
            }
            Operand::Json(literal) => {
                let resolved = if text_compare {
                    self.resolve(&literal.value)?
                } else {
                    self.vars.resolve_json(&literal.text, literal.value.span)?
                };
                if text_compare {
                    return Ok(Expected::Text(resolved));
                }
                check::parse_json(&resolved)
                    .map(Expected::Typed)
                    .map_err(|error| filter_error(format!("the JSON literal is invalid: {error}")))
            }
        }
    }

    /// Runs one check line (SPEC 9.7).
    pub(super) async fn run_check(
        &mut self,
        node: StepNode<'_>,
        prepared: &mut PreparedCheck,
        title: &str,
        budget: LineBudget,
        client: &mut ShimClient,
        state: &mut EntryState,
    ) -> StepEnd {
        let deadline = Instant::now() + Duration::from_millis(budget.timeout_ms);
        let retry = matches!(prepared.source, Source::Page { retry: true, .. });
        if retry {
            self.trace_group(StepCommand::TraceGroup, title, client)
                .await;
        }
        let mut attempt = 0;
        let mut last: Option<StepError> = None;
        let end = loop {
            let remaining = remaining_ms(deadline);
            if remaining == 0 {
                break timed_out(budget, last.take());
            }
            match self
                .read(
                    node,
                    &mut prepared.source,
                    remaining,
                    budget,
                    (!retry).then_some(title),
                    client,
                    state,
                )
                .await
            {
                Attempt::End(end) => break end,
                Attempt::Retry(error) => last = Some(error),
                Attempt::Read(read) => match prepared
                    .check
                    .evaluate(read, self.read_context(&prepared.source))
                {
                    Ok(()) => break StepEnd::Passed,
                    Err(failure) => last = Some(failure_error(&self.vars, &failure)),
                },
            }
            if !retry {
                break StepEnd::Failed(last.take().unwrap_or_default());
            }
            if !pause(deadline, attempt).await {
                break timed_out(budget, last.take());
            }
            attempt += 1;
        };
        if retry {
            self.trace_group(StepCommand::TraceGroupEnd, title, client)
                .await;
        }
        end
    }

    /// Runs one capture line (SPEC 10) and stores its value.
    pub(super) async fn run_capture(
        &mut self,
        node: StepNode<'_>,
        prepared: &mut PreparedCapture,
        title: &str,
        budget: LineBudget,
        client: &mut ShimClient,
        state: &mut EntryState,
    ) -> StepEnd {
        let deadline = Instant::now() + Duration::from_millis(budget.timeout_ms);
        let retry = matches!(prepared.source, Source::Page { retry: true, .. });
        if retry {
            self.trace_group(StepCommand::TraceGroup, title, client)
                .await;
        }
        let mut attempt = 0;
        let mut last: Option<StepError> = None;
        let end = loop {
            let remaining = remaining_ms(deadline);
            if remaining == 0 {
                break timed_out(budget, last.take());
            }
            match self
                .read(
                    node,
                    &mut prepared.source,
                    remaining,
                    budget,
                    (!retry).then_some(title),
                    client,
                    state,
                )
                .await
            {
                Attempt::End(end) => break end,
                Attempt::Retry(error) => last = Some(error),
                Attempt::Read(read) => match check::apply_filters(
                    &prepared.filters,
                    read,
                    self.read_context(&prepared.source),
                ) {
                    Ok(Read::Value(value)) => {
                        if value.text_form().is_none() {
                            break StepEnd::Failed(simple_error(
                                "filter-error",
                                "a node set cannot be captured",
                            ));
                        }
                        self.store_capture(&prepared.name, value, state);
                        break StepEnd::Passed;
                    }
                    Ok(Read::Missing(missing)) => {
                        last = Some(simple_error("missing-value", &missing.to_string()));
                    }
                    Err(error) => {
                        last = Some(simple_error(
                            "filter-error",
                            &self.vars.mask(&error.to_string()),
                        ));
                    }
                },
            }
            if !retry {
                break StepEnd::Failed(last.take().unwrap_or_default());
            }
            if !pause(deadline, attempt).await {
                break timed_out(budget, last.take());
            }
            attempt += 1;
        };
        if retry {
            self.trace_group(StepCommand::TraceGroupEnd, title, client)
                .await;
        }
        end
    }

    /// Stores a capture with its type (SPEC 10, 11). The report masks a
    /// value whose text form holds a secret, and keeps its type (SPEC 14).
    fn store_capture(&mut self, name: &str, value: Value, state: &mut EntryState) {
        let value_type = value.value_type().name();
        let text = value.text_form().unwrap_or_default();
        let reported = if self.vars.mask(&text) == text {
            CaptureValue::new(
                value_type,
                value.to_json().unwrap_or_else(|| "null".to_owned()),
            )
        } else {
            CaptureValue::masked(value_type, MASK)
        };
        // A later capture of the same name overwrites the earlier one
        // (SPEC 10).
        upsert(&mut state.captures, name, reported);
        upsert(&mut self.captures, name, value.clone());
        self.vars.set(name.to_owned(), value);
    }

    /// The context of a read: the time, and whether a first `xpath:` parses
    /// the body of an XML response (SPEC 9.5).
    fn read_context(&self, source: &Source) -> ReadContext {
        let markup = match source {
            Source::Response {
                name,
                field: ResponseRead::Body,
            } => self
                .responses
                .get(name)
                .map_or(Markup::Html, ResponseData::markup),
            Source::Request {
                name,
                field: RequestRead::Body,
            } => self
                .requests
                .get(name)
                .map_or(Markup::Html, RequestData::markup),
            _ => Markup::Html,
        };
        ReadContext { now: now(), markup }
    }

    /// Opens or closes a check's trace group. A tracing hiccup never
    /// fails the line.
    async fn trace_group(&mut self, command: StepCommand, title: &str, client: &mut ShimClient) {
        let request = StepRequest {
            entry_start: false,
            command,
            timeout_ms: 1_000,
            title: Some(title.to_owned()),
        };
        let _ = client.run_step(&request).await;
    }

    /// Reads the line's subject once. `title` names the read's trace group
    /// when the line opened none of its own (ADR `evaluate-checks-in-rust`
    /// §1.3).
    async fn read(
        &mut self,
        node: StepNode<'_>,
        source: &mut Source,
        remaining: u64,
        budget: LineBudget,
        title: Option<&str>,
        client: &mut ShimClient,
        state: &mut EntryState,
    ) -> Attempt {
        match source {
            Source::Page {
                subject, attr, ai, ..
            } => {
                if let Some(ai) = ai.as_deref_mut()
                    && ai.stale
                {
                    match self
                        .find_subject(node, ai, remaining, budget, title, client, state)
                        .await
                    {
                        Ok(Some(found)) => *subject = found,
                        // No element yet, or none at all: a missing value,
                        // which `not exists` accepts (SPEC 6.3).
                        Ok(None) => return Attempt::Read(Read::Missing(Missing::NoElement)),
                        Err(end) => return Attempt::End(end),
                    }
                }
                let command = StepCommand::Read {
                    subject: subject.clone(),
                };
                let result = match self
                    .shim_call(node, command, remaining, budget, title, client, state)
                    .await
                {
                    Ok(result) => result,
                    Err(attempt) => return attempt,
                };
                let result = serde_json::from_value::<ReadResult>(result);
                // The element is gone: find it again on a later attempt.
                if let (
                    Some(ai),
                    Ok(ReadResult::Missing {
                        reason: MissingReason::NoElement,
                    }),
                ) = (ai.as_deref_mut(), &result)
                {
                    ai.stale = true;
                }
                match result {
                    Ok(ReadResult::Value { value }) => {
                        Attempt::Read(Read::Value(Value::from_json(value)))
                    }
                    Ok(ReadResult::Missing {
                        reason: MissingReason::NoElement,
                    }) => Attempt::Read(Read::Missing(Missing::NoElement)),
                    Ok(ReadResult::Missing {
                        reason: MissingReason::AbsentAttribute,
                    }) => Attempt::Read(Read::Missing(Missing::AbsentAttribute(
                        attr.clone().unwrap_or_default(),
                    ))),
                    Err(_) => Attempt::End(StepEnd::Error(simple_error(
                        "internal",
                        "malformed read result from the shim",
                    ))),
                }
            }
            Source::Response { name, field } => {
                let needs_body = matches!(field, ResponseRead::Body | ResponseRead::Bytes);
                let cached = self
                    .responses
                    .get(name)
                    .is_some_and(|data| !needs_body || data.body.is_some());
                if !cached {
                    let command = StepCommand::ReadResponse {
                        name: name.clone(),
                        body: needs_body,
                    };
                    let result = match self
                        .shim_call(node, command, remaining, budget, title, client, state)
                        .await
                    {
                        Ok(result) => result,
                        Err(Attempt::Retry(error)) => return Attempt::End(StepEnd::Failed(error)),
                        Err(attempt) => return attempt,
                    };
                    let Ok(read) = serde_json::from_value::<ResponseReadResult>(result) else {
                        return Attempt::End(StepEnd::Error(simple_error(
                            "internal",
                            "malformed readResponse result from the shim",
                        )));
                    };
                    self.responses
                        .insert(name.clone(), ResponseData::from_read(read));
                }
                let data = &self.responses[name];
                match data.field(field) {
                    Ok(read) => Attempt::Read(read),
                    Err(message) => Attempt::End(StepEnd::Failed(simple_error("read", &message))),
                }
            }
            Source::Extract { name } => Attempt::Read(match self.extracts.get(name) {
                Some(Some(value)) => Read::Value(value.clone()),
                _ => Read::Missing(Missing::NoExtractValue(name.clone())),
            }),
            Source::Request { name, field } => {
                if !self.requests.contains_key(name) {
                    let command = StepCommand::ReadRequest { name: name.clone() };
                    let result = match self
                        .shim_call(node, command, remaining, budget, title, client, state)
                        .await
                    {
                        Ok(result) => result,
                        Err(Attempt::Retry(error)) => return Attempt::End(StepEnd::Failed(error)),
                        Err(attempt) => return attempt,
                    };
                    let Ok(read) = serde_json::from_value::<RequestReadResult>(result) else {
                        return Attempt::End(StepEnd::Error(simple_error(
                            "internal",
                            "malformed readRequest result from the shim",
                        )));
                    };
                    self.requests
                        .insert(name.clone(), RequestData::from_read(read));
                }
                match self.requests[name].field(field) {
                    Ok(read) => Attempt::Read(read),
                    Err(message) => Attempt::End(StepEnd::Failed(simple_error("read", &message))),
                }
            }
        }
    }

    /// Finds the element of a subject's `ai:` target, at most once every 2
    /// seconds (SPEC 6.3), and returns the read subject on it. `None` means
    /// no element yet.
    async fn find_subject(
        &mut self,
        node: StepNode<'_>,
        ai: &mut AiRead,
        remaining: u64,
        budget: LineBudget,
        title: Option<&str>,
        client: &mut ShimClient,
        state: &mut EntryState,
    ) -> Result<Option<Json>, StepEnd> {
        if ai.target.is_none() {
            let Subject::Element { locator, .. } = &ai.subject else {
                unreachable!("an AI read has an element subject");
            };
            ai.target = Some(self.ai_target(node, locator, ai.absence_ok));
        }
        let target = ai.target.as_mut().expect("the target was just made");
        if !target.may_ask() {
            return Ok(None);
        }
        let mut line = ActLine {
            node,
            title: title.unwrap_or_default(),
            deadline: Instant::now() + Duration::from_millis(remaining),
            budget: ActBudget {
                timeout_ms:      budget.timeout_ms,
                entry_capped:    budget.entry_capped,
                entry_budget_ms: budget.entry_budget_ms,
            },
            entry_start: false,
            what: "the step",
        };
        let locator = match self
            .find_target(&mut line, target, &mut ai.usage, client, state)
            .await?
        {
            Found::One(locator) => locator,
            Found::Nothing => return Ok(None),
        };
        ai.stale = false;
        let mut subject = ai.subject.clone();
        if let Subject::Element {
            locator: target, ..
        } = &mut subject
        {
            *target = locator;
        }
        let vars = &mut self.vars;
        let wire = wire::read_subject_wire(&subject, &mut |value| vars.resolve(value)).map_err(
            |error| {
                StepEnd::Failed(StepError {
                    code: "variable-resolution".to_owned(),
                    message: error.to_string(),
                    ..StepError::default()
                })
            },
        )?;
        Ok(wire)
    }

    /// Runs one shim command for a line. A retryable page failure comes
    /// back as [`Attempt::Retry`]; every other failure is classified like
    /// any step's (protocol section 7).
    async fn shim_call(
        &mut self,
        node: StepNode<'_>,
        command: StepCommand,
        timeout_ms: u64,
        budget: LineBudget,
        title: Option<&str>,
        client: &mut ShimClient,
        state: &mut EntryState,
    ) -> Result<Json, Attempt> {
        let request = StepRequest {
            entry_start: false,
            command,
            timeout_ms,
            title: title.map(str::to_owned),
        };
        let started = Instant::now();
        match client.run_step(&request).await {
            StepOutcome::Ok(result) => Ok(result),
            StepOutcome::ShimError(error) if RETRYABLE_KINDS.contains(&error.kind.as_str()) => {
                Err(Attempt::Retry(step_error(&self.vars, &error)))
            }
            outcome => {
                let step_budget = StepBudget {
                    entry_capped: budget.entry_capped,
                    entry_budget_ms: budget.entry_budget_ms,
                    timeout_ms,
                    elapsed: started.elapsed(),
                };
                Err(Attempt::End(self.apply_outcome(
                    node,
                    outcome,
                    state,
                    step_budget,
                )))
            }
        }
    }
}

impl ResponseData {
    fn from_read(read: ResponseReadResult) -> Self {
        let body = match (read.body_base64, read.body_error) {
            (Some(encoded), _) => Some(
                STANDARD
                    .decode(encoded)
                    .map_err(|_| "the shim sent a malformed body".to_owned()),
            ),
            (None, Some(error)) => Some(Err(error)),
            (None, None) => None,
        };
        Self {
            status: read.status,
            url: read.url,
            headers: read.headers,
            body,
            body_may_be_decoded: read.body_may_be_decoded,
        }
    }

    /// A header's value; repeated headers join with `, `. Names compare
    /// without case.
    fn header(&self, name: &str) -> Option<String> {
        let values: Vec<&str> = self
            .headers
            .iter()
            .filter(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
            .collect();
        (!values.is_empty()).then(|| values.join(", "))
    }

    /// XML when the `Content-Type` names an XML media type, else HTML.
    fn markup(&self) -> Markup {
        match self.header("content-type") {
            Some(content_type) if check::is_xml_content_type(&content_type) => Markup::Xml,
            _ => Markup::Html,
        }
    }

    fn body(&self) -> Result<&[u8], String> {
        match &self.body {
            Some(Ok(bytes)) => Ok(bytes),
            Some(Err(error)) => Err(format!("the response body is unavailable: {error}")),
            None => Err("the response body was not read".to_owned()),
        }
    }

    /// Reads one field (SPEC 9.2).
    fn field(&self, field: &ResponseRead) -> Result<Read, String> {
        Ok(match field {
            ResponseRead::Status => {
                Read::Value(Value::Number(Number::integer(i64::from(self.status))))
            }
            ResponseRead::Header(name) => match self.header(name) {
                Some(value) => Read::Value(Value::String(value)),
                None => Read::Missing(Missing::AbsentHeader(name.clone())),
            },
            ResponseRead::Location => match self.header("location") {
                Some(location) => {
                    let absolute = url::Url::parse(&self.url)
                        .and_then(|base| base.join(&location))
                        .map_or(location, |joined| joined.to_string());
                    Read::Value(Value::String(absolute))
                }
                None => Read::Missing(Missing::AbsentHeader("location".to_owned())),
            },
            ResponseRead::Bytes => Read::Value(Value::Bytes(match self.undo_browser_decode()? {
                Some((_, bytes)) => bytes,
                None => self.body()?.to_vec(),
            })),
            ResponseRead::Body => Read::Value(Value::String(match self.undo_browser_decode()? {
                Some((text, _)) => text,
                None => self.charset()?.decode(self.body()?)?,
            })),
        })
    }

    /// The `Content-Type` charset, UTF-8 by default.
    fn charset(&self) -> Result<Charset, String> {
        let label = self
            .header("content-type")
            .and_then(|content_type| charset_label(&content_type))
            .unwrap_or_else(|| "utf-8".to_owned());
        Charset::from_label(&label)
    }

    /// The text and bytes of a body that the browser handed back decoded,
    /// when Whirl can undo that (SPEC 9.2).
    fn undo_browser_decode(&self) -> Result<Option<(String, Vec<u8>)>, String> {
        if !self.body_may_be_decoded {
            return Ok(None);
        }
        let body = self.body()?;
        Ok(self
            .charset()
            .ok()
            .and_then(|charset| charset.undo_browser_decode(body)))
    }
}

impl RequestData {
    fn from_read(read: RequestReadResult) -> Self {
        let body = match (read.body_base64, read.body_error) {
            (Some(encoded), _) => STANDARD
                .decode(encoded)
                .map_err(|_| "the shim sent a malformed body".to_owned()),
            (None, Some(error)) => Err(error),
            (None, None) => Ok(Vec::new()),
        };
        Self {
            method: read.method,
            url: read.url,
            headers: read.headers,
            body,
        }
    }

    /// A header's value; repeated headers join with `, `. Names compare
    /// without case.
    fn header(&self, name: &str) -> Option<String> {
        let values: Vec<&str> = self
            .headers
            .iter()
            .filter(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
            .collect();
        (!values.is_empty()).then(|| values.join(", "))
    }

    /// XML when the `Content-Type` names an XML media type, else HTML.
    fn markup(&self) -> Markup {
        match self.header("content-type") {
            Some(content_type) if check::is_xml_content_type(&content_type) => Markup::Xml,
            _ => Markup::Html,
        }
    }

    fn body(&self) -> Result<&[u8], String> {
        self.body
            .as_deref()
            .map_err(|error| format!("the request body is unavailable: {error}"))
    }

    /// Reads one field (SPEC 9.2).
    fn field(&self, field: &RequestRead) -> Result<Read, String> {
        Ok(match field {
            RequestRead::Method => Read::Value(Value::String(self.method.clone())),
            RequestRead::Url => Read::Value(Value::String(self.url.clone())),
            RequestRead::Header(name) => match self.header(name) {
                Some(value) => Read::Value(Value::String(value)),
                None => Read::Missing(Missing::AbsentHeader(name.clone())),
            },
            RequestRead::Bytes => Read::Value(Value::Bytes(self.body()?.to_vec())),
            RequestRead::Body => {
                let label = self
                    .header("content-type")
                    .and_then(|content_type| charset_label(&content_type))
                    .unwrap_or_else(|| "utf-8".to_owned());
                Read::Value(Value::String(
                    Charset::from_label(&label)?.decode(self.body()?)?,
                ))
            }
        })
    }
}

/// Replaces the value stored under `name`, or appends it.
fn upsert<T>(entries: &mut Vec<(String, T)>, name: &str, value: T) {
    match entries.iter_mut().find(|(existing, _)| existing == name) {
        Some((_, slot)) => *slot = value,
        None => entries.push((name.to_owned(), value)),
    }
}

/// The `charset` parameter of a `Content-Type` value.
fn charset_label(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|param| {
        let (key, value) = param.split_once('=')?;
        key.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches('"').to_owned())
    })
}

fn compile(regex: &ast::Regex) -> Result<Pattern, BuildError> {
    let flags = PatternFlags {
        ignore_case: regex.flags.ignore_case,
        dot_all:     regex.flags.dot_all,
        multiline:   regex.flags.multiline,
    };
    Pattern::new(&regex.pattern, flags).map_err(|error| filter_error(error.to_string()))
}

/// The current time for the day filters. chrono's `clock` feature is off,
/// so the time comes from the standard library.
fn now() -> DateTime<Utc> {
    DateTime::from(SystemTime::now())
}

fn remaining_ms(deadline: Instant) -> u64 {
    u64::try_from(
        deadline
            .saturating_duration_since(Instant::now())
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

/// Waits before the next read. Returns false when the budget has too
/// little time left for another read, so the line keeps the result of its
/// last real attempt.
async fn pause(deadline: Instant, attempt: usize) -> bool {
    let interval = POLL_INTERVALS_MS[attempt.min(POLL_INTERVALS_MS.len() - 1)];
    let remaining = remaining_ms(deadline);
    if remaining < MIN_RETRY_MS {
        return false;
    }
    sleep(Duration::from_millis(interval.min(remaining))).await;
    remaining_ms(deadline) >= MIN_RETRY_MS
}

/// The end of a line whose budget ran out: an entry-timeout failure when
/// the entry budget capped the line, else the last attempt's failure
/// (SPEC 9.7, 12).
fn timed_out(budget: LineBudget, last: Option<StepError>) -> StepEnd {
    if budget.entry_capped {
        return StepEnd::Failed(entry_timeout_error(budget.entry_budget_ms));
    }
    StepEnd::Failed(last.unwrap_or_else(|| {
        simple_error(
            "timeout",
            &format!("the check did not pass within {}ms", budget.timeout_ms),
        )
    }))
}

fn simple_error(code: &str, message: &str) -> StepError {
    StepError {
        code: code.to_owned(),
        message: format!("{code}: {message}"),
        ..StepError::default()
    }
}

/// A check failure as a masked report detail (SPEC 9.7, 11).
fn failure_error(vars: &VarStore, failure: &check::Failure) -> StepError {
    StepError {
        code:       failure.code.as_str().to_owned(),
        message:    vars.mask(&format!("{}: {}", failure.code, failure.message)),
        expected:   Some(vars.mask(&failure.expected)),
        actual:     Some(vars.mask(&failure.actual)),
        candidates: None,
    }
}

/// The flow's cache of read responses.
pub(super) type ResponseCache = HashMap<String, ResponseData>;

/// The flow's cache of read requests, by `RESPONSE` name.
pub(super) type RequestCache = HashMap<String, RequestData>;
