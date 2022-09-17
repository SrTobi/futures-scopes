use std::{
    sync::{
        atomic::{self, AtomicBool, AtomicUsize},
        Mutex,
    },
    task::Waker,
};

#[derive(Debug)]
pub struct WaitToken(usize);

pub enum WaitResult {
    Ok,
    TokenMismatch,
}

#[derive(Debug)]
pub struct WakerPark {
    inner: Mutex<Vec<Waker>>,
    has_wakers: AtomicBool,
    token: AtomicUsize,
}

impl WakerPark {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Vec::new()),
            has_wakers: AtomicBool::new(false),
            token: AtomicUsize::new(0),
        }
    }

    pub fn token(&self) -> WaitToken {
        WaitToken(self.token.load(atomic::Ordering::SeqCst))
    }

    pub fn wait(&self, waker: Waker, token: WaitToken) -> WaitResult {
        let mut inner = self.inner.lock().unwrap();

        self.has_wakers.store(true, atomic::Ordering::SeqCst);

        if self.token.load(atomic::Ordering::SeqCst) != token.0 {
            return WaitResult::TokenMismatch;
        }
        inner.push(waker);
        WaitResult::Ok
    }

    pub fn wake(&self) {
        self.token.fetch_add(1, atomic::Ordering::SeqCst);
        if self.has_wakers.load(atomic::Ordering::SeqCst) {
            let mut inner = self.inner.lock().unwrap();
            for waker in inner.drain(..) {
                waker.wake();
            }
            self.has_wakers.store(false, atomic::Ordering::SeqCst);
        }
    }
}
