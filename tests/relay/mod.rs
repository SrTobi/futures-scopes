use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::*;
use std::sync::{Arc, Barrier, Mutex};
use std::thread;

use futures::channel::mpsc;
use futures::executor::{block_on, LocalPool, ThreadPool, ThreadPoolBuilder};
use futures::{SinkExt, StreamExt};
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
                })
                .unwrap();
        }
    }
    assert_eq!(1, Arc::strong_count(&counter));
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
