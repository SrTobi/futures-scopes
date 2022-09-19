#![warn(missing_docs)]

pub mod local;
pub mod relay;
mod scoped_spawn;
mod spawn_scope;

pub use scoped_spawn::*;
pub use spawn_scope::*;
