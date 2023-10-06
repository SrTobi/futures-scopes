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

/// A spawn scope that can be used to spawn non-send/sync futures of lifetime `'sc`.
///
/// Spawned futures are not processed until the scope is polled by awaiting one of the
/// futures returned by [`until`](Self::until), [`until_stalled`](Self::until_stalled) or [`until_empty`](Self::until_stalled).
///
/// Spawned futures can return a result of type `T` that can be accessed via [`results`](Self::results) or [`take_results`](Self::take_results).
///
/// # Dropping
/// If the scope is dropped, all futures that where spawned on it will be dropped as well.
///
/// # Examples
///
/// ```
/// # use futures::future::FutureExt;
/// # use futures::task::LocalSpawnExt;
/// # use futures::executor::block_on;
/// use futures_scopes::local::LocalSpawnScope;
///
/// let some_value = &42;
///
/// let mut scope = LocalSpawnScope::<usize>::new();
/// let spawner = scope.spawner();
/// spawner.spawn_local_scoped(async {
///   // You can reference `some_value` here
///   *some_value
/// }).unwrap();
///
/// // Process the scope and wait until all futures have been completed
/// block_on(scope.until_empty());
///
/// // use `scope.results()` to access the results of the spawned futures
/// assert_eq!(scope.results(), &[42]);
/// ```
///
#[derive(Debug)]
pub struct LocalSpawnScope<'sc, T = ()> {
    futures: FuturesUnordered<LocalFutureObj<'sc, T>>,
    incoming: IncomingPadRef<'sc, T>,
    result: Vec<T>,
}

impl<'sc, T> LocalSpawnScope<'sc, T> {
    /// Creates a new spawn scope.
    ///
    /// Spawned futures can reference anything before the creation of the scope.
    pub fn new() -> Self {
        Self {
            futures: FuturesUnordered::new(),
            incoming: Rc::new(RefCell::new(Some(IncomingPad::new()))),
            result: Vec::new(),
        }
    }

    /// Get a cloneable spawner that can be used to spawn local futures to this scope.
    ///
    /// This spawner can live longer then the scope.
    /// In case a future is spawned after the scope has been dropped, the spawner will return [`SpawnError::shutdown`].
    pub fn spawner(&self) -> LocalSpawnScopeSpawner<'sc, T> {
        LocalSpawnScopeSpawner {
            scope: self.incoming.clone(),
        }
    }

    /// Drops all spawned futures and prevents new futures from being spawned.
    ///
    /// In case a future is spawned after `cancel` has been called, the spawner will return [`SpawnError::shutdown`].
    ///
    /// # Examples
    ///
    /// ```
    /// # use futures::future::FutureExt;
    /// # use futures::task::LocalSpawnExt;
    /// # use futures::task::SpawnError;
    /// # use futures::executor::block_on;
    /// use futures_scopes::local::LocalSpawnScope;
    ///
    /// let mut scope = LocalSpawnScope::new();
    /// let spawner = scope.spawner();
    /// spawner.spawn_local_scoped(async {
    ///  // ...
    /// }).unwrap();
    ///
    /// scope.cancel();
    /// let res = spawner.spawn_local_scoped(async { () });
    ///
    /// assert!(matches!(res, Err(err) if err.is_shutdown()));
    /// ```
    pub fn cancel(&mut self) {
        self.drain_incoming();
        self.incoming.borrow_mut().take();
    }

    /// Returns a future that polls the scope until the given future `fut` is ready.
    ///
    /// The returned future will guarantee that at least some progress is made
    /// on the futures spawned on this scope, by polling all these futures at least once.
    ///
    /// # Examples
    /// ```
    /// # use futures::future::{FutureExt, pending};
    /// # use futures::task::LocalSpawnExt;
    /// # use futures::executor::block_on;
    /// # use std::cell::RefCell;
    /// # use futures_scopes::local::LocalSpawnScope;
    /// let counter = RefCell::new(0);
    ///
    /// let mut scope = LocalSpawnScope::new();
    /// let spawner = scope.spawner();
    /// for _ in 0..10 {
    ///   spawner.spawn_local_scoped(async {
    ///     *counter.borrow_mut() += 1;
    ///   }).unwrap();
    /// }
    ///
    /// // Spawning a never-ready future will not hinder .until from returning
    /// spawner.spawn_local_scoped(pending()).unwrap();
    ///
    /// block_on(scope.until(async {
    ///   // at least one future has been polled ready
    ///   assert!(*counter.borrow() == 10);
    /// }));
    /// ```
    ///
    pub fn until<Fut: Future>(&mut self, fut: Fut) -> Until<'_, 'sc, T, Fut> {
        Until {
            scope: self,
            future: fut,
        }
    }

    /// Returns a future that polls the scope until no further progress can be made,
    /// because all spawned futures are pending or the scope is empty.
    ///
    /// Guarantees that all spawned futures are polled at least once.
    ///
    /// # Examples
    /// ```
    /// # use futures::future::{FutureExt, pending};
    /// # use futures::task::LocalSpawnExt;
    /// # use futures::executor::block_on;
    /// # use std::cell::RefCell;
    /// # use futures::channel::oneshot;
    /// # use futures_scopes::local::LocalSpawnScope;
    /// let counter = RefCell::new(0);
    /// let (sx, rx) = oneshot::channel();
    ///
    /// let mut scope = LocalSpawnScope::new();
    /// scope.spawner().spawn_local_scoped(async {
    ///   *counter.borrow_mut() += 1;
    ///
    ///    // wait until the oneshot is ready
    ///   rx.await.unwrap();
    ///
    ///   *counter.borrow_mut() += 1;
    /// }).unwrap();
    ///
    /// assert!(*counter.borrow() == 0);
    ///
    /// block_on(scope.until_stalled());
    ///
    /// // scope is stalled because the future is waiting for rx
    /// assert!(*counter.borrow() == 1);
    /// sx.send(()).unwrap();
    ///
    /// block_on(scope.until_stalled());
    ///
    /// // scope is empty because the future has finished
    /// assert!(*counter.borrow() == 2);
    /// ```
    ///
    pub fn until_stalled(&mut self) -> UntilStalled<'_, 'sc, T> {
        UntilStalled { scope: self }
    }

    /// Returns a future that polls the scope until all spawned futures have been polled ready.
    ///
    /// # Examples
    /// ```
    /// # use futures::future::{FutureExt, pending};
    /// # use futures::task::LocalSpawnExt;
    /// # use futures::executor::block_on;
    /// # use std::cell::RefCell;
    /// # use futures_scopes::local::LocalSpawnScope;
    /// let counter = RefCell::new(0);
    ///
    /// let mut scope = LocalSpawnScope::new();
    /// let spawner = scope.spawner();
    /// for _ in 0..10 {
    ///   spawner.spawn_local_scoped(async {
    ///     *counter.borrow_mut() += 1;
    ///   }).unwrap();
    /// }
    ///
    /// // Spawning a never-ready future will block .until_empty from ever returning
    /// // spawner.spawn_local_scoped(pending()).unwrap();
    ///
    /// block_on(scope.until_empty());
    ///
    /// // all futures have been polled ready
    /// assert!(*counter.borrow() == 10);
    /// ```
    ///
    pub fn until_empty(&mut self) -> UntilEmpty<'_, 'sc, T> {
        UntilEmpty { scope: self }
    }

    /// Returns a reference to the results of all futures that have been polled ready until now.
    /// 
    /// Results are not ordered in any specific way.
    ///
    /// To take ownership of the results, use [`take_results`](Self::take_results).
    ///
    /// # Examples
    ///
    /// ```
    /// # use futures::future::{FutureExt, pending};
    /// # use futures::task::LocalSpawnExt;
    /// # use futures::executor::block_on;
    /// # use futures_scopes::local::LocalSpawnScope;
    ///
    /// let mut scope = LocalSpawnScope::new();
    /// let spawner = scope.spawner();
    /// for i in 0..5 {
    ///     spawner.spawn_local_scoped(async move {
    ///        i
    ///    }).unwrap();
    /// }
    ///
    /// block_on(scope.until_empty());
    ///
    /// assert_eq!(scope.results(), &[0, 1, 2, 3, 4]);
    pub fn results(&self) -> &Vec<T> {
        &self.result
    }

    /// Returns the results of all futures that have been polled ready until now.
    /// This removes the results from the scope.
    /// This does not hinder future results from being added to the scope.
    ///
    /// Results are not ordered in any specific way.
    ///
    /// To **not** take ownership of the results, use [`results`](Self::results).
    ///
    /// # Examples
    ///
    /// ```
    /// # use futures::future::{FutureExt, pending};
    /// # use futures::task::LocalSpawnExt;
    /// # use futures::executor::block_on;
    /// # use futures_scopes::local::LocalSpawnScope;
    ///
    /// let mut scope = LocalSpawnScope::new();
    /// let spawner = scope.spawner();
    /// for i in 0..5 {
    ///     spawner.spawn_local_scoped(async move {
    ///        i
    ///    }).unwrap();
    /// }
    ///
    /// block_on(scope.until_empty());
    ///
    /// assert_eq!(scope.take_results(), vec![0, 1, 2, 3, 4]);
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

/// Future returned by [`until`](LocalSpawnScope::until).
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

/// Future returned by [`until_stalled`](LocalSpawnScope::until_stalled).
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

/// Future returned by [`until_empty`](LocalSpawnScope::until_empty).
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

/// A spawner that can be obtained from [`LocalSpawnScope::spawner`].
/// 
/// This spawner may live longer then the scope.
/// In case a future is spawned after the scope has been canceled or dropped,
/// the spawner will return [`SpawnError::shutdown`].
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
    /// Spawns a task that polls the given local future.
    /// 
    /// # Errors
    ///
    /// This method returns a [`Result`] that contains a [`SpawnError`] if spawning fails.
    pub fn spawn_scoped_local_obj(&self, future: LocalFutureObj<'sc, T>) -> Result<(), SpawnError> {
        let mut incoming = self.scope.borrow_mut();
        if let Some(incoming) = incoming.as_mut() {
            incoming.push(future);
            Ok(())
        } else {
            Err(SpawnError::shutdown())
        }
    }

    /// Spawns a task that polls the given local future.
    /// 
    /// # Errors
    ///
    /// This method returns a [`Result`] that contains a [`SpawnError`] if spawning fails.
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
