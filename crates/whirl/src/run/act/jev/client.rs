//! TypeSafe's Jev through `lithos-llm`'s evaluation API: the classifier the
//! `--jev` planner asks first (SPEC 7.4, 13).
//!
//! Jev cannot write text. It answers typed questions about a JSON state
//! with probabilities: a choice among named options, or the probability
//! that a statement holds. `lithos-llm` routes `typesafe/jev-latest` to
//! TypeSafe's `/v1/systemone`, checks each verdict, retries throttled and
//! failed calls, and prices the verdict from its catalog.

use std::collections::HashMap;
use std::env;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use lithos_llm::catalog::{Catalog, CatalogError};
use lithos_llm::client::ClientBuildError;
use lithos_llm::credentials::ConventionalCredentials;
use lithos_llm::middleware::{CallContext, RetryMiddleware, RetryPolicy};
use lithos_llm::types::ErrorKind;
use lithos_llm::{Client, Evaluation, Verdict};
use serde_json::{Map, Value as Json};

/// Jev's key (SPEC 13). `lithos-llm` reads it for the `typesafe` provider.
pub(crate) const API_KEY_ENV: &str = "TYPESAFE_API_KEY";
/// Another Jev server, such as a twin in tests (SPEC 13).
pub(crate) const ENDPOINT_ENV: &str = "WHIRL_JEV_ENDPOINT";
/// The catalog row for Jev on TypeSafe's own API.
const MODEL: &str = "typesafe/jev-latest";

/// One request's cap, below any `ACT` budget.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
/// A rejected key fails every request the same way, so Jev pauses.
const AUTH_PAUSE: Duration = Duration::from_secs(60);
/// After this many failures in a row, Jev pauses for [`OUTAGE_PAUSE`].
const FAILURES_TO_PAUSE: u32 = 3;
const OUTAGE_PAUSE: Duration = Duration::from_secs(30);

/// A question to Jev.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum JevQuestion {
    /// Which of the named options fits; `options` maps each option's key
    /// to its description, in order.
    Choice {
        instructions: Json,
        options:      Map<String, Json>,
    },
    /// How likely the statement is to hold.
    Noul { instructions: Json },
}

/// Jev's answer to one question.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum JevAnswer {
    Choice {
        choice:        String,
        confidence:    f64,
        probabilities: HashMap<String, f64>,
    },
    Noul {
        noul: f64,
    },
}

/// What one request used.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct JevSpend {
    pub(crate) input_tokens:    u64,
    pub(crate) output_tokens:   u64,
    /// `None` when the catalog could not price the request.
    pub(crate) cost_usd_micros: Option<u64>,
}

/// Jev's answers, by question name.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct JevResponse {
    pub(crate) answers: HashMap<String, JevAnswer>,
    pub(crate) spend:   JevSpend,
}

/// Why a request got no answers. The planner falls back either way, so
/// the kinds only steer the breaker.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum JevError {
    #[error("Jev rejected the key")]
    Auth,
    #[error("the Jev request failed")]
    Failed,
    #[error("Jev is paused after repeated failures")]
    Paused,
}

/// A failure to set up the Jev client. The run stops with a runtime error
/// (exit 3).
#[derive(Debug, thiserror::Error)]
pub(crate) enum JevSetupError {
    #[error("--jev needs {API_KEY_ENV}")]
    MissingKey,
    #[error("the Jev catalog could not be built")]
    Catalog(#[source] Box<CatalogError>),
    #[error("the Jev client could not be built")]
    Client(#[source] Box<ClientBuildError>),
}

/// Pauses Jev after a rejected key or repeated failures, so a broken setup
/// does not cost each `ACT` line a doomed request before its fallback.
#[derive(Debug, Default)]
struct Breaker {
    paused_until: Option<Instant>,
    failures:     u32,
}

impl Breaker {
    fn record(&mut self, error: Option<JevError>) {
        match error {
            None => self.failures = 0,
            Some(JevError::Auth) => self.paused_until = Some(Instant::now() + AUTH_PAUSE),
            Some(_) => {
                self.failures += 1;
                if self.failures >= FAILURES_TO_PAUSE {
                    self.failures = 0;
                    self.paused_until = Some(Instant::now() + OUTAGE_PAUSE);
                }
            }
        }
    }

    fn paused(&self) -> bool {
        self.paused_until
            .is_some_and(|until| Instant::now() < until)
    }
}

/// The run's Jev client, shared by every flow.
#[derive(Debug)]
pub(crate) struct JevClient {
    client:  Client,
    breaker: Mutex<Breaker>,
}

impl JevClient {
    /// Builds the client from the environment (SPEC 13).
    pub(crate) fn from_env() -> Result<Self, JevSetupError> {
        if env::var(API_KEY_ENV).map_or(true, |key| key.is_empty()) {
            return Err(JevSetupError::MissingKey);
        }
        let endpoint = env::var(ENDPOINT_ENV).ok().filter(|url| !url.is_empty());
        let catalog = catalog(endpoint.as_deref())
            .map_err(|source| JevSetupError::Catalog(Box::new(source)))?;
        let build = Client::builder()
            .catalog(catalog)
            .credentials(ConventionalCredentials::new())
            .application("whirl")
            .middleware(RetryMiddleware::new(RetryPolicy::default()))
            .build()
            .map_err(|source| JevSetupError::Client(Box::new(source)))?;
        Ok(Self {
            client:  build.client,
            breaker: Mutex::new(Breaker::default()),
        })
    }

    /// Asks the questions about `state`. The request, with its retries,
    /// ends by `deadline`.
    pub(crate) async fn ask(
        &self,
        state: Json,
        questions: Vec<(&'static str, JevQuestion)>,
        deadline: Instant,
    ) -> Result<JevResponse, JevError> {
        if self.breaker().paused() {
            return Err(JevError::Paused);
        }
        let result = self.evaluate(state, questions, deadline).await;
        self.breaker().record(result.as_ref().err().copied());
        result
    }

    async fn evaluate(
        &self,
        state: Json,
        questions: Vec<(&'static str, JevQuestion)>,
        deadline: Instant,
    ) -> Result<JevResponse, JevError> {
        let mut builder = Evaluation::builder()
            .model(MODEL)
            .state(state)
            .timeout(REQUEST_TIMEOUT);
        let asked: Vec<(&'static str, bool)> = questions
            .iter()
            .map(|(id, question)| (*id, matches!(question, JevQuestion::Choice { .. })))
            .collect();
        for (id, question) in questions {
            builder = match question {
                JevQuestion::Choice {
                    instructions,
                    options,
                } => builder.choice(
                    id,
                    instructions,
                    options
                        .into_iter()
                        .map(|(key, description)| (key, Some(description))),
                ),
                JevQuestion::Noul { instructions } => builder.boolean(id, instructions),
            };
        }
        let evaluation = builder.build().map_err(|_| JevError::Failed)?;
        let mut context = CallContext::new();
        context.set_deadline(deadline);
        let verdict = self
            .client
            .evaluate_with_context(evaluation, context)
            .await
            .map_err(|error| match error.kind() {
                ErrorKind::Authentication | ErrorKind::AccessDenied => JevError::Auth,
                _ => JevError::Failed,
            })?;
        Ok(response(&verdict, &asked))
    }

    fn breaker(&self) -> MutexGuard<'_, Breaker> {
        self.breaker
            .lock()
            .expect("breaker users do not panic while holding the lock")
    }
}

/// The built-in catalog, with the `typesafe` provider pointed at another
/// server when `endpoint` is set.
fn catalog(endpoint: Option<&str>) -> Result<Catalog, CatalogError> {
    let builder = Catalog::builder().with_builtin();
    let Some(url) = endpoint else {
        return builder.build();
    };
    let base_url = serde_json::to_string(&format!("{}/v1", url.trim_end_matches('/')))
        .expect("a string always serializes");
    let overlay = format!("schema_version = 1\n\n[providers.typesafe]\nbase_url = {base_url}\n");
    builder.toml_layer(ENDPOINT_ENV, &overlay)?.build()
}

/// A verdict in the planner's terms. `asked` names each question and
/// whether it was a choice; `lithos-llm` checked that every one has an
/// answer of that kind.
fn response(verdict: &Verdict, asked: &[(&'static str, bool)]) -> JevResponse {
    let answers = asked
        .iter()
        .filter_map(|&(id, is_choice)| {
            let answer = if is_choice {
                let choice = verdict.choice(id).ok()?;
                JevAnswer::Choice {
                    choice:        choice.choice.clone(),
                    confidence:    choice.confidence.unwrap_or_default(),
                    probabilities: choice
                        .probabilities
                        .iter()
                        .flatten()
                        .map(|(key, probability)| (key.clone(), *probability))
                        .collect(),
                }
            } else {
                JevAnswer::Noul {
                    noul: verdict.boolean(id).ok()?.probability,
                }
            };
            Some((id.to_owned(), answer))
        })
        .collect();
    JevResponse {
        answers,
        spend: JevSpend {
            input_tokens:    verdict.usage.input.saturating_add(verdict.usage.cache_read),
            output_tokens:   verdict.usage.output.saturating_add(verdict.usage.reasoning),
            cost_usd_micros: verdict.cost.map(|cost| cost.usd_micros),
        },
    }
}

/// A choice question.
pub(crate) fn choice(instructions: Json, options: Map<String, Json>) -> JevQuestion {
    JevQuestion::Choice {
        instructions,
        options,
    }
}

/// A yes-or-no question, answered as a probability.
pub(crate) fn noul(instructions: Json) -> JevQuestion {
    JevQuestion::Noul { instructions }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_breaker_pauses_after_a_rejected_key_or_three_failures() {
        let mut breaker = Breaker::default();
        breaker.record(Some(JevError::Failed));
        breaker.record(Some(JevError::Failed));
        assert!(!breaker.paused());
        breaker.record(None);
        for _ in 0..3 {
            breaker.record(Some(JevError::Failed));
        }
        assert!(breaker.paused());

        let mut breaker = Breaker::default();
        breaker.record(Some(JevError::Auth));
        assert!(breaker.paused());
    }

    #[test]
    fn the_catalog_routes_jev_to_typesafe_or_to_the_endpoint() {
        let builtin = catalog(None).expect("the built-in catalog is valid");
        let local = catalog(Some("http://127.0.0.1:3000/")).expect("the overlay is valid");
        for (catalog, base_url) in [
            (builtin, "https://api.typesafe.ai/v1"),
            (local, "http://127.0.0.1:3000/v1"),
        ] {
            let provider = catalog
                .provider("typesafe")
                .expect("the catalog has the typesafe provider");
            assert_eq!(provider.base_url(), base_url);
        }
    }
}
