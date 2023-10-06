use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::mem;
use std::ops::DerefMut;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use futures::stream::FuturesUnordered;
use futures::task::{noop_waker, FutureObj, LocalFutureObj, LocalSpawn, Spawn, SpawnError};
use futures::StreamExt;
use pin_project::pin_project;

use crate::ScopedSpawn;

#[derive(Debug)]
struct IncomingPad<'sc, T> {
    futures: VecDeque<LocalFutureObj<'sc, T>>,
    waker: Option<Waker>,
}

impl<'sc, T> IncomingPad<'sc, T> {
    fn new() -> Self {
        Self {
            futures: VecDeque::new(),
            waker: None,
        }
    }

    fn push(&mut self, fut: LocalFutureObj<'sc, T>) {
        self.futures.push_back(fut);
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }
}

type IncomingPadRef<'sc, T> = Rc<RefCell<Option<IncomingPad<'sc, T>>>>;

#[derive(Debug)]
pub struct LocalSpawnScope<'sc, T = ()> {
    futures: FuturesUnordered<LocalFutureObj<'sc, T>>,
    incoming: IncomingPadRef<'sc, T>,
    result: Vec<T>,
}

impl<'sc, T> LocalSpawnScope<'sc, T> {
    pub fn new() -> Self {
        Self {
            futures: FuturesUnordered::new(),
            incoming: Rc::new(RefCell::new(Some(IncomingPad::new()))),
            result: Vec::new(),
        }
    }

    pub fn spawner(&self) -> LocalSpawnScopeSpawner<'sc, T> {
        LocalSpawnScopeSpawner {
            scope: self.incoming.clone(),
        }
    }

    pub fn cancel(&mut self) {
        self.drain_incoming();
        self.incoming.borrow_mut().take();
    }

    pub fn until<Fut: Future>(&mut self, fut: Fut) -> Until<'_, 'sc, T, Fut> {
        Until {
            scope: self,
            future: fut,
        }
    }

    pub fn until_stalled(&mut self) -> UntilStalled<'_, 'sc, T> {
        UntilStalled { scope: self }
    }

    pub fn until_empty(&mut self) -> UntilEmpty<'_, 'sc, T> {
        UntilEmpty { scope: self }
    }

    pub fn results(&self) -> &Vec<T> {
        &self.result
    }

    pub fn take_results(mut self) -> Vec<T> {
        mem::take(&mut self.result)
    }

    fn drain_incoming(&mut self) -> bool {
        let mut incoming = self.incoming.borrow_mut();
        if let Some(pad) = incoming.as_mut() {
            pad.waker = None;
            let has_incoming = !pad.futures.is_empty();
            self.futures.extend(pad.futures.drain(..));
            has_incoming
        } else {
            false
        }
    }

    fn register_waker_on_incoming(&mut self, cx: &mut Context<'_>) {
        if let Some(pad) = self.incoming.borrow_mut().as_mut() {
            pad.waker = Some(cx.waker().clone());
        }
    }
}

impl<'sc, T> Drop for LocalSpawnScope<'sc, T> {
    fn drop(&mut self) {
        // Close the stream so that spawners can give a shutdown error
        self.incoming.borrow_mut().take();
    }
}

impl<'sc, T> Default for LocalSpawnScope<'sc, T> {
    fn default() -> Self {
        Self::new()
    }
}

#[pin_project]
#[derive(Debug)]
pub struct Until<'s, 'sc, T, Fut> {
    scope: &'s mut LocalSpawnScope<'sc, T>,
    #[pin]
    future: Fut,
}

impl<'s, 'sc, T, Fut: Future> Future for Until<'s, 'sc, T, Fut> {
    type Output = Fut::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().project();
        let scope = this.scope.deref_mut();

        loop {
            scope.drain_incoming();
            match scope.futures.poll_next_unpin(cx) {
                Poll::Ready(Some(result)) => {
                    scope.result.push(result);
                    continue;
                }
                Poll::Ready(None) => break,
                Poll::Pending => break,
            };
        }

        match this.future.poll(cx) {
            Poll::Ready(result) => Poll::Ready(result),
            Poll::Pending => {
                scope.register_waker_on_incoming(cx);
                Poll::Pending
            }
        }
    }
}

#[derive(Debug)]
pub struct UntilStalled<'s, 'sc, T> {
    scope: &'s mut LocalSpawnScope<'sc, T>,
}

impl<'s, 'sc, T> Future for UntilStalled<'s, 'sc, T> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        let scope = self.scope.deref_mut();
        let waker = noop_waker();
        let mut noop_ctx = Context::from_waker(&waker);
        let mut polled_once = false;
        loop {
            if !scope.drain_incoming() && polled_once {
                return Poll::Ready(());
            }

            match scope.futures.poll_next_unpin(&mut noop_ctx) {
                Poll::Ready(Some(result)) => {
                    scope.result.push(result);
                    continue;
                }
                Poll::Ready(None) => (),
                Poll::Pending => (),
            };
            polled_once = true;
        }
    }
}

#[derive(Debug)]
pub struct UntilEmpty<'s, 'sc, T> {
    scope: &'s mut LocalSpawnScope<'sc, T>,
}

impl<'s, 'sc, T> Future for UntilEmpty<'s, 'sc, T> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let scope = self.scope.deref_mut();
        loop {
            let result = match scope.futures.poll_next_unpin(cx) {
                Poll::Ready(Some(result)) => {
                    scope.result.push(result);
                    continue;
                }
                Poll::Ready(None) => Poll::Ready(()),
                Poll::Pending => Poll::Pending,
            };

            if !scope.drain_incoming() {
                if result.is_pending() {
                    scope.register_waker_on_incoming(cx);
                }
                return result;
            }
        }
    }
}

#[derive(Debug)]
pub struct LocalSpawnScopeSpawner<'sc, T> {
    scope: IncomingPadRef<'sc, T>,
}

impl<'sc, T> Clone for LocalSpawnScopeSpawner<'sc, T> {
    fn clone(&self) -> Self {
        Self {
            scope: self.scope.clone(),
        }
    }
}

impl<'sc, T> LocalSpawnScopeSpawner<'sc, T> {
    pub fn spawn_scoped_local_obj(&self, future: LocalFutureObj<'sc, T>) -> Result<(), SpawnError> {
        let mut incoming = self.scope.borrow_mut();
        if let Some(incoming) = incoming.as_mut() {
            incoming.push(future);
            Ok(())
        } else {
            Err(SpawnError::shutdown())
        }
    }

    pub fn spawn_local_scoped<Fut>(&self, future: Fut) -> Result<(), SpawnError>
    where
        Fut: Future<Output = T> + 'sc,
    {
        self.spawn_scoped_local_obj(LocalFutureObj::new(Box::new(future)))
    }
}

impl<'sc, T> ScopedSpawn<'sc, T> for LocalSpawnScopeSpawner<'sc, T> {
    fn spawn_obj_scoped(&self, future: FutureObj<'sc, T>) -> Result<(), SpawnError> {
        self.spawn_scoped_local_obj(future.into())
    }

    fn status_scoped(&self) -> Result<(), SpawnError> {
        if self.scope.borrow().is_some() {
            Ok(())
        } else {
            Err(SpawnError::shutdown())
        }
    }
}

impl<'sc> LocalSpawn for LocalSpawnScopeSpawner<'sc, ()> {
    fn spawn_local_obj(&self, future: LocalFutureObj<'static, ()>) -> Result<(), SpawnError> {
        self.spawn_scoped_local_obj(future)
    }

    fn status_local(&self) -> Result<(), SpawnError> {
        self.status_scoped()
    }
}

impl<'sc> Spawn for LocalSpawnScopeSpawner<'sc, ()> {
    fn spawn_obj(&self, future: FutureObj<'static, ()>) -> Result<(), SpawnError> {
        self.spawn_obj_scoped(future)
    }

    fn status(&self) -> Result<(), SpawnError> {
        self.status_scoped()
    }
}
