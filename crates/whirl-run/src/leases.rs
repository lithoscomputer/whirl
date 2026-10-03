//! The run's browser leases: one per worker slot, taken from the run's
//! [`BrowserProvider`] before any flow starts and given back when the run
//! ends. Owning them in one place keeps the release and sweep on every
//! path out of a run.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{fmt, mem, process};

use tokio::task::JoinSet;
use tokio::time::timeout;
use tracing::warn;
use whirl_lang::ast::BrowserKind;
use whirl_shim::provider::{
    BrowserLease, BrowserProvider, BrowserSource, LeaseRequest, ProviderError, RunId,
};

/// How long the release and sweep may take at the end of a run. The
/// reports are complete by then, and a provider that creates paid
/// resources makes them expire on its own (the [`BrowserProvider`]
/// contract), so the run stops waiting.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(20);

/// A run that its browser provider cannot serve.
#[derive(Debug, thiserror::Error)]
pub enum LeaseError {
    /// A usage error (exit 4): a flow needs an engine the provider lacks.
    #[error("the {provider} browser provider runs {supported} only; {path} needs {engine}")]
    Engine {
        provider:  &'static str,
        supported: EngineList,
        engine:    &'static str,
        path:      String,
    },
    /// A runtime error (exit 3).
    #[error(transparent)]
    Provider(#[from] ProviderError),
}

/// The engines a provider supports, for messages.
#[derive(Debug)]
pub struct EngineList(&'static [BrowserKind]);

impl fmt::Display for EngineList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.0.iter().map(|engine| engine.as_str()).collect();
        f.write_str(&names.join(", "))
    }
}

/// Checks that the provider can run every engine the files name, before
/// any lease is taken. `engines` pairs each file path with its literal
/// `browser` option; an interpolated option fails at `startFlow` instead.
pub(crate) fn check_engines<'a>(
    provider: &dyn BrowserProvider,
    engines: impl IntoIterator<Item = (&'a str, BrowserKind)>,
) -> Result<(), LeaseError> {
    let capabilities = provider.capabilities();
    for (path, engine) in engines {
        if !capabilities.supports(engine) {
            return Err(LeaseError::Engine {
                provider:  provider.name(),
                supported: EngineList(capabilities.engines),
                engine:    engine.as_str(),
                path:      path.to_owned(),
            });
        }
    }
    Ok(())
}

/// The leases of one run, indexed by worker slot.
#[must_use = "call `release` to give the browsers back"]
pub(crate) struct RunLeases {
    provider: Arc<dyn BrowserProvider>,
    run:      RunId,
    sources:  Vec<BrowserSource>,
    leases:   Vec<BrowserLease>,
}

impl RunLeases {
    /// Takes one lease per worker slot, concurrently. If any lease fails,
    /// gives back the ones that arrived and sweeps the run.
    pub(crate) async fn acquire(
        provider: Arc<dyn BrowserProvider>,
        slots: usize,
        headed: bool,
    ) -> Result<Self, LeaseError> {
        let run = new_run_id();
        let mut tasks = JoinSet::new();
        for slot in 0..slots {
            let provider = Arc::clone(&provider);
            let run = run.clone();
            tasks.spawn(async move {
                let request = LeaseRequest {
                    run: &run,
                    slot,
                    headed,
                };
                (slot, provider.acquire(request).await)
            });
        }
        let mut arrived: Vec<Option<BrowserLease>> = (0..slots).map(|_| None).collect();
        let mut failure = None;
        while let Some(joined) = tasks.join_next().await {
            let (slot, result) = joined.expect("a lease task does not panic");
            match result {
                Ok(lease) => arrived[slot] = Some(lease),
                Err(error) => failure = Some(error),
            }
        }
        let leases: Vec<BrowserLease> = arrived.into_iter().flatten().collect();
        let mut run_leases = Self {
            provider,
            run,
            sources: leases.iter().map(|lease| lease.source.clone()).collect(),
            leases,
        };
        if let Some(error) = failure {
            run_leases.release().await;
            return Err(error.into());
        }
        Ok(run_leases)
    }

    /// The browser source of each worker slot, in slot order.
    pub(crate) fn sources(&self) -> Arc<[BrowserSource]> {
        Arc::from(self.sources.as_slice())
    }

    /// Gives every lease back, then sweeps the run. Best effort within
    /// [`RELEASE_TIMEOUT`]; failures are logged, not returned, because the
    /// run's result does not depend on them.
    pub(crate) async fn release(&mut self) {
        let provider = Arc::clone(&self.provider);
        let leases = mem::take(&mut self.leases);
        let run = &self.run;
        let work = async {
            for lease in leases {
                let id = lease.id.clone();
                if let Err(error) = provider.release(lease).await {
                    warn!(provider = provider.name(), lease = %id, %error, "could not release a browser");
                }
            }
            if let Err(error) = provider.sweep(run).await {
                warn!(provider = provider.name(), %error, "could not sweep the run's browsers");
            }
        };
        if timeout(RELEASE_TIMEOUT, work).await.is_err() {
            warn!(
                provider = self.provider.name(),
                "releasing browsers timed out; the provider's own expiry reclaims them"
            );
        }
    }
}

/// A run id unique enough to tag one run's resources: the process id and
/// the start time in milliseconds.
fn new_run_id() -> RunId {
    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis());
    RunId::new(format!("{}-{started}", process::id()))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use whirl_shim::provider::{Capabilities, LocalBrowsers, ProviderFuture};

    use super::*;

    /// Records calls and fails the slots it is told to fail.
    #[derive(Debug, Default)]
    struct FakeProvider {
        fail_slot: Option<usize>,
        released:  Mutex<Vec<String>>,
        swept:     Mutex<Vec<String>>,
    }

    impl BrowserProvider for FakeProvider {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                engines:       &[BrowserKind::Chromium],
                local_network: false,
            }
        }

        fn acquire<'a>(&'a self, request: LeaseRequest<'a>) -> ProviderFuture<'a, BrowserLease> {
            Box::pin(async move {
                if self.fail_slot == Some(request.slot) {
                    return Err(ProviderError::MissingCredential {
                        provider: "fake",
                        env:      "FAKE_KEY",
                    });
                }
                Ok(BrowserLease {
                    source:    BrowserSource::Attach {
                        cdp_endpoint: format!("ws://fake/{}", request.slot),
                    },
                    id:        format!("lease-{}", request.slot),
                    live_view: None,
                })
            })
        }

        fn release(&self, lease: BrowserLease) -> ProviderFuture<'_, ()> {
            self.released.lock().expect("not poisoned").push(lease.id);
            Box::pin(async { Ok(()) })
        }

        fn sweep<'a>(&'a self, run: &'a RunId) -> ProviderFuture<'a, ()> {
            self.swept
                .lock()
                .expect("not poisoned")
                .push(run.to_string());
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn each_slot_gets_its_own_source_in_slot_order() {
        let provider = Arc::new(FakeProvider::default());
        let mut leases = RunLeases::acquire(provider.clone(), 3, false)
            .await
            .expect("every lease succeeds");
        let endpoints: Vec<_> = leases
            .sources()
            .iter()
            .map(|source| match source {
                BrowserSource::Attach { cdp_endpoint } => cdp_endpoint.clone(),
                BrowserSource::Launch => String::new(),
            })
            .collect();
        assert_eq!(endpoints, ["ws://fake/0", "ws://fake/1", "ws://fake/2"]);
        leases.release().await;
        let mut released = provider.released.lock().expect("not poisoned").clone();
        released.sort();
        assert_eq!(released, ["lease-0", "lease-1", "lease-2"]);
        assert_eq!(provider.swept.lock().expect("not poisoned").len(), 1);
    }

    #[tokio::test]
    async fn a_failed_lease_gives_back_the_others_and_sweeps() {
        let provider = Arc::new(FakeProvider {
            fail_slot: Some(1),
            ..FakeProvider::default()
        });
        let error = RunLeases::acquire(provider.clone(), 3, false)
            .await
            .err()
            .expect("slot 1 fails");
        assert!(matches!(error, LeaseError::Provider(_)));
        let mut released = provider.released.lock().expect("not poisoned").clone();
        released.sort();
        assert_eq!(released, ["lease-0", "lease-2"]);
        assert_eq!(provider.swept.lock().expect("not poisoned").len(), 1);
    }

    #[test]
    fn an_engine_the_provider_lacks_fails_before_any_lease() {
        let error = check_engines(&FakeProvider::default(), [(
            "a.whirl",
            BrowserKind::Firefox,
        )])
        .expect_err("fake runs chromium only");
        assert_eq!(
            error.to_string(),
            "the fake browser provider runs chromium only; a.whirl needs firefox"
        );
        assert!(check_engines(&LocalBrowsers, [("a.whirl", BrowserKind::Webkit)]).is_ok());
    }
}
