//! The runner's infrastructure: variables and secret masking, the AI
//! cache, and artifact directory mapping. Flow execution itself builds
//! on these modules and on the shim client in `whirl-shim`.

pub(crate) mod artifacts;
pub(crate) mod cache;
pub(crate) mod extract;
pub(crate) mod flow;
pub(crate) mod runner;
pub(crate) mod vars;
