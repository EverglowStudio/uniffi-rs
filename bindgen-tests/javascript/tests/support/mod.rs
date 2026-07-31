//! Shared JavaScript bindgen fixtures and helpers.
//!
//! Integration test entrypoints live in their dedicated target files; this
//! module deliberately contains no `#[test]` functions.

mod core;

pub use core::*;
