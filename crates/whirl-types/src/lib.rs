//! The value vocabulary that the `.whirl` language and the check engine
//! share (SPEC 3.1, 9.3-9.6).
//!
//! This crate owns the typed [`Value`] and its [`ValueType`], exact JSON
//! [`Number`]s, the static types that `whirl check` sees before a run
//! ([`StaticType`], [`FilterKind`], [`PredicateKind`]), the keyword tables
//! that name filters and predicates, and the bytes-literal helpers. It
//! holds data and `Display` only: no filter runs and no predicate is
//! tested here, and it depends on no other Whirl crate, so the language
//! crate builds without libxml2.

mod bytes;
mod number;
mod types;
mod value;

pub use bytes::{bytes_literal, is_bytes_literal_shape};
pub use number::Number;
pub use types::{
    COMPARE_KEYWORDS, FILTER_KEYWORDS, FilterKind, PredicateKind, StaticType, WORD_PREDICATES,
};
pub use value::{Value, ValueType, quote};
