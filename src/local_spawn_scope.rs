use futures::{
    stream::FuturesUnordered,
    task::{noop_waker, FutureObj, LocalFutureObj, LocalSpawn, Spawn, SpawnError},
    StreamExt,
};
use pin_project_lite::pin_project;
use std::{
    cell::RefCell,
    collections::VecDeque,
    future::Future,
    ops::DerefMut,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};

use crate::ScopedSpawn;

type IncomingPad<'sc> = Rc<RefCell<Option<VecDeque<LocalFutureObj<'sc, ()>>>>>;

pub struct LocalSpawnScope<'sc> {
    futures: FuturesUnordered<LocalFutureObj<'sc, ()>>,
    incoming: IncomingPad<'sc>,
}

impl<'sc> LocalSpawnScope<'sc> {
    pub fn new() -> Self {
        Self {
            futures: FuturesUnordered::new(),
            incoming: Rc::new(RefCell::new(Some(VecDeque::new()))),
        }
    }

    pub fn spawner(&self) -> LocalSpawnScopeSpawner<'sc> {
        LocalSpawnScopeSpawner {
            scope: self.incoming.clone(),
        }
    }

    pub fn until<Fut: Future>(&mut self, fut: Fut) -> Until<'_, 'sc, Fut> {
        Until {
            scope: self,
            future: fut,
        }
    }

    pub fn cancel(&mut self) {
        self.drain_incoming();
        self.incoming.borrow_mut().take();
    }

    pub fn until_stalled(&mut self) -> UntilStalled<'_, 'sc> {
        UntilStalled { scope: self }
    }

    pub fn until_empty(&mut self) -> UntilEmpty<'_, 'sc> {
        UntilEmpty { scope: self }
    }

    fn drain_incoming(&mut self) -> bool {
        let mut incoming = self.incoming.borrow_mut();
        if let Some(incoming) = incoming.as_mut() {
            let has_incoming = !incoming.is_empty();
            self.futures.extend(incoming.drain(..));
            has_incoming
        } else {
            false
        }
    }
}

impl<'sc> Drop for LocalSpawnScope<'sc> {
    fn drop(&mut self) {
        // Close the stream so that spawners can give a shutdown error
        self.incoming.borrow_mut().take();
    }
}

pin_project! {
    pub struct Until<'s, 'sc, Fut> {
        scope: &'s mut LocalSpawnScope<'sc>,
        #[pin]
        future: Fut,
    }
}

impl<'s, 'sc, Fut: Future> Future for Until<'s, 'sc, Fut> {
    type Output = Fut::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().project();
        let scope = this.scope.deref_mut();

        match this.future.poll(cx) {
            Poll::Ready(result) => return Poll::Ready(result),
            Poll::Pending => (),
        }

        loop {
            scope.drain_incoming();
            match scope.futures.poll_next_unpin(cx) {
                Poll::Ready(Some(())) => continue,
                Poll::Ready(None) => return Poll::Pending,
                Poll::Pending => return Poll::Pending,
            };
        }
    }
}

pub struct UntilStalled<'s, 'sc> {
    scope: &'s mut LocalSpawnScope<'sc>,
}

impl<'s, 'sc> Future for UntilStalled<'s, 'sc> {
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
                Poll::Ready(Some(())) => continue,
                Poll::Ready(None) => (),
                Poll::Pending => (),
            };
            polled_once = true;
        }
    }
}

pub struct UntilEmpty<'s, 'sc> {
    scope: &'s mut LocalSpawnScope<'sc>,
}

impl<'s, 'sc> Future for UntilEmpty<'s, 'sc> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let scope = self.scope.deref_mut();
        loop {
            let result = match scope.futures.poll_next_unpin(cx) {
                Poll::Ready(Some(())) => continue,
                Poll::Ready(None) => Poll::Ready(()),
                Poll::Pending => Poll::Pending,
            };

            if !scope.drain_incoming() {
                return result;
            }
        }
    }
}

#[derive(Clone)]
pub struct LocalSpawnScopeSpawner<'sc> {
    scope: IncomingPad<'sc>,
}

impl<'sc> LocalSpawnScopeSpawner<'sc> {
    pub fn spawn_scoped_local_obj(
        &self,
        future: LocalFutureObj<'sc, ()>,
    ) -> Result<(), SpawnError> {
        let mut incoming = self.scope.borrow_mut();
        if let Some(incoming) = incoming.as_mut() {
            incoming.push_back(future);
            Ok(())
        } else {
            Result::Err(SpawnError::shutdown())
        }
    }

    pub fn spawn_local_scoped<Fut>(&self, future: Fut) -> Result<(), SpawnError>
    where
        Fut: Future<Output = ()> + 'sc,
    {
        self.spawn_scoped_local_obj(LocalFutureObj::new(Box::new(future)))
    }
}

impl<'sc> ScopedSpawn<'sc> for LocalSpawnScopeSpawner<'sc> {
    fn spawn_obj_scoped(&self, future: FutureObj<'sc, ()>) -> Result<(), SpawnError> {
        self.spawn_scoped_local_obj(future.into())
    }
}

impl<'sc> LocalSpawn for LocalSpawnScopeSpawner<'sc> {
    fn spawn_local_obj(&self, future: LocalFutureObj<'static, ()>) -> Result<(), SpawnError> {
        self.spawn_scoped_local_obj(future)
    }
}

impl<'sc> Spawn for LocalSpawnScopeSpawner<'sc> {
    fn spawn_obj(&self, future: FutureObj<'static, ()>) -> Result<(), SpawnError> {
        self.spawn_obj_scoped(future)
    }
}

#[cfg(test)]
mod tests {
    use futures::{channel::oneshot, executor::block_on, task::SpawnExt};

    use super::LocalSpawnScope;

    #[test]
    fn test_local_spawn_scope() {
        struct Test(i32);
        let org = Test(3);
        let value = &org;
        let mut spawn = LocalSpawnScope::new();
        let (sx, rx) = oneshot::channel::<i32>();
        let spawner = spawn.spawner();
        let inner_spawner = spawn.spawner();
        spawner
            .spawn_local_scoped(async move {
                let mut inner_scope = LocalSpawnScope::new();

                inner_scope
                    .spawner()
                    .spawn_local_scoped(async {
                        println!("  inner scope");
                        inner_spawner
                            .spawn_local_scoped(async { println!("from inner scope") })
                            .unwrap();
                    })
                    .unwrap();

                inner_scope
                    .spawner()
                    .spawn_local_scoped(async {
                        println!("  inner scope2");
                        inner_spawner
                            .spawn_local_scoped(async { println!("from inner scope2") })
                            .unwrap();
                    })
                    .unwrap();


                println!("outer {}", value.0);
                inner_spawner
                    .spawn_local_scoped(async move {
                        sx.send(10).unwrap();
                        println!("inner {}", value.0);
                        //pending::<()>().await;
                    })
                    .unwrap();

                println!("before inner_scope.until.empty()");
                inner_scope.until_empty().await;
                println!("after inner_scope.until.empty()");
            })
            .unwrap();

        let result = block_on(async {
            let (sx2, rx2) = oneshot::channel::<i32>();
            spawner
                .spawn(async move {
                    println!("before rx.await");
                    rx2.await.unwrap();
                    println!("after rx.await");
                })
                .unwrap();
            println!("before until_stalled");
            spawn.until_stalled().await;
            println!("after until_stalled");
            spawn.cancel();
            sx2.send(3).unwrap();
            let result = spawn.until(rx).await.unwrap();
            println!("after until");
            spawn.until_empty().await;
            result
        });

        println!("{}", result);
    }
}
