use crate::ScopedSpawn;

/// A trait for scopes that support spawning futures of non-static lifetime.
///
/// # Dropping
///
/// The general contract is, that when a scope is dropped,
/// it will immediately drop all futures that where spawned on it.
/// Scopes may offer methods to wait until all tasks have been processed.
/// See for example [`RelayScope::until_empty`].
///
/// [`RelayScope::until_empty`]: crate::relay::RelayScope::until_empty
pub trait SpawnScope<'sc, T> {
    /// The type of the spawner used by this scope.
    type Spawner: ScopedSpawn<'sc, T>;

    /// Creates a new spawner, that spawns new futures of lifetime `'sc` to this scope.
    fn spawner(&self) -> Self::Spawner;
}
