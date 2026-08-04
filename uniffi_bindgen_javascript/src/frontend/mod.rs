//! Canonical JavaScript frontend.

mod ir;
mod normalize;

pub use ir::*;
pub use normalize::{normalize, BindingInput, FrontendError};
