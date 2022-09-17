use std::{
    ops::Not,
    sync::{
        atomic::{self, AtomicBool, AtomicUsize},
        Condvar, Mutex,
    },
    task::Context,
};

use futures::task::{FutureObj, SpawnError};

use super::waker_park::{WaitResult, WakerPark};

#[derive(Debug)]
pub struct RelayPad<'sc> {
    sender: crossbeam_channel::Sender<FutureObj<'sc, ()>>,
    receiver: crossbeam_channel::Receiver<FutureObj<'sc, ()>>,
    waker_park: WakerPark,
    current_polls: AtomicUsize,
    destroy: AtomicBool,
    destroy_waiting: Condvar,
    destroy_mutex: Mutex<()>,
}

impl<'sc> RelayPad<'sc> {
    pub fn new() -> Self {
        let (sender, receiver) = crossbeam_channel::unbounded();
        Self {
            sender,
            receiver,
            waker_park: WakerPark::new(),
            current_polls: AtomicUsize::new(0),
            destroy: AtomicBool::new(false),
            destroy_waiting: Condvar::new(),
            destroy_mutex: Mutex::new(()),
        }
    }

    pub fn enqueue_task(&self, task: FutureObj<'sc, ()>) -> Result<(), SpawnError> {
        if self.destroy.load(atomic::Ordering::Relaxed) {
            return Err(SpawnError::shutdown());
        }

        let result = self
            .sender
            .send(task)
            .map_err(|_err| SpawnError::shutdown());

        if result.is_ok() {
            self.waker_park.wake();
        }

        result
    }

    pub fn dequeue_task(
        &self,
        cx: Option<&mut Context<'_>>,
    ) -> Result<FutureObj<'sc, ()>, TaskDequeueErr> {
        loop {
            let token = self.waker_park.token();

            if self.destroy.load(atomic::Ordering::SeqCst) {
                return Err(TaskDequeueErr::Destroy);
            }

            match self.receiver.try_recv() {
                Ok(task) => return Ok(task),
                Err(_) => {
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

    pub fn rescue_future(&self, future: FutureObj<'sc, ()>) {
        println!("rescued future");
        self.sender.send(future).unwrap();
    }

    pub fn start_future_polling(&self) -> Option<FuturePollingGuard<'_, 'sc>> {
        self.current_polls.fetch_add(1, atomic::Ordering::SeqCst);
        let guard = FuturePollingGuard { pad: self };
        self.destroy
            .load(atomic::Ordering::SeqCst)
            .not()
            .then_some(guard)
    }

    fn end_future_polling(&self) {
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
}

impl<'sc> Drop for RelayPad<'sc> {
    fn drop(&mut self) {
        println!("Drop Relay Pad");
    }
}

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
}

impl<'l, 'sc> Drop for FuturePollingGuard<'l, 'sc> {
    fn drop(&mut self) {
        self.pad.end_future_polling();
    }
}
