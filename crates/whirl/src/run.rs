//! The runner's infrastructure: the shim process client, variables and
//! secret masking, and artifact directory mapping. Flow execution itself
//! builds on these modules.

pub(crate) mod artifacts;
pub(crate) mod flow;
pub(crate) mod runner;
pub(crate) mod shim;
pub(crate) mod vars;
