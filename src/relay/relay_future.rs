use std::cell::Cell;
use std::pin::Pin;
use std::sync::atomic::{self, AtomicUsize};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures::task::{FutureObj, LocalFutureObj, LocalSpawn, LocalSpawnExt, Spawn, SpawnError};
use futures::Future;
use pin_project::{pin_project, pinned_drop};

use super::relay_pad::{RelayPad, TaskDequeueErr};

trait Respawn<'sc>: 'sc {
    fn respawn(&self, pad: Arc<RelayPad<'sc>>, manager: Arc<SpawnManager>, root: bool);
}

#[derive(Clone)]
pub struct GlobalRespawn<Sp>(Sp);

impl<'sc, Sp: Spawn + Clone + Send + 'sc> Respawn<'sc> for GlobalRespawn<Sp> {
    fn respawn(&self, pad: Arc<RelayPad<'sc>>, manager: Arc<SpawnManager>, root: bool) {
        let fut = unsafe { RelayFuture::new_global_raw(pad, self.clone(), root, manager) };
        if let Some(fut) = fut {
            self.0.spawn_obj(fut).ok();
        }
    }
}

#[derive(Clone)]
pub struct LocalRespawn<Sp>(Sp);

impl<'sc, Sp: LocalSpawn + Clone + 'sc> Respawn<'sc> for LocalRespawn<Sp> {
    fn respawn(&self, pad: Arc<RelayPad<'sc>>, manager: Arc<SpawnManager>, root: bool) {
        let fut = unsafe { RelayFuture::new_local_raw(pad, self.clone(), root, manager) };
        if let Some(fut) = fut {
            self.0.spawn_local(fut).ok();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RelayFutureId {
    spawn_id: usize,
    instance: usize,
}

#[derive(Debug)]
struct SpawnManager {
    non_working: AtomicUsize,
    all: AtomicUsize,
    id: usize,
    next_instance: AtomicUsize,
}

impl SpawnManager {
    fn new(id: usize) -> Self {
        Self {
            non_working: AtomicUsize::new(0),
            all: AtomicUsize::new(0),
            id,
            next_instance: AtomicUsize::new(0),
        }
    }

    fn register(&self) -> RelayFutureId {
        self.non_working.fetch_add(1, atomic::Ordering::Relaxed);
        self.all.fetch_add(1, atomic::Ordering::Relaxed);

        RelayFutureId {
            spawn_id: self.id,
            instance: self.next_instance.fetch_add(1, atomic::Ordering::Relaxed),
        }
    }

    fn unregister(&self) {
        self.non_working.fetch_sub(1, atomic::Ordering::Relaxed);
        self.all.fetch_sub(1, atomic::Ordering::Relaxed);
    }

    fn start_polling(&self) -> RespawnCounterPollingGuard<'_> {
        let non_working = self.non_working.fetch_sub(1, atomic::Ordering::Relaxed) - 1;
        let should_respawn = non_working < 5;
        RespawnCounterPollingGuard(self, should_respawn)
    }
}

#[derive(Debug)]
struct RespawnCounterPollingGuard<'c>(&'c SpawnManager, bool);

impl<'c> RespawnCounterPollingGuard<'c> {
    fn should_respawn(&self) -> bool {
        self.1
    }
}

impl<'c> Drop for RespawnCounterPollingGuard<'c> {
    fn drop(&mut self) {
        self.0.non_working.fetch_add(1, atomic::Ordering::Relaxed);
    }
}

#[derive(Debug)]
struct Unpinned<'sc, Sp: 'sc> {
    pad: Arc<RelayPad<'sc>>,
    panicked: Cell<bool>,
    root: bool,
    spawn: Sp,
    manager: Arc<SpawnManager>,
}

impl<'sc, Sp: Respawn<'sc>> Unpinned<'sc, Sp> {
    fn respawn(&self, root: bool) {
        //println!("spawn another RelayFuture {:?}", self.manager);
        self.spawn.respawn(self.pad.clone(), self.manager.clone(), root);
    }
}

#[derive(Debug)]
struct ActiveFuture<'sc> {
    future: Option<FutureObj<'sc, ()>>,
    destroyed: bool,
}

impl<'sc> ActiveFuture<'sc> {
    fn new() -> Self {
        Self {
            future: None,
            destroyed: false,
        }
    }
}

#[pin_project]
#[derive(Debug)]
pub struct RelayFutureInner<'sc> {
    #[pin]
    active: Mutex<ActiveFuture<'sc>>,
    id: RelayFutureId,
}

impl<'sc> RelayFutureInner<'sc> {
    pub fn destroy(&self, pad: &RelayPad<'sc>, rescue_future: bool) {
        let mut guard = self.active.lock().unwrap_or_else(|err| err.into_inner());
        if !guard.destroyed {
            // destroy our future
            //println!("Destroy inner");
            let fut = guard.future.take();
            guard.destroyed = true;
            pad.unregister_relay_future(self.id, fut.filter(|_| rescue_future));
        }
        debug_assert!(guard.future.is_none());
    }

    pub fn id(&self) -> RelayFutureId {
        self.id
    }
}

impl<'sc> RelayFutureInner<'sc> {}

#[pin_project(PinnedDrop)]
#[derive(Debug)]
struct RelayFuture<'sc, Sp> {
    #[pin]
    inner: Arc<RelayFutureInner<'sc>>,
    unpinned: Unpinned<'sc, Sp>,
}

impl<'sc, Sp> RelayFuture<'sc, Sp> {
    fn new(pad: Arc<RelayPad<'sc>>, spawn: Sp, root: bool, manager: Arc<SpawnManager>) -> Option<Self> {
        let id = manager.register();
        let inner = pad.register_relay_future(RelayFutureInner {
            active: Mutex::new(ActiveFuture::new()),
            id,
        })?;

        Some(Self {
            inner,
            unpinned: Unpinned {
                pad,
                panicked: Cell::new(false),
                root,
                spawn,
                manager,
            },
        })
    }
}

#[pinned_drop]
impl<'sc, Sp> PinnedDrop for RelayFuture<'sc, Sp> {
    #[allow(clippy::needless_lifetimes)]
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();
        let unpinned = this.unpinned;
        this.inner.destroy(&unpinned.pad, !unpinned.panicked.get());

        unpinned.manager.unregister();
    }
}

impl<'sc, Sp: Respawn<'sc>> Future for RelayFuture<'sc, Sp> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.as_mut().project();
        let unpinned = this.unpinned;
        //println!("RelayFutureInner::poll");
        let mut finished_tasks = 0;
        let this_id = this.inner.id;
        let future_cell = &mut this.inner.active.lock().unwrap().future;
        loop {
            if let Some(fut) = future_cell {
                //println!("RelayFutureInner::poll start polling future");
                if let Some(mut poll_guard) = unpinned.pad.start_future_polling(this_id) {
                    //println!("RelayFutureInner::poll got guard. polling inner");
                    let respawn_guard = unpinned.manager.start_polling();
                    if respawn_guard.should_respawn() {
                        unpinned.respawn(false);
                    }

                    struct Bomb<'l, 'sc, Sp: Respawn<'sc>>(&'l Unpinned<'sc, Sp>, bool);
                    impl<'l, 'sc, Sp: Respawn<'sc>> Drop for Bomb<'l, 'sc, Sp> {
                        fn drop(&mut self) {
                            if self.1 {
                                //println!("polling panicked.. respawn to ensure at least one future is present");
                                self.0.respawn(true);
                                self.0.panicked.set(true);
                            }
                        }
                    }

                    let mut bomb = Bomb(unpinned, unpinned.root);
                    let poll_result = Pin::new(fut).poll(cx);
                    bomb.1 = false;

                    match poll_result {
                        Poll::Ready(()) => {
                            future_cell.take();
                            finished_tasks += 1;
                            if finished_tasks > 5 {
                                unpinned.respawn(unpinned.root);
                                return Poll::Ready(());
                            }
                            continue;
                        }
                        Poll::Pending => {
                            poll_guard.will_poll_again();
                            unpinned.respawn(false);
                            return Poll::Pending;
                        }
                    }
                } else {
                    // destroying is ongoing...
                    return Poll::Ready(());
                }
            } else {
                //println!("RelayFutureInner::poll get future");
                match unpinned.pad.dequeue_task(unpinned.root.then_some(cx)) {
                    Ok(task) => *future_cell = Some(task),
                    Err(TaskDequeueErr::WaitingForTasks) => return Poll::Pending,
                    Err(TaskDequeueErr::NoTasks) => return Poll::Ready(()),
                    Err(TaskDequeueErr::Destroy) => return Poll::Ready(()),
                };

                //println!("RelayFutureInner::poll acquire new future");
            }
        }
    }
}

impl<'sc, Sp: 'sc> RelayFuture<'sc, Sp> {
    unsafe fn new_global_raw(
        pad: Arc<RelayPad<'sc>>,
        spawn: Sp,
        root: bool,
        manager: Arc<SpawnManager>,
    ) -> Option<FutureObj<'static, ()>>
    where
        Sp: Respawn<'sc> + Send,
    {
        let fut = Self::new(pad, spawn, root, manager)?;
        let fut_obj = FutureObj::new(Box::new(fut));
        /*
            This transmute is the lynchpin of the entire RelayScope.
            Two lifetime issues we have to be aware of:
            1. The relay future may have futures inside it that are 'sc.
               Thus these futures may only be processed as long as 'sc is alive.
               This is ensured by the fact that the RelayScope will wait for
               all these futures to be dropped before the RelayScope itself is dropped.
               And the RelayScope must be dropped before 'sc is dropped.
               Thus we can give the relay future into any 'static context.
            2. The relay future holds a spawn that lives at least for 'sc.
               This spawn is not accessed after the RelayScope is dropped.
               and the spawn will live longer than the RelayScope.
        */
        let static_fut = std::mem::transmute::<FutureObj<'sc, ()>, FutureObj<'static, ()>>(fut_obj);
        Some(static_fut)
    }

    unsafe fn new_local_raw(
        pad: Arc<RelayPad<'sc>>,
        spawn: Sp,
        root: bool,
        manager: Arc<SpawnManager>,
    ) -> Option<LocalFutureObj<'static, ()>>
    where
        Sp: Respawn<'sc>,
    {
        let fut = Self::new(pad, spawn, root, manager)?;
        let fut_obj = LocalFutureObj::new(Box::new(fut));
        /* See RelayFuture::new_global_raw */
        let static_fut = std::mem::transmute::<LocalFutureObj<'sc, ()>, LocalFutureObj<'static, ()>>(fut_obj);
        Some(static_fut)
    }
}

pub fn spawn_on_global<'sc, Sp: Spawn + Clone + Send + 'sc>(
    pad: Arc<RelayPad<'sc>>,
    spawn: Sp,
    spawn_id: usize,
) -> Result<(), SpawnError> {
    let fut = unsafe {
        RelayFuture::new_global_raw(
            pad,
            GlobalRespawn(spawn.clone()),
            true,
            Arc::new(SpawnManager::new(spawn_id)),
        )
    };
    if let Some(fut) = fut {
        spawn.spawn_obj(fut)
    } else {
        Err(SpawnError::shutdown())
    }
}

pub fn spawn_on_local<'sc, Sp: LocalSpawn + Clone + 'sc>(
    pad: Arc<RelayPad<'sc>>,
    spawn: Sp,
    spawn_id: usize,
) -> Result<(), SpawnError> {
    let fut = unsafe {
        RelayFuture::new_local_raw(
            pad,
            LocalRespawn(spawn.clone()),
            true,
            Arc::new(SpawnManager::new(spawn_id)),
        )
    };
    if let Some(fut) = fut {
        spawn.spawn_local_obj(fut)
    } else {
        Err(SpawnError::shutdown())
    }
}
