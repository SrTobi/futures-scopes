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
    fn spawn_scope_local<'sc>(&self, scope: &RelayScope<'sc>) {
        scope.relay_to_local(self);
    }
}

impl<Sp: LocalSpawn + Clone + 'static + ?Sized> RelayScopeLocalSpawning for Sp {}

pub trait RelayScopeSpawning: Spawn + Clone + 'static + Send {
    fn spawn_scope<'sc>(&self, scope: &RelayScope<'sc>) {
        scope.relay_to(self);
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

    pub fn relay_to(&self, spawn: &(impl Spawn + Clone + Send + 'static)) {
        let fut =
            unsafe { UnsafeRelayFuture::new_global(self.pad.clone(), spawn.clone(), self.pad.next_spawn_id()) };
        spawn.spawn(fut).unwrap();
    }

    pub fn relay_to_local(&self, spawn: &(impl LocalSpawn + Clone + 'static)) {
        let fut =
            unsafe { UnsafeRelayFuture::new_local(self.pad.clone(), spawn.clone(), self.pad.next_spawn_id()) };
        spawn.spawn_local(fut).unwrap();
    }

    pub fn until_empty(&self) -> UntilEmpty {
        self.pad.until_empty()
    }
}

impl<'sc> Drop for RelayScope<'sc> {
    fn drop(&mut self) {
        self.pad.destroy();
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
