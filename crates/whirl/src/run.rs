//! The runner's infrastructure: the shim process client, variables and
//! secret masking, and artifact directory mapping. Flow execution itself
//! builds on these modules.

pub mod artifacts;
pub mod flow;
pub mod runner;
pub mod shim;
pub mod vars;
