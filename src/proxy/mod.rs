//! Reverse-proxy primitives: model routing, path rewriting, and forwarding.

pub mod decode;
pub mod forward;
pub mod model;
pub mod prepare;
pub(crate) mod response_model;
pub mod rewrite;
