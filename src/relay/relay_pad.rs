use std::ops::{DerefMut, Not};
use std::pin::Pin;
use std::sync::atomic::{self, AtomicBool, AtomicUsize};
use std::sync::{Condvar, Mutex};
use std::task::{Context, Poll};

use futures::channel::oneshot;
use futures::future::Shared;
use futures::task::{FutureObj, SpawnError};
use futures::{Future, FutureExt};
use pin_project::pin_project;

use super::waker_park::{WaitResult, WakerPark};

#[derive(Debug)]
pub struct RelayPad<'sc> {
    sender: crossbeam_channel::Sender<FutureObj<'sc, ()>>,
    receiver: crossbeam_channel::Receiver<FutureObj<'sc, ()>>,
    waker_park: WakerPark,
    current_polls: AtomicUsize,
    current_tasks: AtomicUsize,
    destroy: AtomicBool,
    destroy_waiting: Condvar,
    destroy_mutex: Mutex<()>,
    wait_until_empty: Mutex<Option<(Shared<oneshot::Receiver<()>>, oneshot::Sender<()>)>>,
}

impl<'sc> RelayPad<'sc> {
    pub fn new() -> Self {
        let (sender, receiver) = crossbeam_channel::unbounded();
        Self {
            sender,
            receiver,
            waker_park: WakerPark::new(),
            current_polls: AtomicUsize::new(0),
            current_tasks: AtomicUsize::new(0),
            destroy: AtomicBool::new(false),
            destroy_waiting: Condvar::new(),
            destroy_mutex: Mutex::new(()),
            wait_until_empty: Mutex::new(None),
        }
    }

    pub fn enqueue_task(&self, task: FutureObj<'sc, ()>) -> Result<(), SpawnError> {
        if self.destroy.load(atomic::Ordering::Relaxed) {
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

            if self.destroy.load(atomic::Ordering::SeqCst) {
                return Err(TaskDequeueErr::Destroy);
            }

            match self.receiver.try_recv() {
                Ok(task) => return Ok(task),
                Err(_) => {
                    //println!("no tasks {:?}", cx);
                    if let Some(cx) = &cx {
                        match self.waker_park.wait(cx.waker().clone(), token) {
                            WaitResult::Ok => return Err(TaskDequeueErr::WaitingForTasks),
                            WaitResult::TokenMismatch => continue,
                        }
                    } else {
                        return Err(TaskDequeueErr::NoTasks);
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
        self.current_polls.fetch_add(1, atomic::Ordering::SeqCst);
        let guard = FuturePollingGuard {
            pad: self,
            poll_again: false,
        };
        self.destroy.load(atomic::Ordering::SeqCst).not().then_some(guard)
    }

    fn end_future_polling(&self, poll_again: bool) {
        if !poll_again {
            if 1 == self.current_tasks.fetch_sub(1, atomic::Ordering::Relaxed) {
                if let Some((_, sx)) = self.wait_until_empty.lock().unwrap().deref_mut().take() {
                    sx.send(()).unwrap();
                }
            }
        }
        if 1 == self.current_polls.fetch_sub(1, atomic::Ordering::SeqCst) {
            // last future polling has ended

            let _guard = self.destroy_mutex.lock().unwrap();
            self.destroy_waiting.notify_all();
        }
    }

    pub fn destroy(&self) {
        self.destroy.store(true, atomic::Ordering::SeqCst);
        //println!("destroy");
        self.waker_park.wake();
        let mut guard = self.destroy_mutex.lock().unwrap();
        while 0 != self.current_polls.load(atomic::Ordering::SeqCst) {
            guard = self.destroy_waiting.wait(guard).unwrap();
        }
    }

    pub fn until_empty(&self) -> UntilEmpty {
        if 0 == self.current_tasks.load(atomic::Ordering::Relaxed) {
            //println!("return empty UntilEmpty");
            let (sx, rx) = oneshot::channel();
            sx.send(()).unwrap();
            return UntilEmpty { receiver: rx.shared() };
        }
        let mut lock = self.wait_until_empty.lock().unwrap();
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
