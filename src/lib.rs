#![feature(pin_macro)]

pub mod local;
mod scoped_spawn;
mod spawn_scope;

pub use scoped_spawn::*;
pub use spawn_scope::*;
