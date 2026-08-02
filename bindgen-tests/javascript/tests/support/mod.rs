//! Shared JavaScript bindgen fixtures and helpers.
//!
//! Integration test entrypoints live in their dedicated target files; this
//! module deliberately contains no `#[test]` functions.

mod composite;
mod core;

pub use composite::*;
pub use core::*;
