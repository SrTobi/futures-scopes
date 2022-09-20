use std::sync::Arc;

mod relay_future;
mod relay_pad;
mod waker_park;

use futures::task::{FutureObj, LocalSpawn, LocalSpawnExt, Spawn, SpawnError, SpawnExt};
pub use relay_pad::UntilEmpty;

use self::relay_future::UnsafeRelayFuture;
use self::relay_pad::RelayPad;
use crate::{ScopedSpawn, SpawnScope};

pub trait RelayScopeLocalSpawning: LocalSpawn + Clone + 'static {
    fn spawn_scope_local<'sc>(&self, scope: &RelayScope<'sc>) -> Result<(), SpawnError> {
        scope.relay_to_local(self)
    }
}

impl<Sp: LocalSpawn + Clone + 'static + ?Sized> RelayScopeLocalSpawning for Sp {}

pub trait RelayScopeSpawning: Spawn + Clone + 'static + Send {
    fn spawn_scope<'sc>(&self, scope: &RelayScope<'sc>) -> Result<(), SpawnError> {
        scope.relay_to(self)
    }
}

impl<Sp: Spawn + Clone + Send + ?Sized + 'static> RelayScopeSpawning for Sp {}

#[derive(Debug)]
pub struct RelayScope<'sc> {
    pad: Arc<RelayPad<'sc>>,
}

#[macro_export]
macro_rules! new_relay_scope {
    () => {{
        &unsafe { $crate::relay::RelayScope::unchecked_new() }
    }};
}

impl RelayScope<'static> {
    pub fn new() -> Self {
        unsafe { Self::unchecked_new() }
    }
}

impl Default for RelayScope<'static> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'sc> RelayScope<'sc> {
    /// Creates a new RelayScope
    ///
    /// Spawned futures can reference everything covered by `'sc`.
    ///
    /// # Safety
    /// It is of utmost important that the created scope is dropped at the end of 'sc.
    /// Especially [`std::mem::forget`] should not be used on this type.
    /// Failing to drop this correctly can lead to spawned futures having references
    /// into undefined memory (namely when they reference something on the stack that is already popped).
    /// Use [`new_relay_scope`] to safely create a RelayScope, that cannot not be dropped.
    pub unsafe fn unchecked_new() -> Self {
        Self {
            pad: Arc::new(RelayPad::new()),
        }
    }

    pub fn relay_to(&self, spawn: &(impl Spawn + Clone + Send + 'static)) -> Result<(), SpawnError> {
        let fut =
            unsafe { UnsafeRelayFuture::new_global(self.pad.clone(), spawn.clone(), self.pad.next_spawn_id()) };
        spawn.spawn(fut)
    }

    pub fn relay_to_local(&self, spawn: &(impl LocalSpawn + Clone + 'static)) -> Result<(), SpawnError> {
        let fut =
            unsafe { UnsafeRelayFuture::new_local(self.pad.clone(), spawn.clone(), self.pad.next_spawn_id()) };
        spawn.spawn_local(fut)
    }

    pub fn until_empty(&self) -> UntilEmpty {
        self.pad.until_empty()
    }

    /// Prevents new tasks from being spawned and cleans up all existing tasks.
    ///
    /// Calling this function is effectively the same as dropping the Scope.
    /// Afterwards the scope can be used normally, but all attempts to spawn
    /// new futures will fail with a [`shutdown error`].
    ///
    ///
    /// [`shutdown error`]: futures::task::SpawnError
    ///
    /// # Blocking
    ///
    /// This method blocks the current thread to cleanup the remaining tasks.
    /// Like drop it will wait for currently running, non-pending tasks to finish
    /// execution, before dropping them.
    ///
    /// ```
    /// use futures_scopes::*;
    /// use futures::executor::block_on;
    ///
    /// let scope = new_relay_scope!();
    ///
    /// // stop the scope
    /// scope.stop();
    ///
    /// // New spawn attempts will fail
    /// scope.spawner().spawn_scoped(async { /* ... */ }).unwrap_err();
    ///
    /// // The future returned by until_empty will continue to work
    /// // and return immediately
    /// block_on(scope.until_empty());
    /// ```
    pub fn stop(&self) {
        self.pad.destroy();
    }
}

impl<'sc> Drop for RelayScope<'sc> {
    fn drop(&mut self) {
        self.stop();
    }
}

impl<'sc> SpawnScope<'sc, ()> for RelayScope<'sc> {
    type Spawner = RelayScopeSpawner<'sc>;

    fn spawner(&self) -> Self::Spawner {
        RelayScopeSpawner {
            sender: self.pad.clone(),
        }
    }
}

#[derive(Debug)]
pub struct RelayScopeSpawner<'sc> {
    sender: Arc<RelayPad<'sc>>,
}

impl<'sc> Clone for RelayScopeSpawner<'sc> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
        }
    }
}

impl<'sc> ScopedSpawn<'sc, ()> for RelayScopeSpawner<'sc> {
    fn spawn_obj_scoped(&self, future: FutureObj<'sc, ()>) -> Result<(), SpawnError> {
        self.sender.enqueue_task(future)
    }

    fn status_scoped(&self) -> Result<(), SpawnError> {
        if self.sender.is_destroyed() {
            Err(SpawnError::shutdown())
        } else {
            Ok(())
        }
    }
}

impl<'sc> Spawn for RelayScopeSpawner<'sc> {
    fn spawn_obj(&self, future: FutureObj<'static, ()>) -> Result<(), SpawnError> {
        self.spawn_obj_scoped(future)
    }

    fn status(&self) -> Result<(), SpawnError> {
        self.status_scoped()
    }
}
