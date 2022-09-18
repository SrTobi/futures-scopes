use std::pin::Pin;
use std::sync::atomic::{self, AtomicUsize};
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::task::{FutureObj, LocalSpawn, LocalSpawnExt, Spawn, SpawnExt};
use futures::Future;
use pin_project::{pin_project, pinned_drop};

use super::relay_pad::{RelayPad, TaskDequeueErr};

trait Respawn {
    fn respawn<'sc>(&self, pad: Arc<RelayPad<'sc>>, respawn_counter: Arc<RespawnCounter>, root: bool);
}

#[derive(Clone)]
pub struct GlobalRespawn<Sp>(Sp);

impl<Sp: Spawn + Clone + Send + 'static> Respawn for GlobalRespawn<Sp> {
    fn respawn<'sc>(&self, pad: Arc<RelayPad<'sc>>, respawn_counter: Arc<RespawnCounter>, root: bool) {
        let fut = unsafe { RelayFuture::new_full(pad, self.clone(), root, respawn_counter) };
        self.0.spawn(fut).ok();
    }
}

#[derive(Clone)]
pub struct LocalRespawn<Sp>(Sp);

impl<Sp: LocalSpawn + Clone + 'static> Respawn for LocalRespawn<Sp> {
    fn respawn<'sc>(&self, pad: Arc<RelayPad<'sc>>, respawn_counter: Arc<RespawnCounter>, root: bool) {
        let fut = unsafe { RelayFuture::new_full(pad, self.clone(), root, respawn_counter) };
        self.0.spawn_local(fut).ok();
    }
}

#[derive(Debug)]
struct RespawnCounter {
    non_working: AtomicUsize,
    all: AtomicUsize,
}

impl RespawnCounter {
    fn new() -> Self {
        Self {
            non_working: AtomicUsize::new(0),
            all: AtomicUsize::new(0),
        }
    }

    fn subscribe(&self) {
        self.non_working.fetch_add(1, atomic::Ordering::Relaxed);
        self.all.fetch_add(1, atomic::Ordering::Relaxed);
    }

    fn unsubscribe(&self) {
        self.non_working.fetch_sub(1, atomic::Ordering::Relaxed);
        self.all.fetch_sub(1, atomic::Ordering::Relaxed);
    }

    fn start_polling(&self) -> RespawnCounterPollingGuard<'_> {
        let non_working = self.non_working.fetch_sub(1, atomic::Ordering::Relaxed) - 1;
        let should_respawn = non_working < 5;
        return RespawnCounterPollingGuard(self, should_respawn);
    }
}

#[derive(Debug)]
struct RespawnCounterPollingGuard<'c>(&'c RespawnCounter, bool);

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
struct Unpinned<'sc, Sp> {
    pad: Arc<RelayPad<'sc>>,
    spawn: Sp,
    root: bool,
    respawn_counter: Arc<RespawnCounter>,
}

impl<'sc, Sp> Unpinned<'sc, Sp> {
    fn respawn(&self, root: bool)
    where
        Sp: Respawn,
    {
        println!("spawn another RelayFuture {:?}", self.respawn_counter);
        self.spawn.respawn(self.pad.clone(), self.respawn_counter.clone(), root);
    }
}

#[pin_project(PinnedDrop)]
#[derive(Debug)]
struct RelayFutureInner<'sc, Sp> {
    #[pin]
    future: Option<FutureObj<'sc, ()>>,
    unpinned: Unpinned<'sc, Sp>,
}

impl<'sc, Sp> RelayFutureInner<'sc, Sp> {
    fn new(pad: Arc<RelayPad<'sc>>, spawn: Sp, root: bool, respawn_counter: Arc<RespawnCounter>) -> Self {
        respawn_counter.subscribe();
        Self {
            future: None,
            unpinned: Unpinned {
                pad,
                spawn,
                root,
                respawn_counter,
            },
        }
    }
}

#[pinned_drop]
impl<'sc, Sp> PinnedDrop for RelayFutureInner<'sc, Sp> {
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();
        let unpinned = this.unpinned;
        println!("drop RelayFuture {:?}", unpinned.respawn_counter);
        /*let fut = this.future.take();
        if let Some(fut) = fut {
            unpinned.pad.rescue_future(fut);
        }*/
        unpinned.respawn_counter.unsubscribe();
    }
}

impl<'sc, Sp: Respawn> Future for RelayFutureInner<'sc, Sp> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut this = self.as_mut().project();
        let unpinned = this.unpinned;
        //println!("RelayFutureInner::poll");
        let mut finished_tasks = 0;
        loop {
            if let Some(fut) = this.future.as_mut().as_pin_mut() {
                //println!("RelayFutureInner::poll start polling future");
                if let Some(mut poll_guard) = unpinned.pad.start_future_polling() {
                    //println!("RelayFutureInner::poll got guard. polling inner");
                    let _respawn_guard = unpinned.respawn_counter.start_polling();
                    if _respawn_guard.should_respawn() {
                        unpinned.respawn(false);
                    }

                    struct Bomb<'l, 'sc, Sp: Respawn>(&'l Unpinned<'sc, Sp>, bool);
                    impl<'l, 'sc, Sp: Respawn> Drop for Bomb<'l, 'sc, Sp> {
                        fn drop(&mut self) {
                            if self.1 {
                                println!("polling panicked.. respawn to ensure at least one future is present");
                                self.0.respawn(true);
                            }
                        }
                    }

                    let mut bomb = Bomb(&unpinned, unpinned.root);
                    let poll_result = fut.poll(cx);
                    bomb.1 = false;

                    match poll_result {
                        Poll::Ready(()) => {
                            this.future.take();
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
                    let _fut = this.future.take();
                    //unpinned.pad.rescue_future(fut.unwrap());
                    return Poll::Ready(());
                }
            } else {
                //println!("RelayFutureInner::poll get future");
                match unpinned.pad.dequeue_task(unpinned.root.then_some(cx)) {
                    Ok(task) => this.future.as_mut().set(Some(task)),
                    Err(TaskDequeueErr::WaitingForTasks) => return Poll::Pending,
                    Err(TaskDequeueErr::NoTasks) => return Poll::Ready(()),
                    Err(TaskDequeueErr::Destroy) => return Poll::Ready(()),
                };

                //println!("RelayFutureInner::poll acquire new future");
            }
        }
    }
}

#[pin_project]
#[derive(Debug)]
pub struct RelayFuture<Sp> {
    #[pin]
    inner: RelayFutureInner<'static, Sp>,
}

impl<Sp> RelayFuture<Sp> {
    unsafe fn new_full<'sc>(
        pad: Arc<RelayPad<'sc>>,
        spawn: Sp,
        root: bool,
        respawn_counter: Arc<RespawnCounter>,
    ) -> Self {
        let static_pad = std::mem::transmute::<Arc<RelayPad<'sc>>, Arc<RelayPad<'static>>>(pad);
        Self {
            inner: RelayFutureInner::new(static_pad, spawn, root, respawn_counter),
        }
    }
}

impl<Sp> RelayFuture<GlobalRespawn<Sp>> {
    pub unsafe fn new_global<'sc>(pad: Arc<RelayPad<'sc>>, spawn: Sp) -> Self
    where
        Sp: Spawn + Clone + Send + 'static,
    {
        Self::new_full(pad, GlobalRespawn(spawn), true, Arc::new(RespawnCounter::new()))
    }
}

impl<Sp> RelayFuture<LocalRespawn<Sp>> {
    pub unsafe fn new_local<'sc>(pad: Arc<RelayPad<'sc>>, spawn: Sp) -> Self
    where
        Sp: LocalSpawn + Clone + 'static,
    {
        Self::new_full(pad, LocalRespawn(spawn), true, Arc::new(RespawnCounter::new()))
    }
}

impl<Sp: Respawn> Future for RelayFuture<Sp> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().inner.poll(cx)
    }
}
