//! An extension to [futures-rs](https://github.com/rust-lang/futures-rs) that offers scopes.
//! Scopes can be used to spawn non-static futures that can reference variables on the stack
//! that were created before the scope was created.
//!
//! There are currently two different Scope types:
//! - For spawning local non-send/sync futures, use [`LocalScope`](local::LocalScope).
//! - For spawning futures on multiple, underlying spawns, use [`RelayScope`](relay::RelayScope).
//!
//! # Features
//!
//! The following features are available on the crate:
//!
//! - `local`: Enables [`LocalScope`](local::LocalScope).
//! - `relay`: Enables [`RelayScope`](relay::RelayScope).
//!
//! By default both features are enabled.
//! To only use one of the features, disable the default features and enable the feature you want to use.
//! For example:
//!
//! ```toml
//! [dependencies]
//! futures-scopes = {version = "...", default-features = false, features = ["local"]}
//! ```

#![warn(missing_docs)]

/// Provides [`LocalScope`](local::LocalScope), which can spawn non-send/sync futures of non-static lifetime.
#[cfg(feature = "local")]
pub mod local;

/// Provides [`RelayScope`](relay::RelayScope), which can spawn send/sync futures of non-static lifetime.
#[cfg(feature = "relay")]
pub mod relay;

mod scoped_spawn;
mod spawn_scope;

pub use scoped_spawn::*;
pub use spawn_scope::*;
