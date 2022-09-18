use std::sync::Arc;

mod relay_future;
mod relay_pad;
mod waker_park;

use futures::task::{FutureObj, LocalSpawn, LocalSpawnExt, Spawn, SpawnError, SpawnExt};
pub use relay_pad::UntilEmpty;

use self::relay_future::RelayFuture;
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
    /// Spawned futures can reference everything covered by 'sc.
    ///
    /// # Safety
    /// It is of utmost important that the created scope is dropped at the end of 'sc.
    /// Especially [`std::mem::forget`] should not be used on this type.
    /// Failing to drop this correctly can lead to spawned futures having references
    /// into undefined memory (namely when they reference something on the stack that is already popped).
    pub unsafe fn unchecked_new() -> Self {
        Self {
            pad: Arc::new(RelayPad::new()),
        }
    }

    pub fn relay_to(&self, spawn: &(impl Spawn + Clone + Send + 'static)) {
        let fut = unsafe { RelayFuture::new_global(self.pad.clone(), spawn.clone()) };
        spawn.spawn(fut).unwrap();
    }

    pub fn relay_to_local(&self, spawn: &(impl LocalSpawn + Clone + 'static)) {
        let fut = unsafe { RelayFuture::new_local(self.pad.clone(), spawn.clone()) };
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
}

impl<'sc> Spawn for RelayScopeSpawner<'sc> {
    fn spawn_obj(&self, future: FutureObj<'static, ()>) -> Result<(), SpawnError> {
        self.spawn_obj_scoped(future)
    }
}

/*
#[cfg(test)]
mod tests {

    use std::future::pending;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use futures::channel::oneshot;
    use futures::executor::{block_on, LocalPool, ThreadPool, ThreadPoolBuilder};

    use super::RelayScope;
    use crate::relay::{RelayScopeLocalSpawning, RelayScopeSpawning};
    use crate::{ScopedSpawnExt, SpawnScope};


    #[test]
        fn test_relay_scope() {
            #[derive(Debug)]
            struct Unmovable(i32);
            let unmovable = Unmovable(100);
            let mut pool = LocalPool::new();
            let (sx, rx) = oneshot::channel();
            {
                let relay_scope = new_relay_scope!();
                pool.spawner().spawn_scope_local(&relay_scope);
                let spawner = relay_scope.spawner();

                spawner
                    .spawn_scoped(async {
                        println!("unmovable: {:?}", unmovable);
                    })
                    .unwrap();

                spawner
                    .spawn_scoped(async {
                        println!("never end {:?}", unmovable);
                        rx.await.unwrap();
                        println!("received data {:?}", unmovable);
                    })
                    .unwrap();

                pool.run_until_stalled();
            }
            println!("pool.run");
            sx.send(()).ok();
            pool.run();
        }
    #[test]
    fn test_on_thread_pool() {
        let pool = ThreadPool::new().unwrap();

        let until_empty = {
            let scope = new_relay_scope!();

            pool.spawn_scope(scope);

            let spawner = scope.spawner();

            for i in 0..1000 {
                spawner
                    .spawn_scoped(async move {
                        println!("process {} on {:?}", i, thread::current().id());
                        //panic!();
                    })
                    .unwrap();
            }

            let until_empty = scope.until_empty();

            block_on(scope.until_empty());
            println!("done... do some more");

            for _ in 0..50 {
                spawner
                    .spawn_scoped(async {
                        println!("in second scope");
                    })
                    .unwrap();
            }

            until_empty
        };
        println!("scope destroyed");
        thread::sleep(Duration::from_millis(300));
        block_on(until_empty);
    }
}

*/
