//! The language model client behind `ACT` (SPEC 7.4, 13), built on
//! `lithos-llm`.
//!
//! Without `WHIRL_LLM_ENDPOINT`, the `model` option selects a model from
//! lithos-llm's built-in catalog, and credentials come from the usual
//! provider variables such as `ANTHROPIC_API_KEY`. With it, every call goes
//! to that one OpenAI-compatible server, and `WHIRL_LLM_API_KEY`, when set,
//! is its bearer token.

use std::env;
use std::time::Instant;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use lithos_llm::catalog::{Catalog, CatalogError, Support};
use lithos_llm::client::ClientBuildError;
use lithos_llm::credentials::{ConventionalCredentials, CredentialProvider as _};
use lithos_llm::middleware::{CallContext, RetryMiddleware, RetryPolicy};
use lithos_llm::resolver::{AvailableProviders, CatalogResolver, ModelResolver as _};
use lithos_llm::types::{ContentPart, ErrorKind, ImageContent, MediaSource, Message, Role, Usage};
use lithos_llm::{Client, Request, StructuredCompletion};
use serde::Deserialize;
use serde_json::{Value as Json, json};

use crate::lang::lint::ModelFacts;
use crate::run::act::decision::{ActInference, inference_schema};
use crate::run::act::prompt;

/// Sends every model call to one OpenAI-compatible server (SPEC 13).
pub(crate) const ENDPOINT_ENV: &str = "WHIRL_LLM_ENDPOINT";
/// The endpoint's bearer token. lithos-llm derives this name from the
/// endpoint provider's id.
pub(crate) const API_KEY_ENV: &str = "WHIRL_LLM_API_KEY";
const ENDPOINT_PROVIDER: &str = "whirl-llm";

/// The endpoint from the environment; an empty value counts as unset.
fn endpoint_from_env() -> Option<String> {
    env::var(ENDPOINT_ENV).ok().filter(|url| !url.is_empty())
}

/// A failure to build the model client. The run stops with a runtime
/// error (exit 3).
#[derive(Debug, thiserror::Error)]
pub(crate) enum ModelSetupError {
    #[error("the model catalog could not be built")]
    Catalog(#[source] Box<CatalogError>),
    #[error("the language model client could not be built")]
    Client(#[source] Box<ClientBuildError>),
}

/// Which models `whirl check` accepts in the `model` option.
#[derive(Debug)]
pub(crate) struct ModelCatalog {
    /// `None` in endpoint mode, where the server decides.
    catalog: Option<Catalog>,
}

impl ModelCatalog {
    pub(crate) fn from_env() -> Self {
        let catalog = endpoint_from_env().is_none().then(|| {
            Catalog::builder()
                .with_builtin()
                .build()
                .expect("lithos-llm's built-in catalog is valid")
        });
        Self { catalog }
    }

    /// What the catalog says about the model the selector names. In
    /// endpoint mode every model is known and counts as accepting images.
    pub(crate) fn facts(&self, selector: &str) -> ModelFacts {
        let Some(catalog) = &self.catalog else {
            return ModelFacts::Known { images: Some(true) };
        };
        let Ok(request) = Request::builder().model(selector).user("-").build() else {
            return ModelFacts::Unknown;
        };
        match CatalogResolver.resolve(&request, catalog, &AvailableProviders::all(catalog)) {
            Ok(route) => ModelFacts::Known {
                images: match route.model().capabilities().images() {
                    Support::Supported => Some(true),
                    Support::Unsupported => Some(false),
                    _ => None,
                },
            },
            Err(_) => ModelFacts::Unknown,
        }
    }
}

/// One model answer: the parsed decision or why it did not parse, and
/// what the call used.
#[derive(Debug)]
pub(crate) struct ModelReply {
    pub(crate) answer: Result<ActInference, serde_json::Error>,
    pub(crate) usage:  Usage,
}

/// One element that an `ai:` target call found (SPEC 6.3).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FoundElement {
    pub(crate) element_id:  String,
    pub(crate) description: String,
}

/// Every element an `ai:` target call found.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub(crate) struct TargetAnswer {
    pub(crate) elements: Vec<FoundElement>,
}

/// One `ai:` target answer, and what the call used.
#[derive(Debug)]
pub(crate) struct TargetReply {
    pub(crate) answer: Result<TargetAnswer, serde_json::Error>,
    pub(crate) usage:  Usage,
}

/// An `EXTRACT` answer (SPEC 7.6): the object, the raw text it came
/// from, which keeps exact numbers, and what the call used.
#[derive(Debug)]
pub(crate) struct ExtractReply {
    pub(crate) object: Json,
    pub(crate) text:   String,
    pub(crate) usage:  Usage,
}

/// A `JUDGE` verdict (SPEC 9.8).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Verdict {
    Yes,
    No,
    Unsure,
}

impl Verdict {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Yes => "yes",
            Self::No => "no",
            Self::Unsure => "unsure",
        }
    }
}

/// A `JUDGE` answer.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub(crate) struct JudgeAnswer {
    pub(crate) verdict: Verdict,
    pub(crate) reason:  String,
}

/// One `JUDGE` answer, and what the call used.
#[derive(Debug)]
pub(crate) struct JudgeReply {
    pub(crate) answer: Result<JudgeAnswer, serde_json::Error>,
    pub(crate) usage:  Usage,
}

/// The text an instruction wants typed, and what the call used.
#[derive(Debug)]
pub(crate) struct TextReply {
    pub(crate) text:  Option<String>,
    pub(crate) usage: Usage,
}

/// The run's model client, shared by every flow.
#[derive(Debug)]
pub(crate) struct ModelClient {
    client:   Client,
    endpoint: bool,
}

impl ModelClient {
    pub(crate) fn from_env() -> Result<Self, ModelSetupError> {
        let endpoint = endpoint_from_env();
        let catalog = match &endpoint {
            Some(url) => endpoint_catalog(url),
            None => Catalog::builder().with_builtin().build(),
        }
        .map_err(|source| ModelSetupError::Catalog(Box::new(source)))?;
        let build = Client::builder()
            .catalog(catalog)
            .credentials(ConventionalCredentials::new())
            .application("whirl")
            .middleware(RetryMiddleware::new(RetryPolicy::default()))
            .build()
            .map_err(|source| ModelSetupError::Client(Box::new(source)))?;
        Ok(Self {
            client:   build.client,
            endpoint: endpoint.is_some(),
        })
    }

    /// Asks the model for one act decision. The call, with its retries,
    /// ends by `deadline`.
    pub(crate) async fn plan(
        &self,
        model: &str,
        system: &str,
        user: &str,
        deadline: Instant,
    ) -> Result<ModelReply, lithos_llm::Error> {
        let (object, usage) = self
            .structured(model, system, user, "Act", inference_schema(), deadline)
            .await?;
        Ok(ModelReply {
            answer: serde_json::from_value(object),
            usage,
        })
    }

    /// Asks the model for every element that an `ai:` description names
    /// (SPEC 6.3). The call, with its retries, ends by `deadline`.
    pub(crate) async fn find_elements(
        &self,
        model: &str,
        user: &str,
        deadline: Instant,
    ) -> Result<TargetReply, lithos_llm::Error> {
        let schema = json!({
            "type": "object",
            "properties": {
                "elements": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "elementId": {
                                "type": "string",
                                "description": "The element's ref, copied exactly from the tree, such as e12"
                            },
                            "description": {
                                "type": "string",
                                "description": "A few words that describe the element"
                            }
                        },
                        "required": ["elementId", "description"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["elements"],
            "additionalProperties": false
        });
        let (object, usage) = self
            .structured(
                model,
                &prompt::target_system_prompt(),
                user,
                "AiTarget",
                schema,
                deadline,
            )
            .await?;
        Ok(TargetReply {
            answer: serde_json::from_value(object),
            usage,
        })
    }

    /// Asks the model whether a claim holds, from the text and a PNG
    /// screenshot (SPEC 9.8).
    pub(crate) async fn judge(
        &self,
        model: &str,
        text: &str,
        png: &[u8],
        deadline: Instant,
    ) -> Result<JudgeReply, lithos_llm::Error> {
        let schema = json!({
            "type": "object",
            "properties": {
                "verdict": {"type": "string", "enum": ["yes", "no", "unsure"]},
                "reason": {"type": "string", "description": "One or two sentences on the evidence"}
            },
            "required": ["verdict", "reason"],
            "additionalProperties": false
        });
        let image = ImageContent::new(MediaSource::base64(STANDARD.encode(png), "image/png"));
        let message = Message::new(Role::User, [
            ContentPart::Text {
                text: text.to_owned(),
            },
            ContentPart::Image(image),
        ]);
        let request = Request::builder()
            .model(self.selector(model))
            .system(prompt::judge_system_prompt())
            .message(message)
            .build()
            .map_err(|source| {
                lithos_llm::Error::new(ErrorKind::InvalidRequest, "the JUDGE request is invalid")
                    .with_source(source)
            })?;
        let mut context = CallContext::new();
        context.set_deadline(deadline);
        let completion = self
            .client
            .complete_object_with_context(request, "Judge", schema, context)
            .await?;
        Ok(JudgeReply {
            answer: serde_json::from_value(completion.object),
            usage:  completion.response.usage_with_cost(),
        })
    }

    /// False when the model's provider has no credentials in the
    /// environment (SPEC 9.8). An endpoint, and a model the catalog does
    /// not know, count as ready: their calls report their own errors.
    pub(crate) async fn has_credentials(&self, model: &str) -> bool {
        if self.endpoint {
            return true;
        }
        let Ok(request) = Request::builder().model(model).user("-").build() else {
            return true;
        };
        let Ok(route) = self.client.resolve_route(&request) else {
            return true;
        };
        let Ok(provider) = self
            .client
            .catalog()
            .provider(route.handle().provider().as_str())
        else {
            return true;
        };
        ConventionalCredentials::new().is_configured(provider).await
    }

    fn selector(&self, model: &str) -> String {
        if self.endpoint {
            format!("{ENDPOINT_PROVIDER}/{model}")
        } else {
            model.to_owned()
        }
    }

    /// Asks the model to read a value in the shape of `schema` (SPEC 7.6).
    pub(crate) async fn extract(
        &self,
        model: &str,
        user: &str,
        schema: Json,
        deadline: Instant,
    ) -> Result<ExtractReply, lithos_llm::Error> {
        let completion = self
            .structured_completion(
                model,
                &prompt::extract_system_prompt(),
                user,
                "Extract",
                schema,
                deadline,
            )
            .await?;
        Ok(ExtractReply {
            text:   completion.response.text(),
            usage:  completion.response.usage_with_cost(),
            object: completion.object,
        })
    }

    /// Asks the model for the text an instruction wants typed, copied from
    /// the instruction; the page is not sent. `None` when the instruction
    /// does not say.
    pub(crate) async fn text_argument(
        &self,
        model: &str,
        instruction: &str,
        placeholders: &[String],
        deadline: Instant,
    ) -> Result<TextReply, lithos_llm::Error> {
        let schema = json!({
            "type": "object",
            "properties": {
                "text": {
                    "anyOf": [{"type": "string"}, {"type": "null"}],
                    "description": "The exact text the instruction wants typed, copied verbatim from the instruction, or the %placeholder% that stands for it. Null when the instruction does not say what to type."
                }
            },
            "required": ["text"],
            "additionalProperties": false
        });
        let (object, usage) = self
            .structured(
                model,
                &prompt::text_argument_system_prompt(),
                &prompt::text_argument_message(instruction, placeholders),
                "ActTextArgument",
                schema,
                deadline,
            )
            .await?;
        let text = object.get("text").and_then(Json::as_str).map(str::to_owned);
        Ok(TextReply { text, usage })
    }

    /// One structured-output call, with its usage and cost.
    async fn structured(
        &self,
        model: &str,
        system: &str,
        user: &str,
        name: &str,
        schema: Json,
        deadline: Instant,
    ) -> Result<(Json, Usage), lithos_llm::Error> {
        let completion = self
            .structured_completion(model, system, user, name, schema, deadline)
            .await?;
        Ok((completion.object, completion.response.usage_with_cost()))
    }

    /// One structured-output call's whole completion.
    async fn structured_completion(
        &self,
        model: &str,
        system: &str,
        user: &str,
        name: &str,
        schema: Json,
        deadline: Instant,
    ) -> Result<StructuredCompletion, lithos_llm::Error> {
        let request = Request::builder()
            .model(self.selector(model))
            .system(system)
            .user(user)
            .build()
            .map_err(|source| {
                lithos_llm::Error::new(ErrorKind::InvalidRequest, "the ACT request is invalid")
                    .with_source(source)
            })?;
        let mut context = CallContext::new();
        context.set_deadline(deadline);
        self.client
            .complete_object_with_context(request, name, schema, context)
            .await
    }
}

/// A catalog with one OpenAI-compatible provider that accepts any model
/// name. Without an API key it sends no credentials.
fn endpoint_catalog(url: &str) -> Result<Catalog, CatalogError> {
    let auth = if env::var(API_KEY_ENV).is_ok_and(|key| !key.is_empty()) {
        "bearer"
    } else {
        "none"
    };
    let base_url = serde_json::to_string(url).expect("a string always serializes");
    let overlay = format!(
        "schema_version = 1\n\n\
         [providers.{ENDPOINT_PROVIDER}]\n\
         display_name = \"{ENDPOINT_ENV}\"\n\
         codecs = [\"openai-chat\"]\n\
         base_url = {base_url}\n\
         allow_passthrough = true\n\
         auth = {{ type = \"{auth}\" }}\n"
    );
    Catalog::builder()
        .toml_layer(ENDPOINT_ENV, &overlay)?
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin() -> ModelCatalog {
        ModelCatalog {
            catalog: Some(
                Catalog::builder()
                    .with_builtin()
                    .build()
                    .expect("the built-in catalog is valid"),
            ),
        }
    }

    #[test]
    fn the_builtin_catalog_knows_listed_models_and_their_image_support() {
        let models = builtin();
        assert_eq!(
            models.facts("anthropic/claude-sonnet-5"),
            ModelFacts::Known { images: Some(true) }
        );
        assert_eq!(
            models.facts("deepseek/deepseek-v4-flash"),
            ModelFacts::Known {
                images: Some(false),
            }
        );
        assert_eq!(models.facts("nobody/claude-sonnet-5"), ModelFacts::Unknown);
        assert_eq!(models.facts("anthropic/"), ModelFacts::Unknown);
    }

    #[test]
    fn endpoint_mode_accepts_any_model() {
        let models = ModelCatalog { catalog: None };
        assert_eq!(models.facts("gpt-test"), ModelFacts::Known {
            images: Some(true),
        });
    }

    #[test]
    fn the_endpoint_catalog_routes_any_model_to_the_endpoint() {
        let catalog = endpoint_catalog("http://127.0.0.1:3000").expect("valid overlay");
        let request = Request::builder()
            .model("whirl-llm/anthropic/claude-sonnet-5")
            .user("-")
            .build()
            .expect("valid request");
        let route = CatalogResolver
            .resolve(&request, &catalog, &AvailableProviders::all(&catalog))
            .expect("the endpoint accepts any model");
        assert_eq!(route.api_model(), "anthropic/claude-sonnet-5");
        assert_eq!(route.provider().base_url(), "http://127.0.0.1:3000");
    }
}
