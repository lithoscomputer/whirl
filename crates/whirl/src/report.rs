//! Reporting: the shared run report model and the report renderers.

pub(crate) mod aggregate;
pub(crate) mod console;
#[cfg(test)]
mod fixture;
pub(crate) mod html;
pub(crate) mod json;
pub(crate) mod junit;
pub(crate) mod metadata;
pub(crate) mod model;
