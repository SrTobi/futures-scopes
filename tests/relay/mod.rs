use std::future::pending;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::*;
use std::sync::{Arc, Barrier, Mutex};
use std::thread;

use futures::channel::mpsc;
use futures::executor::{block_on, LocalPool, ThreadPool, ThreadPoolBuilder};
use futures::task::Spawn;
use futures::{SinkExt, StreamExt};
use futures_scopes::ScopedSpawn;
use futures_scopes::relay::{RelayScopeLocalSpawning, RelayScopeSpawning};
use futures_scopes::{new_relay_scope, ScopedSpawnExt, SpawnScope};

#[test]
fn test_mutate_outer() {
    let mut called = true;
    {
        let scope = new_relay_scope!();

        let pool = ThreadPool::new().unwrap();
        pool.spawn_scope(scope);

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
                    pending::<()>().await
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
        let scope = new_relay_scope!();
        pool.spawner().spawn_scope_local(scope);

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
        scope.spawner().spawn_scoped(async move {
            let _conuter = counter;
        }).unwrap();
    }
    assert_eq!(1, Arc::strong_count(&counter));
}


#[test]
fn test_spawner_status() {
    let spawner = {
        let scope = new_relay_scope!();
        let spawner = scope.spawner();
        assert!(spawner.status().is_ok());
        assert!(spawner.status_scoped().is_ok());
        spawner
    };
    assert!(spawner.status().is_err());
    assert!(spawner.status_scoped().is_err());
}

#[test]
fn test_panic_in_spawned() {
    let counter = Arc::new(());
    let scope = new_relay_scope!();

    let pool = ThreadPool::new().unwrap();
    pool.spawn_scope(scope);

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
        pool.spawn_scope(scope);

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
fn test_on_local_pool() {
    let called = Mutex::new(0);
    {
        let scope = new_relay_scope!();

        let mut pool = LocalPool::new();
        pool.spawner().spawn_scope_local(scope);

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
fn test_scope_in_scope() {
    let outer_scope = new_relay_scope!();

    let pool = ThreadPool::new().unwrap();
    pool.spawn_scope(outer_scope);

    let (sx, mut rx) = mpsc::channel(1);

    let outer_spawner = outer_scope.spawner();
    outer_scope
        .spawner()
        .spawn_scoped(async move {
            let inner_scope = new_relay_scope!();
            outer_spawner.spawn_scope(inner_scope);

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

    let scope = new_relay_scope!();
    pool1.spawn_scope(scope);
    pool2.spawn_scope(scope);

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
async fn example(spawn: &(impl Spawn + Clone + Send + 'static)) {
    let counter = AtomicUsize::new(0);

    // Create a scope.
    // Futures spawned on `scope` can reference everything before new_relay_scope!()
    let scope = new_relay_scope!();
    spawn.spawn_scope(scope);

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
