//! Kernel hosted Chromium browsers as a [`BrowserProvider`]. Each lease
//! is one Kernel browser session, tagged with the run and the slot so a
//! sweep can delete what a failed or crashed run left behind. Kernel's
//! idle timeout reclaims a browser that no sweep reaches.

use std::collections::HashMap;

use kernel::types::{BrowserCreateParams, BrowserListParams, BrowserListStatus};
use kernel::{Error as KernelError, Kernel};
use tracing::{info, warn};
use whirl_lang::ast::BrowserKind;
use whirl_shim::provider::{
    BrowserLease, BrowserProvider, BrowserSource, Capabilities, LeaseRequest, ProviderError,
    ProviderFuture, RunId,
};

const NAME: &str = "kernel";

/// The environment variable that holds the API key, as every Kernel SDK
/// reads it.
const API_KEY_ENV: &str = "KERNEL_API_KEY";

/// Seconds without a CDP connection before Kernel deletes a browser: the
/// backstop when a run dies before it releases. Matches e2e's provider.
const IDLE_TIMEOUT_SECONDS: i64 = 600;

const RUN_TAG: &str = "whirl_run";
const SLOT_TAG: &str = "whirl_slot";

/// Leases Kernel browsers.
#[derive(Clone, Debug)]
pub struct KernelBrowsers {
    client: Kernel,
}

impl KernelBrowsers {
    /// Reads `KERNEL_API_KEY`, and `KERNEL_BASE_URL` when it is set.
    pub fn from_env() -> Result<Self, ProviderError> {
        match Kernel::new() {
            Ok(client) => Ok(Self { client }),
            Err(KernelError::Config(_)) => Err(ProviderError::MissingCredential {
                provider: NAME,
                env:      API_KEY_ENV,
            }),
            Err(error) => Err(request_error("configuring the client", error)),
        }
    }

    async fn delete(&self, session_id: &str) -> Result<(), ProviderError> {
        match self.client.browsers().delete_by_id(session_id).await {
            Ok(()) => Ok(()),
            Err(error) if error.status() == Some(404) => Ok(()),
            Err(error) => Err(request_error("deleting a browser", error)),
        }
    }
}

impl BrowserProvider for KernelBrowsers {
    fn name(&self) -> &'static str {
        NAME
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            engines:       &[BrowserKind::Chromium],
            local_network: false,
        }
    }

    fn acquire<'a>(&'a self, request: LeaseRequest<'a>) -> ProviderFuture<'a, BrowserLease> {
        Box::pin(async move {
            let params = BrowserCreateParams {
                headless: Some(!request.headed),
                timeout_seconds: Some(IDLE_TIMEOUT_SECONDS),
                tags: Some(run_tags(request.run, Some(request.slot))),
                ..BrowserCreateParams::default()
            };
            let browser = self
                .client
                .browsers()
                .create(params)
                .await
                .map_err(|error| request_error("creating a browser", error))?;
            info!(
                slot = request.slot,
                session_id = %browser.session_id,
                "Kernel browser ready"
            );
            Ok(BrowserLease {
                source:    BrowserSource::Attach {
                    cdp_endpoint: browser.cdp_ws_url,
                },
                id:        browser.session_id,
                live_view: browser.browser_live_view_url,
            })
        })
    }

    fn release(&self, lease: BrowserLease) -> ProviderFuture<'_, ()> {
        Box::pin(async move { self.delete(&lease.id).await })
    }

    fn sweep<'a>(&'a self, run: &'a RunId) -> ProviderFuture<'a, ()> {
        Box::pin(async move {
            let params = BrowserListParams {
                status: Some(BrowserListStatus::Active),
                tags: Some(run_tags(run, None)),
                ..BrowserListParams::default()
            };
            let mut page = Some(
                self.client
                    .browsers()
                    .list(params)
                    .await
                    .map_err(|error| request_error("listing browsers", error))?,
            );
            while let Some(current) = page {
                for browser in &current.items {
                    // Keep sweeping past one failure; Kernel's idle
                    // timeout reclaims what this misses.
                    if let Err(error) = self.delete(&browser.session_id).await {
                        warn!(session_id = %browser.session_id, %error, "sweep could not delete a browser");
                    } else {
                        info!(session_id = %browser.session_id, "swept a Kernel browser");
                    }
                }
                page = current
                    .next_page()
                    .await
                    .map_err(|error| request_error("listing browsers", error))?;
            }
            Ok(())
        })
    }
}

fn run_tags(run: &RunId, slot: Option<usize>) -> HashMap<String, String> {
    let mut tags = HashMap::from([(RUN_TAG.to_owned(), run.as_str().to_owned())]);
    if let Some(slot) = slot {
        tags.insert(SLOT_TAG.to_owned(), slot.to_string());
    }
    tags
}

fn request_error(action: &'static str, error: KernelError) -> ProviderError {
    ProviderError::Request {
        provider: NAME,
        action,
        source: Box::new(error),
    }
}
