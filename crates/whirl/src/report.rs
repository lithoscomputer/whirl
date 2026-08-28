//! Reporting: the shared run report model and the report renderers.

pub mod console;
#[cfg(test)]
mod fixture;
pub mod json;
pub mod junit;
pub mod model;
