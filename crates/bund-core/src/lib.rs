//! A UI-independent, resumable coding-agent engine. Implement adapters against
//! the public traits; neither terminal nor provider wire types belong here.
mod context;
mod engine;
mod types;

pub use context::*;
pub use engine::*;
pub use types::*;
