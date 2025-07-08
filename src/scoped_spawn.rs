use std::future::Future;

use futures::future::{FutureExt, RemoteHandle};
use futures::task::{FutureObj, SpawnError};

/// A scope that can spawn non-static futures
pub trait ScopedSpawn<'sc, T> {
    /// Spawns a task that polls the given future.
    ///
    /// This method returns a [`Result`] that contains a [`SpawnError`] if spawning fails.
    fn spawn_obj_scoped(&self, future: FutureObj<'sc, T>) -> Result<(), SpawnError>;

    /// Determines whether the scope is able to spawn new tasks.
    ///
    /// This method will return `Ok` when the scope is *likely*
    /// (but not guaranteed) to accept a subsequent spawn attempt.
    /// Likewise, an `Err` return means that `spawn` is likely, but
    /// not guaranteed, to yield an error.
    #[inline]
    fn status_scoped(&self) -> Result<(), SpawnError> {
        Ok(())
    }
}

/// Extension trait for `ScopedSpawn`.
pub trait ScopedSpawnExt<'a, T>: ScopedSpawn<'a, T> {
    /// Spawns a task that polls the given future.
    ///
    /// This method returns a [`Result`] that contains a [`SpawnError`] if spawning fails.
    fn spawn_scoped<Fut>(&self, future: Fut) -> Result<(), SpawnError>
    where
        Fut: Future<Output = T> + Send + 'a,
    {
        self.spawn_obj_scoped(FutureObj::new(Box::new(future)))
    }

    /// Spawns a task that polls the given future and returns a handle to its output.
    ///
    /// This method returns a [`Result`] that contains a [`SpawnError`] if spawning fails.
    fn spawn_scoped_with_handle<Fut>(&self, future: Fut) -> Result<RemoteHandle<Fut::Output>, SpawnError>
    where
        Fut: Future + Send + 'a,
        Fut::Output: Send + 'a,
        Self: ScopedSpawnExt<'a, ()>,
    {
        let (remote, handle) = future.remote_handle();
        let _: () = self.spawn_scoped(remote)?;
        Ok(handle)
    }
}

impl<'sc, T, Sp: ?Sized> ScopedSpawnExt<'sc, T> for Sp where Sp: ScopedSpawn<'sc, T> {}
