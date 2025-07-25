mod fuzzy;

use std::future::pending;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::*;
use std::sync::{Arc, Barrier, Mutex};
use std::thread;

use futures::channel::{mpsc, oneshot};
use futures::executor::{LocalPool, ThreadPool, ThreadPoolBuilder, block_on};
use futures::task::Spawn;
use futures::task::SpawnExt;
use futures::{SinkExt, StreamExt};
use futures_scopes::local::LocalScope;
use futures_scopes::relay::{RelayScope, RelayScopeLocalSpawning, RelayScopeSpawning, new_relay_scope};
use futures_scopes::{ScopedSpawn, ScopedSpawnExt, SpawnScope};

#[test]
fn test_mutate_outer() {
    let mut called = true;
    {
        let scope = new_relay_scope!();

        let pool = ThreadPool::new().unwrap();
        pool.spawn_scope(scope).unwrap();

        scope
            .spawner()
            .spawn_scoped(async {
                called = true;
            })
            .unwrap();

        block_on(scope.until_empty());
    }
    assert!(called);
}

#[test]
fn test_drop_without_spawner() {
    let counter = Arc::new(());
    {
        let scope = new_relay_scope!();

        for _ in 0..50 {
            let counter = counter.clone();
            scope
                .spawner()
                .spawn_scoped(async move {
                    let _counter = counter;
                    pending::<()>().await;
                })
                .unwrap();
        }
    }
    assert_eq!(1, Arc::strong_count(&counter));
}

#[test]
fn test_futures_are_dropped() {
    let mut pool = LocalPool::new();
    let counter = Arc::new(());
    {
        let scope = new_relay_scope!(pool.spawner());

        for _ in 0..50 {
            let counter = counter.clone();
            scope
                .spawner()
                .spawn_scoped(async move {
                    let _counter = counter;
                    pending::<()>().await;
                })
                .unwrap();
        }
        pool.run_until_stalled();

        let counter = counter.clone();
        scope
            .spawner()
            .spawn_scoped(async move {
                let _counter = counter;
            })
            .unwrap();
    }
    assert_eq!(1, Arc::strong_count(&counter));
}

#[test]
fn test_spawner_status() {
    let spawner = {
        let scope = new_relay_scope!();
        let spawner = scope.spawner();
        spawner.status().unwrap();
        spawner.status_scoped().unwrap();
        spawner
    };
    spawner.status().unwrap_err();
    spawner.status_scoped().unwrap_err();
}

#[test]
fn test_panic_in_spawned() {
    let counter = Arc::new(());
    let pool = ThreadPool::new().unwrap();
    let scope = new_relay_scope!(pool);

    let inner_counter = counter.clone();
    scope
        .spawner()
        .spawn_scoped(async {
            let _counter = inner_counter;
            panic!("test panic in spawned scoped future");
        })
        .unwrap();

    block_on(scope.until_empty());

    assert_eq!(1, Arc::strong_count(&counter), "inner_counter should have been dropped");
}

#[test]
fn test_can_continue_after_panic() {
    let mut called = false;
    {
        let scope = new_relay_scope!();

        let pool = ThreadPoolBuilder::new().pool_size(2).create().unwrap();
        pool.spawn_scope(scope).unwrap();

        scope
            .spawner()
            .spawn_scoped(async {
                panic!("test panic in spawned scoped future");
            })
            .unwrap();

        scope
            .spawner()
            .spawn_scoped(async {
                called = true;
            })
            .unwrap();

        block_on(scope.until_empty());
    }

    assert!(called);
}

#[test]
fn test_spawn_with_handle() {
    let scope = new_relay_scope!();
    let pool = ThreadPoolBuilder::new().pool_size(2).create().unwrap();
    pool.spawn_scope(scope).unwrap();

    let handle = scope.spawner().spawn_with_handle(async { 42 }).unwrap();
    assert_eq!(block_on(handle), 42);
}

#[test]
fn test_on_local_pool() {
    let called = Mutex::new(0);
    {
        let mut pool = LocalPool::new();
        let scope = new_relay_scope!(pool.spawner());

        for _ in 0..50 {
            scope
                .spawner()
                .spawn_scoped(async {
                    *called.lock().unwrap() += 1;
                })
                .unwrap();
        }

        pool.run_until(scope.until_empty());
    }

    assert_eq!(called.into_inner().unwrap(), 50);
}

#[test]
fn test_pool_drop() {
    let called = &Mutex::new(0);
    {
        let scope = new_relay_scope!();

        let (sx, rx) = oneshot::channel();

        scope
            .spawner()
            .spawn_scoped(async move {
                *called.lock().unwrap() += 1;
                let _ = rx.await;
                *called.lock().unwrap() += 1;
            })
            .unwrap();

        {
            assert_eq!(*called.lock().unwrap(), 0);
            let mut pool = LocalPool::new();
            pool.spawner().spawn_scope(scope).unwrap();
            let did_run_one = pool.try_run_one();
            assert!(did_run_one);
            assert_eq!(*called.lock().unwrap(), 1);

            sx.send(()).unwrap();
        }

        {
            let mut pool = LocalPool::new();
            scope.relay_to_local(&pool.spawner()).unwrap();
            pool.run_until(scope.until_empty());
            assert_eq!(*called.lock().unwrap(), 2);
        }
    }
}

#[test]
fn test_waiting() {
    fn build_task_chain(
        scope: &RelayScope<'static>,
        prev: oneshot::Receiver<usize>,
        n: usize,
    ) -> oneshot::Receiver<usize> {
        if n == 0 {
            prev
        } else {
            let (sx, rx) = oneshot::channel();
            let spawn = || {
                scope
                    .spawner()
                    .spawn_scoped(async move {
                        let prev = prev.await.unwrap();
                        sx.send(prev + 1).unwrap();
                    })
                    .unwrap();
            };

            // Scrumble up the order of the tasks
            if n % 2 == 0 {
                let rx = build_task_chain(scope, rx, n - 1);
                spawn();
                rx
            } else {
                spawn();
                build_task_chain(scope, rx, n - 1)
            }
        }
    }

    const N: usize = 100;

    let scope = RelayScope::new();
    let (sx, rx) = oneshot::channel();
    let rx = build_task_chain(&scope, rx, N);

    let mut pool = LocalPool::new();
    scope.relay_to_local(&pool.spawner()).unwrap();

    pool.run_until(async move {
        sx.send(0).unwrap();
        //println!("sent");
        assert_eq!(rx.await.unwrap(), N);
    });
}

#[test]
fn test_scope_in_scope() {
    let pool = ThreadPool::new().unwrap();
    let outer_scope = new_relay_scope!(pool);

    let (sx, mut rx) = mpsc::channel(1);

    let outer_spawner = outer_scope.spawner();
    outer_scope
        .spawner()
        .spawn_scoped(async move {
            let inner_scope = new_relay_scope!();
            outer_spawner.spawn_scope(inner_scope).unwrap();

            for i in 0..1000 {
                let mut sx = sx.clone();
                inner_scope
                    .spawner()
                    .spawn_scoped(async move {
                        //println!("running {}", i);
                        if i == 100 {
                            sx.send(()).await.unwrap();
                        }
                    })
                    .unwrap();
            }

            rx.next().await;
            //println!("got message");
        })
        .unwrap();

    block_on(outer_scope.until_empty());
}

#[test]
fn test_scope_in_scope2() {
    fn inner(outer: &RelayScope<'_>) {
        let scope = new_relay_scope!(outer.spawner());
        scope.relay_to(&outer.spawner()).unwrap();
    }

    {
        let scope = new_relay_scope!();
        inner(scope);
    }

    fn inner_local(outer: &LocalScope<'_>) {
        let scope = new_relay_scope!(outer.spawner());
        scope.relay_to_local(&outer.spawner()).unwrap();
    }

    inner_local(&LocalScope::new());
}

#[test]
fn test_run_on_multiple_pools() {
    let barrier = Arc::new(Barrier::new(3));
    let id1 = Arc::new(Mutex::new(None));
    let id2 = Arc::new(Mutex::new(None));

    let (pool1, pool2) = {
        let id1 = id1.clone();
        let barrier1 = barrier.clone();
        let pool1 = ThreadPool::builder()
            .pool_size(1)
            .after_start(move |i| {
                assert_eq!(0, i);
                let id = thread::current().id();
                let _ = id1.lock().unwrap().insert(id);
                barrier1.wait();
            })
            .create()
            .unwrap();

        let id2 = id2.clone();
        let barrier2 = barrier.clone();
        let pool2 = ThreadPool::builder()
            .pool_size(1)
            .after_start(move |i| {
                assert_eq!(0, i);
                let id = thread::current().id();
                let _ = id2.lock().unwrap().insert(id);
                barrier2.wait();
            })
            .create()
            .unwrap();

        (pool1, pool2)
    };

    barrier.wait();
    let id1 = id1.lock().unwrap().unwrap();
    let id2 = id2.lock().unwrap().unwrap();

    let count1 = AtomicUsize::new(0);
    let count2 = AtomicUsize::new(0);

    let scope = new_relay_scope!(pool1, pool2);

    while count1.load(Relaxed) < 10 && count2.load(Relaxed) < 10 {
        scope
            .spawner()
            .spawn_scoped(async {
                let id = thread::current().id();
                if id == id1 {
                    count1.fetch_add(1, Relaxed);
                } else if id == id2 {
                    count2.fetch_add(1, Relaxed);
                } else {
                    panic!("Shouldn't happen");
                }
            })
            .unwrap();
    }
}

// An async example function that has access to some kind of spawner
#[allow(clippy::print_stdout)]
async fn example(spawn: &(impl Spawn + Clone + Send + 'static)) {
    let counter = AtomicUsize::new(0);

    // Create a scope.
    // Futures spawned on `scope` can reference everything before new_relay_scope!()
    let scope = new_relay_scope!();
    spawn.spawn_scope(scope).unwrap();

    // Create a new Spawner that spawns futures on the scope.
    // `spawner` is a Spawn+Clone+Send+'static,
    // so another nested scope can be spawned inside our scope
    let spawner = scope.spawner();

    for i in 1..=100 {
        // Tell rust not to move the counter into the async fn
        let counter = &counter;
        let fut = async move {
            for _ in 0..100 {
                // `counter` is not moved into the future but referenced
                // `i`, however, was moved(copied) into the future
                counter.fetch_add(i, Relaxed);
            }
        };

        // spawn the future on the scope
        spawner.spawn_scoped(fut).unwrap();
    }

    // Wait until all futures have been processed.
    // This does not block the thread, but returns a future that we can await!
    scope.until_empty().await;

    // Gives: 505000
    println!("Counter: {}", counter.load(SeqCst));

    // The scope is dropped here.
    // If we wouldn't have waited for all futures to be processed,
    // the drop would stop the execution of all scope-futures and drop them.
    // The drop blocks the current thread only minimally
    // until all currently running futures are have left their poll method
    // and all futures are destroyed.
}

#[test]
fn test_example() {
    let pool = ThreadPool::new().unwrap();
    block_on(example(&pool));
}
