use std::sync::Arc;

mod relay_future;
mod relay_pad;
mod waker_park;

use futures::task::{FutureObj, LocalSpawn, LocalSpawnExt, Spawn, SpawnError, SpawnExt};
pub use relay_pad::UntilEmpty;

use self::relay_future::UnsafeRelayFuture;
use self::relay_pad::RelayPad;
use crate::{ScopedSpawn, SpawnScope};

/// A local spawn that can be spawned onto a [`RelayScope`].
pub trait RelayScopeLocalSpawning: LocalSpawn + Clone + 'static {
    /// Add this spawn to `scope` and relay spawned futures to it.
    fn spawn_scope(&self, scope: &RelayScope) -> Result<(), SpawnError> {
        scope.relay_to_local(self)
    }
}

impl<Sp: LocalSpawn + Clone + 'static> RelayScopeLocalSpawning for Sp {}

/// A spawn that can be spawned onto a [`RelayScope`].
pub trait RelayScopeSpawning: Spawn + Clone + 'static + Send {
    /// Add this spawn to `scope` and relay spawned futures to it.
    fn spawn_scope(&self, scope: &RelayScope) -> Result<(), SpawnError> {
        scope.relay_to(self)
    }
}

impl<Sp: Spawn + Clone + Send + 'static> RelayScopeSpawning for Sp {}

/// A spawn scope that can be used to spawn futures of lifetime `'sc` onto multiple underlying spawns.
///
/// To poll the spawned futures, `RelayScope` will spawn *relay-tasks* onto all the underlying spawns
/// registered with [`relay_to`](RelayScope::relay_to) or [`relay_to_local`](RelayScope::relay_to_local).
/// These *relay-tasks* will replicate themselves to fill up the underlying spawns, but not overwhelm them.
/// This behavior makes it possible to consolidate multiple spawns into one.
///
/// **Important**: Spawned futures will not be polled until [`relay_to`](RelayScope::relay_to)
///                or [`relay_to_local`](RelayScope::relay_to_local) is used to spawn this scope
///                onto another spawn. Especially [`until_empty`](RelayScope::until_empty) will not poll
///                any future that was spawned onto the scope
///                (unlike [`LocalScope::until_empty`](crate::local::LocalScope::until_empty)).
///
/// To safely create a RelayScope, use [`new_relay_scope!`].
///
/// # Dropping
///
/// If the scope is dropped, all futures that were spawned on it will be dropped as well.
/// For this to happen, the scope may momentarily block the current thread to wait for a future to return from its current polling operation.
/// Note that this does not mean the scope will block until all futures are polled to completion,
/// but only until the currently running futures are done polling, regardless of whether they return ready or pending.
///
/// # Example
///
/// ```
/// # use futures::executor::{LocalPool,ThreadPool};
/// # use std::sync::Mutex;
/// # use futures_scopes::*;
/// # use futures_scopes::relay::*;
/// let thread_pool = ThreadPool::new().unwrap();
/// let mut local_pool = LocalPool::new();
///
/// let task_done = Mutex::new(false);
///
/// let scope = new_relay_scope!();
///
/// // Spawn the scope on both the thread pool and the local pool
/// scope.relay_to(&thread_pool).unwrap();
/// scope.relay_to_local(&local_pool.spawner()).unwrap();
///
/// scope.spawner().spawn_scoped(async {
///   // This is either executed on the thread pool or the local pool
///   println!("Hello from the scope!");
///
///   // task_done can safely be referenced without using Arc
///   *task_done.lock().unwrap() = true;
/// }).unwrap();
///
/// // Run the local pool until we are done for sure
/// local_pool.run_until(scope.until_empty());
///
/// assert!(task_done.lock().unwrap().clone());
/// ```
///
/// # How relaying works
///
/// When [`relay_to`](RelayScope::relay_to) or [`relay_to_local`](RelayScope::relay_to_local) is used,
/// `RelayScope` will spawn a single *relay-task* on the given spawn.
/// Each *relay-task* (potentially on different executors) will take a future from the scope and try to complete it.
/// After completing the future, it will grab a new one and repeat the process.
///
/// When the scope is dropped, all futures within the *relay-tasks* will be dropped as well.
/// The *relay-tasks* themselves will continue to live but complete [`Poll::Ready`](std::task::Poll::Ready) the next time they are polled.
///
/// To process multiple tasks on the same spawn, the *relay-tasks* will replicate themselves,
/// by spawning new *relay-tasks* on the same spawn from time to time.
/// To not overwhelm a single spawn and leave room for other tasks,
/// the `RelayScope` will monitor the amount of currently not processed tasks for each of its underlying spawns.
/// If there are too many tasks that are not processed, it will not spawn new *relay-tasks* on that spawn.
///
///
#[derive(Debug)]
pub struct RelayScope<'sc> {
    pad: Arc<RelayPad<'sc>>,
}

/// Creates a new RelayScope in the current scope and returns a reference to it.
///
/// The macro can take any number of
/// [`LocalSpawn`](futures::task::LocalSpawn)s
/// or
/// [`Spawn`](futures::task::Spawn)s
/// as arguments.
/// The scope will then be spawned onto all of them.
///
/// Because the actual value cannot be accessed, it cannot be moved out of scope or be [`std::mem::forget`]ten,
/// which could lead to undefined behavior.
///
/// # Examples
///
/// ```
/// use futures::executor::{LocalPool, ThreadPool};
/// use futures_scopes::relay::new_relay_scope;
///
/// let thread_pool = ThreadPool::new().unwrap();
/// let pool = LocalPool::new();
///
/// let scope1 = new_relay_scope!();
/// let scope2 = new_relay_scope!(thread_pool, pool.spawner());
/// ```
#[doc(hidden)]
#[macro_export]
macro_rules! __new_relay_scope__ {
    () => {{
        &unsafe { $crate::relay::RelayScope::unchecked_new() }
    }};
    ($($scopes:expr),+) => {{
        let create_custom_scope = || {
            use $crate::relay::{RelayScopeLocalSpawning, RelayScopeSpawning};
            let scope = unsafe { $crate::relay::RelayScope::unchecked_new() };
            $(
                $scopes.spawn_scope(&scope).unwrap();
            )+
            scope
        };
        &create_custom_scope()
    }};
}

#[doc(inline)]
pub use __new_relay_scope__ as new_relay_scope;

impl RelayScope<'static> {
    /// Creates a new RelayScope that can only spawn static futures.
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
    /// Use [`new_relay_scope`] to safely create a RelayScope, that cannot **not** be dropped.
    pub unsafe fn unchecked_new() -> Self {
        Self {
            pad: Arc::new(RelayPad::new()),
        }
    }

    /// Relay this scope to the given `spawn`.
    ///
    /// This will spawn a *relay-task* onto the given [`Spawn`] that will process the futures of this scope.
    /// For more details see [here](RelayScope#how-relaying-works).
    pub fn relay_to(&self, spawn: &(impl Spawn + Clone + Send + 'static)) -> Result<(), SpawnError> {
        let fut =
            unsafe { UnsafeRelayFuture::new_global(self.pad.clone(), spawn.clone(), self.pad.next_spawn_id()) };
        spawn.spawn(fut)
    }

    /// Relay this scope to the given local `spawn`.
    ///
    /// This will spawn a *relay-task* onto the given [`LocalSpawn`] that will process the futures of this scope.
    /// For more details see [here](RelayScope#how-relaying-works).
    pub fn relay_to_local(&self, spawn: &(impl LocalSpawn + Clone + 'static)) -> Result<(), SpawnError> {
        let fut =
            unsafe { UnsafeRelayFuture::new_local(self.pad.clone(), spawn.clone(), self.pad.next_spawn_id()) };
        spawn.spawn_local(fut)
    }

    /// Returns a future that will complete the moment there are no more spawned futures in the scope.
    ///
    /// This does not effect the ability to spawn new futures,
    /// and new futures may have been spawned onto the scope before `.until_empty().await` even returns.
    ///
    /// Note that unlike [`LocalScope::until_empty`](crate::local::LocalScope::until_empty), the returned `UntilEmpty` future
    /// does not poll any future that was spawned onto the scope.
    /// Use [`relay_to`](RelayScope::relay_to) or [`relay_to_local`](RelayScope::relay_to_local), otherwise this future will never complete.
    ///
    /// # Example
    ///
    /// ```
    /// # use futures_scopes::*;
    /// # use futures_scopes::relay::*;
    /// # use futures::executor::{ThreadPool, block_on};
    /// let scope = new_relay_scope!();
    ///
    /// // Relay the scope to a thread pool
    /// let thread_pool = ThreadPool::new().unwrap();
    /// scope.relay_to(&thread_pool).unwrap();
    ///
    /// // Spawn some task
    /// scope.spawner().spawn_scoped(async { /* ... */ }).unwrap();
    ///
    /// // Because task will be executed on another thread, we won't run into a deadlock
    /// // when blocking this thread.
    /// block_on(scope.until_empty());
    /// ```
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
    /// # Example
    ///
    /// ```
    /// # use futures_scopes::*;
    /// # use futures_scopes::relay::*;
    /// # use futures::executor::block_on;
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

/// A spawner that can be obtained from [`RelayScope::spawner`].
///
/// This spawner may live longer then the scope.
/// In case a future is spawned after the scope has been destroyed,
/// the spawner will return [`SpawnError::shutdown`].
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
