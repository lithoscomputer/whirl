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

use lithos_llm::catalog::{Catalog, CatalogError};
use lithos_llm::client::ClientBuildError;
use lithos_llm::credentials::ConventionalCredentials;
use lithos_llm::middleware::{CallContext, RetryMiddleware, RetryPolicy};
use lithos_llm::resolver::{AvailableProviders, CatalogResolver, ModelResolver as _};
use lithos_llm::types::{ErrorKind, Usage};
use lithos_llm::{Client, Request};

use crate::run::act::decision::{ActInference, inference_schema};

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

    /// True when the selector names a model the client can route.
    pub(crate) fn knows(&self, selector: &str) -> bool {
        let Some(catalog) = &self.catalog else {
            return true;
        };
        let Ok(request) = Request::builder().model(selector).user("-").build() else {
            return false;
        };
        CatalogResolver
            .resolve(&request, catalog, &AvailableProviders::all(catalog))
            .is_ok()
    }
}

/// One model answer: the parsed decision or why it did not parse, and
/// what the call used.
#[derive(Debug)]
pub(crate) struct ModelReply {
    pub(crate) answer: Result<ActInference, serde_json::Error>,
    pub(crate) usage:  Usage,
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
        let selector = if self.endpoint {
            format!("{ENDPOINT_PROVIDER}/{model}")
        } else {
            model.to_owned()
        };
        let request = Request::builder()
            .model(selector)
            .system(system)
            .user(user)
            .build()
            .map_err(|source| {
                lithos_llm::Error::new(ErrorKind::InvalidRequest, "the ACT request is invalid")
                    .with_source(source)
            })?;
        let mut context = CallContext::new();
        context.set_deadline(deadline);
        let completion = self
            .client
            .complete_object_with_context(request, "Act", inference_schema(), context)
            .await?;
        Ok(ModelReply {
            answer: serde_json::from_value(completion.object),
            usage:  completion.response.usage_with_cost(),
        })
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
    fn the_builtin_catalog_knows_listed_models() {
        let models = builtin();
        assert!(models.knows("anthropic/claude-sonnet-5"));
        assert!(!models.knows("nobody/claude-sonnet-5"));
        assert!(!models.knows("anthropic/"));
    }

    #[test]
    fn endpoint_mode_accepts_any_model() {
        let models = ModelCatalog { catalog: None };
        assert!(models.knows("gpt-test"));
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
