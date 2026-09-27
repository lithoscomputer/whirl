//! A client for TypeSafe's Jev (`POST /v1/systemone`), the classifier the
//! `--jev` planner asks first (SPEC 7.4, 13).
//!
//! Jev cannot write text. It answers typed questions with probabilities: a
//! `choice` among named options, or a `noul` probability for yes or no. A
//! request carries a JSON `state` and a map of named questions.

use std::collections::HashMap;
use std::env;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use reqwest::header::CONTENT_TYPE;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json};
use tokio::time::sleep;

/// Jev's key (SPEC 13).
pub(crate) const API_KEY_ENV: &str = "TYPESAFE_API_KEY";
/// Another Jev server, such as a twin in tests (SPEC 13).
pub(crate) const ENDPOINT_ENV: &str = "WHIRL_JEV_ENDPOINT";
const DEFAULT_ENDPOINT: &str = "https://api.typesafe.ai";
const MODEL: &str = "jev-latest";

/// One request's cap, below any `ACT` budget.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
/// Rate-limit and overload statuses, tried again after a short pause.
const RETRY_STATUSES: [u16; 2] = [429, 529];
const MAX_ATTEMPTS: u32 = 3;
const RETRY_PAUSE: Duration = Duration::from_millis(250);
/// A rejected key fails every request the same way, so Jev pauses.
const AUTH_PAUSE: Duration = Duration::from_secs(60);
/// After this many failures in a row, Jev pauses for [`OUTAGE_PAUSE`].
const FAILURES_TO_PAUSE: u32 = 3;
const OUTAGE_PAUSE: Duration = Duration::from_secs(30);

/// A question to Jev.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(crate) enum JevQuestion {
    /// Which of the named options fits; `criteria` maps each option's key
    /// to its description.
    Choice {
        instructions: Json,
        criteria:     Map<String, Json>,
    },
    /// How likely the answer is yes.
    Noul { instructions: Json },
}

/// Jev's answer to one question.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
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

/// The tokens one request used.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
pub(crate) struct JevTokens {
    #[serde(default)]
    pub(crate) input_tokens:  u64,
    #[serde(default)]
    pub(crate) output_tokens: u64,
}

/// Jev's answers, by question name.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub(crate) struct JevResponse {
    pub(crate) answers: HashMap<String, JevAnswer>,
    #[serde(default)]
    pub(crate) usage:   JevTokens,
}

/// Why a request got no answers. Messages carry a status or a cause, never
/// the key or a response body.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum JevError {
    #[error("Jev rejected the key (HTTP {status})")]
    Auth { status: u16 },
    #[error("Jev answered HTTP {status}")]
    Status { status: u16 },
    #[error("Jev did not answer in time")]
    Timeout,
    #[error("the Jev request failed")]
    Network,
    #[error("Jev's answer is not a systemone response")]
    Payload,
    #[error("Jev is paused after repeated failures")]
    Paused,
}

/// A failure to set up the Jev client. The run stops with a runtime error
/// (exit 3).
#[derive(Debug, thiserror::Error)]
pub(crate) enum JevSetupError {
    #[error("--jev needs {API_KEY_ENV}")]
    MissingKey,
    #[error("the Jev HTTP client could not be built")]
    Client(#[source] reqwest::Error),
}

/// Pauses Jev after a rejected key or repeated failures, so a broken setup
/// does not cost each `ACT` line a doomed request before its fallback.
#[derive(Debug, Default)]
struct Breaker {
    paused_until: Option<Instant>,
    failures:     u32,
}

/// The run's Jev client, shared by every flow.
#[derive(Debug)]
pub(crate) struct JevClient {
    http:    reqwest::Client,
    url:     String,
    key:     String,
    breaker: Mutex<Breaker>,
}

impl JevClient {
    /// Reads the key and the endpoint from the environment (SPEC 13).
    pub(crate) fn from_env() -> Result<Self, JevSetupError> {
        let key = env::var(API_KEY_ENV)
            .ok()
            .filter(|key| !key.is_empty())
            .ok_or(JevSetupError::MissingKey)?;
        let endpoint = env::var(ENDPOINT_ENV)
            .ok()
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| DEFAULT_ENDPOINT.to_owned());
        let http = reqwest::Client::builder()
            .build()
            .map_err(JevSetupError::Client)?;
        Ok(Self {
            http,
            url: format!("{}/v1/systemone", endpoint.trim_end_matches('/')),
            key,
            breaker: Mutex::new(Breaker::default()),
        })
    }

    /// Asks the questions about `state`. The request, with its retries,
    /// ends by `deadline`.
    pub(crate) async fn ask(
        &self,
        state: Json,
        questions: Map<String, Json>,
        deadline: Instant,
    ) -> Result<JevResponse, JevError> {
        if self
            .breaker()
            .paused_until
            .is_some_and(|until| Instant::now() < until)
        {
            return Err(JevError::Paused);
        }
        let body = serde_json::to_vec(&serde_json::json!({
            "state": state,
            "model": MODEL,
            "questions": questions,
        }))
        .expect("a JSON value always serializes");
        let result = self.send(body, deadline).await;
        self.record(result.as_ref().err());
        result
    }

    async fn send(&self, body: Vec<u8>, deadline: Instant) -> Result<JevResponse, JevError> {
        let mut attempt = 1;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(JevError::Timeout);
            }
            let response = self
                .http
                .post(&self.url)
                .bearer_auth(&self.key)
                .header(CONTENT_TYPE, "application/json")
                .timeout(left.min(REQUEST_TIMEOUT))
                .body(body.clone())
                .send()
                .await
                .map_err(|error| {
                    if error.is_timeout() {
                        JevError::Timeout
                    } else {
                        JevError::Network
                    }
                })?;
            let status = response.status().as_u16();
            if response.status().is_success() {
                let bytes = response.bytes().await.map_err(|_| JevError::Network)?;
                return serde_json::from_slice(&bytes).map_err(|_| JevError::Payload);
            }
            if matches!(status, 401 | 403) {
                return Err(JevError::Auth { status });
            }
            if !RETRY_STATUSES.contains(&status) || attempt == MAX_ATTEMPTS {
                return Err(JevError::Status { status });
            }
            sleep(RETRY_PAUSE * attempt).await;
            attempt += 1;
        }
    }

    fn breaker(&self) -> MutexGuard<'_, Breaker> {
        self.breaker
            .lock()
            .expect("breaker users do not panic while holding the lock")
    }

    fn record(&self, error: Option<&JevError>) {
        let mut breaker = self.breaker();
        match error {
            None => breaker.failures = 0,
            Some(JevError::Auth { .. }) => {
                breaker.paused_until = Some(Instant::now() + AUTH_PAUSE);
            }
            Some(_) => {
                breaker.failures += 1;
                if breaker.failures >= FAILURES_TO_PAUSE {
                    breaker.failures = 0;
                    breaker.paused_until = Some(Instant::now() + OUTAGE_PAUSE);
                }
            }
        }
    }
}

/// A `choice` question with the given options.
pub(crate) fn choice(instructions: Json, criteria: Map<String, Json>) -> Json {
    serde_json::to_value(JevQuestion::Choice {
        instructions,
        criteria,
    })
    .expect("a question always serializes")
}

/// A `noul` question.
pub(crate) fn noul(instructions: Json) -> Json {
    serde_json::to_value(JevQuestion::Noul { instructions }).expect("a question always serializes")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn questions_serialize_as_systemone_expects() {
        let mut criteria = Map::new();
        criteria.insert("e14".to_owned(), json!({"role": "button"}));
        assert_eq!(
            choice(json!("delete invoice 1037"), criteria),
            json!({"type": "choice", "instructions": "delete invoice 1037", "criteria": {"e14": {"role": "button"}}})
        );
        assert_eq!(
            noul(json!({"question": "?"})),
            json!({"type": "noul", "instructions": {"question": "?"}})
        );
    }

    #[test]
    fn a_live_answer_parses() {
        let answer: JevResponse = serde_json::from_value(json!({
            "model": "jev-1.13.0",
            "answers": {
                "strict": {"type": "choice", "choice": "e14", "confidence": 0.99,
                           "probabilities": {"none_match": 0.0, "e14": 1.0}},
                "plausible": {"type": "noul", "noul": 0.91}
            },
            "usage": {"input_tokens": 520, "output_tokens": 73}
        }))
        .expect("a live answer parses");
        assert_eq!(answer.usage.input_tokens, 520);
        assert_eq!(answer.answers["plausible"], JevAnswer::Noul { noul: 0.91 });
    }

    #[test]
    fn the_breaker_pauses_after_a_rejected_key_or_three_failures() {
        let client = JevClient {
            http:    reqwest::Client::new(),
            url:     "http://127.0.0.1:1/v1/systemone".to_owned(),
            key:     "k".to_owned(),
            breaker: Mutex::new(Breaker::default()),
        };
        client.record(Some(&JevError::Network));
        client.record(Some(&JevError::Network));
        assert!(client.breaker().paused_until.is_none());
        client.record(None);
        for _ in 0..3 {
            client.record(Some(&JevError::Timeout));
        }
        assert!(client.breaker().paused_until.is_some());

        let client = JevClient {
            breaker: Mutex::new(Breaker::default()),
            ..client
        };
        client.record(Some(&JevError::Auth { status: 401 }));
        assert!(client.breaker().paused_until.is_some());
    }
}
