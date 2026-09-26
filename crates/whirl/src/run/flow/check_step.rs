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

use super::{
    BuildError, EntryState, FlowExec, StepBudget, StepEnd, StepNode, entry_timeout_error,
    step_error,
};
use crate::check::{
    self, Charset, Check, DateFormat, Expected, Filter, FilterKind, JsonQuery, Markup, Missing,
    Pattern, PatternFlags, Predicate, Read, ReadContext, Value, XpathQuery,
};
use crate::lang::ast::{
    self, Extractor, FilterArg, FilterSpec, Operand, PredicateSpec, ResponseField, Subject,
};
use crate::report::model::{CaptureValue, StepError};
use crate::run::shim::{
    MissingReason, ReadResult, ResponseReadResult, ShimClient, StepCommand, StepOutcome,
    StepRequest, wire,
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
    },
    Response {
        name:  String,
        field: ResponseRead,
    },
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
        let (source, filters) =
            self.prepare_source(&line.subject, &line.filters, implicit_response, true)?;
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
        let (source, filters) =
            self.prepare_source(&capture.subject, &capture.filters, implicit_response, false)?;
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
            page => {
                let vars = &mut self.vars;
                let subject = wire::read_subject_wire(page, &mut |value| vars.resolve(value))?
                    .expect("a page subject always has a read subject");
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
        prepared: PreparedCheck,
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
                    &prepared.source,
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
        prepared: PreparedCapture,
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
                    &prepared.source,
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
        source: &Source,
        remaining: u64,
        budget: LineBudget,
        title: Option<&str>,
        client: &mut ShimClient,
        state: &mut EntryState,
    ) -> Attempt {
        match source {
            Source::Page { subject, attr, .. } => {
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
                match serde_json::from_value::<ReadResult>(result) {
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
        }
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
            ResponseRead::Status => Read::Value(Value::Number(check::Number::integer(i64::from(
                self.status,
            )))),
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
