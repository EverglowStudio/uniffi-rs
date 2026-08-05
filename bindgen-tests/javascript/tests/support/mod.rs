//! Shared JavaScript bindgen fixtures and helpers.
//!
//! Integration test entrypoints live in their dedicated target files; this
//! module deliberately contains no `#[test]` functions.

// Each integration target imports a different subset through `support::*`.
// Keep the shared public surface available without emitting target-specific
// unused re-export warnings.
#![allow(unused_imports)]

mod composite;
mod core;

pub use composite::*;
pub use core::*;
