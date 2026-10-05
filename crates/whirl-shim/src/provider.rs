//! Where a run's browsers come from. A [`BrowserProvider`] hands the
//! runner one [`BrowserLease`] per worker slot. The lease's
//! [`BrowserSource`] tells the shim to launch a browser beside itself or
//! to attach to a running one over CDP. The shim knows only those two
//! cases; the provider owns everything else, such as a hosted service's
//! API and its billing.

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

use whirl_lang::ast::BrowserKind;

/// How the shim gets the browser for a flow (`startFlow`, protocol
/// section 3).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum BrowserSource {
    /// Launch the flow's engine beside the shim.
    #[default]
    Launch,
    /// Attach to a running Chromium through its CDP websocket endpoint.
    Attach { cdp_endpoint: String },
}

/// Identifies one run to a provider, so it can tag what it creates and
/// sweep what a dead run left behind.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunId(String);

impl RunId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a provider can do. The runner checks it before it takes any
/// lease, so an impossible run fails before a hosted browser costs money.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Capabilities {
    /// The engines a lease can run.
    pub engines:       &'static [BrowserKind],
    /// The browser shares the runner's network, so an `app-url` on
    /// localhost reaches the app under test.
    pub local_network: bool,
}

impl Capabilities {
    pub fn supports(&self, engine: BrowserKind) -> bool {
        self.engines.contains(&engine)
    }
}

/// What the runner asks for in one lease.
#[derive(Clone, Copy, Debug)]
pub struct LeaseRequest<'a> {
    pub run:    &'a RunId,
    /// The worker slot the browser serves, from 0.
    pub slot:   usize,
    /// `--headed`: the user wants to watch the browser.
    pub headed: bool,
}

/// One browser that a provider lent to one worker slot. Give it back
/// with [`BrowserProvider::release`], which consumes it.
#[derive(Debug, Eq, PartialEq)]
pub struct BrowserLease {
    pub source:    BrowserSource,
    /// The provider's handle, for release and for logs.
    pub id:        String,
    /// A page where a person can watch a hosted, headed browser.
    pub live_view: Option<String>,
}

/// A provider failure. Every failure stops the run before or after its
/// flows; no flow handles one.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("{provider} browsers need {env}; set it in the environment")]
    MissingCredential {
        provider: &'static str,
        env:      &'static str,
    },
    #[error("{provider}: {action} failed")]
    Request {
        provider: &'static str,
        /// What the provider was doing, such as `creating a browser`.
        action:   &'static str,
        #[source]
        source:   Box<dyn Error + Send + Sync>,
    },
}

/// The future a provider method returns. It is boxed, so the trait is
/// dyn-compatible.
pub type ProviderFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, ProviderError>> + Send + 'a>>;

/// Lends browsers to a run, one per worker slot.
///
/// The runner calls [`capabilities`](Self::capabilities) first, then
/// [`acquire`](Self::acquire) once per slot, concurrently. At the end of
/// the run it calls [`release`](Self::release) for each lease it holds,
/// then [`sweep`](Self::sweep) once. A run that dies skips both; an
/// implementation that creates paid resources must make them expire on
/// their own.
pub trait BrowserProvider: fmt::Debug + Send + Sync {
    /// The name messages and logs give this provider, such as `kernel`.
    fn name(&self) -> &'static str;

    fn capabilities(&self) -> Capabilities;

    fn acquire<'a>(&'a self, request: LeaseRequest<'a>) -> ProviderFuture<'a, BrowserLease>;

    /// Gives the browser back. A browser that is already gone counts as
    /// released.
    fn release(&self, lease: BrowserLease) -> ProviderFuture<'_, ()>;

    /// Removes everything still tagged with `run`, whether or not the
    /// runner holds its lease: a lease that arrived after a failure, or
    /// one a crashed run left behind.
    fn sweep<'a>(&'a self, run: &'a RunId) -> ProviderFuture<'a, ()> {
        let _ = run;
        Box::pin(async { Ok(()) })
    }
}

/// Launches each flow's browser beside the shim. Leases cost nothing and
/// release does nothing.
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalBrowsers;

impl BrowserProvider for LocalBrowsers {
    fn name(&self) -> &'static str {
        "local"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            engines:       &[
                BrowserKind::Chromium,
                BrowserKind::Firefox,
                BrowserKind::Webkit,
            ],
            local_network: true,
        }
    }

    fn acquire<'a>(&'a self, request: LeaseRequest<'a>) -> ProviderFuture<'a, BrowserLease> {
        Box::pin(async move {
            Ok(BrowserLease {
                source:    BrowserSource::Launch,
                id:        format!("local-{}", request.slot),
                live_view: None,
            })
        })
    }

    fn release(&self, _lease: BrowserLease) -> ProviderFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}
