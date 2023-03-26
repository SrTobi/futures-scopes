use std::ops::{DerefMut, Not};
use std::pin::Pin;
use std::sync::atomic::{self, AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use dashmap::DashMap;
use futures::channel::oneshot;
use futures::future::Shared;
use futures::task::{FutureObj, SpawnError};
use futures::{Future, FutureExt};
use pin_project::pin_project;

use super::relay_future::{RelayFutureId, RelayFutureInner};
use super::waker_park::{WaitResult, WakerPark};

#[derive(Debug)]
#[allow(clippy::type_complexity)]
pub struct RelayPad<'sc> {
    next_spawn_id: AtomicUsize,
    sender: crossbeam_channel::Sender<FutureObj<'sc, ()>>,
    receiver: crossbeam_channel::Receiver<FutureObj<'sc, ()>>,
    relays: DashMap<RelayFutureId, Arc<RelayFutureInner<'sc>>>,
    waker_park: WakerPark,
    current_tasks: AtomicUsize,
    destroy: AtomicBool,
    wait_until_empty: Mutex<Option<(Shared<oneshot::Receiver<()>>, oneshot::Sender<()>)>>,
}

impl<'sc> RelayPad<'sc> {
    pub fn new() -> Self {
        let (sender, receiver) = crossbeam_channel::unbounded();
        Self {
            next_spawn_id: AtomicUsize::new(0),
            sender,
            receiver,
            relays: DashMap::new(),
            waker_park: WakerPark::new(),
            current_tasks: AtomicUsize::new(0),
            destroy: AtomicBool::new(false),
            wait_until_empty: Mutex::new(None),
        }
    }

    pub fn next_spawn_id(&self) -> usize {
        self.next_spawn_id.fetch_add(1, atomic::Ordering::Relaxed)
    }

    pub fn register_relay_future(&self, relay: Arc<RelayFutureInner<'sc>>) {
        if !self.destroy.load(atomic::Ordering::SeqCst) {
            self.relays.insert(relay.id(), relay);
        }
    }

    pub fn unregister_relay_future(&self, id: RelayFutureId, fut: Option<FutureObj<'sc, ()>>) {
        //println!("unregister future {:?}", id);
        let old = self.relays.remove(&id);
        debug_assert!(old.is_some(), "Expected {id:?} to have been registered");

        if !self.is_destroyed() {
            if let Some(fut) = fut {
                self.sender.send(fut).unwrap();
            }
        }
    }

    pub fn enqueue_task(&self, task: FutureObj<'sc, ()>) -> Result<(), SpawnError> {
        if self.is_destroyed() {
            return Err(SpawnError::shutdown());
        }

        let result = self.sender.send(task).map_err(|_err| SpawnError::shutdown());

        if result.is_ok() {
            self.current_tasks.fetch_add(1, atomic::Ordering::Relaxed);
            self.waker_park.wake();
        }

        result
    }

    pub fn dequeue_task(&self, cx: Option<&mut Context<'_>>) -> Result<FutureObj<'sc, ()>, TaskDequeueErr> {
        loop {
            let token = self.waker_park.token();

            if self.is_destroyed() {
                return Err(TaskDequeueErr::Destroy);
            }

            match self.receiver.try_recv() {
                Ok(task) => return Ok(task),
                Err(_) => {
                    //println!("no tasks {:?}", cx);
                    let Some(cx) = &cx else {
                        return Err(TaskDequeueErr::NoTasks);
                    };

                    match self.waker_park.wait(cx.waker().clone(), token) {
                        WaitResult::Ok => return Err(TaskDequeueErr::WaitingForTasks),
                        WaitResult::TokenMismatch => continue,
                    }
                }
            }
        }
    }

    /*pub fn rescue_future(&self, future: FutureObj<'sc, ()>) {
        //println!("rescued future");
        self.sender.send(future).unwrap();
    }*/

    pub fn start_future_polling(&self) -> Option<FuturePollingGuard<'_, 'sc>> {
        let guard = FuturePollingGuard {
            pad: self,
            poll_again: false,
        };
        self.is_destroyed().not().then_some(guard)
    }

    fn end_future_polling(&self, poll_again: bool) {
        if !poll_again {
            let rest_tasks = self.current_tasks.fetch_sub(1, atomic::Ordering::Relaxed) - 1;
            //println!("future finised. rest: {}", rest_tasks);
            if rest_tasks == 0 {
                if let Some((_, sx)) = self.wait_until_empty.lock().unwrap().deref_mut().take() {
                    sx.send(()).unwrap();
                }
            };
        }
    }

    pub fn destroy(&self) {
        //println!("destroy");
        self.destroy.store(true, atomic::Ordering::SeqCst);

        while !self.relays.is_empty() {
            //println!("{} futures left", self.relays.len());
            let relays: Vec<_> = self.relays.iter().map(|entry| entry.value().clone()).collect();
            for relay in relays {
                relay.destroy(self, false);
            }
        }

        self.receiver.try_iter().for_each(|_| ());

        self.waker_park.wake();
    }

    pub fn is_destroyed(&self) -> bool {
        self.destroy.load(atomic::Ordering::SeqCst)
    }

    pub fn until_empty(&self) -> UntilEmpty {
        let mut lock = self.wait_until_empty.lock().unwrap();
        if 0 == self.current_tasks.load(atomic::Ordering::Relaxed) {
            //println!("return empty UntilEmpty");
            let (sx, rx) = oneshot::channel();
            sx.send(()).unwrap();
            return UntilEmpty { receiver: rx.shared() };
        }
        let (rx, _) = lock.get_or_insert_with(|| {
            //println!("new until_empty channel {:?}", self.receiver.len());
            let (sx, rx) = oneshot::channel();
            (rx.shared(), sx)
        });

        UntilEmpty { receiver: rx.clone() }
    }
}
/*
impl<'sc> Drop for RelayPad<'sc> {
    fn drop(&mut self) {
        println!("Drop Relay Pad");
    }
}*/

#[derive(Debug)]
pub enum TaskDequeueErr {
    NoTasks,
    WaitingForTasks,
    Destroy,
}

#[must_use = "if unused polling will end immediately"]
#[clippy::has_significant_drop]
#[derive(Debug)]
pub struct FuturePollingGuard<'l, 'sc> {
    pad: &'l RelayPad<'sc>,
    poll_again: bool,
}

impl<'l, 'sc> FuturePollingGuard<'l, 'sc> {
    pub fn will_poll_again(&mut self) {
        self.poll_again = true;
    }
}

impl<'l, 'sc> Drop for FuturePollingGuard<'l, 'sc> {
    fn drop(&mut self) {
        self.pad.end_future_polling(self.poll_again);
    }
}

#[pin_project]
pub struct UntilEmpty {
    #[pin]
    receiver: Shared<oneshot::Receiver<()>>,
}

impl Future for UntilEmpty {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().receiver.poll(cx).map(|_| ())
    }
}
