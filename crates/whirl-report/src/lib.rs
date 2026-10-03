//! Reporting (SPEC 14): the shared run report model and the report
//! renderers.
//!
//! The runner fills a [`model::RunReport`] and the renderers turn it into
//! console text, a versioned JSON document, JUnit XML, or a standalone
//! HTML page. Every string in the model arrives already masked (SPEC 11);
//! this crate never sees a secret and never talks to the runner.

pub mod aggregate;
pub mod console;
#[cfg(test)]
mod fixture;
pub mod html;
pub mod json;
pub mod junit;
mod metadata;
pub mod model;

pub use metadata::{FileMetadata, ReportMetadata};
