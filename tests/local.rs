use std::future::pending;
use std::rc::Rc;

use futures::channel::oneshot::channel;
use futures::executor::block_on;
use futures::{FutureExt, select_biased};
use futures_scopes::local::LocalScope;

#[test]
fn test_mutate_outer() {
    let mut called = true;
    {
        let mut scope = LocalScope::new();

        scope
            .spawner()
            .spawn_local_scoped(async {
                called = true;
            })
            .unwrap();

        block_on(scope.until_empty());
    }
    assert!(called);
}

#[test]
fn test_drop_without_spawner() {
    let counter = Rc::new(());
    {
        let scope = LocalScope::new();

        for _ in 0..50 {
            let counter = counter.clone();
            scope
                .spawner()
                .spawn_local_scoped(async move {
                    let _counter = counter;
                    pending::<()>().await;
                })
                .unwrap();
        }
    }
    assert_eq!(1, Rc::strong_count(&counter));
}

#[test]
fn test_spawn_outside_until_empty() {
    let mut scope = LocalScope::new();
    let spawner = scope.spawner();

    let (sx, rx) = channel();

    let f = async {
        // at this point scope.until_empty().fuse() should have been polled
        // and returned pending
        // Let's test if it can wake up when spawning a new future
        spawner
            .spawn_local_scoped(async {
                sx.send(()).unwrap();
            })
            .unwrap();
        pending::<()>().await;
    };

    block_on(async {
        spawner
            .spawn_local_scoped(async {
                rx.await.unwrap();
            })
            .unwrap();

        scope.until_stalled().await;

        select_biased! {
          _ = scope.until_empty().fuse() => (),
          _ = f.fuse() => (),
        };
    });
}

#[test]
fn test_spawn_outside_until() {
    let mut scope = LocalScope::new();
    let spawner = scope.spawner();

    let (sx, rx) = channel();

    let f = async {
        // at this point scope.until(rx).fuse() should have been polled
        // and returned pending
        // Let's test if it can wake up when spawning a new future
        spawner
            .spawn_local_scoped(async {
                sx.send(()).unwrap();
            })
            .unwrap();
        pending::<()>().await;
    };

    block_on(async {
        select_biased! {
          _ = scope.until(rx).fuse() => (),
          _ = f.fuse() => (),
        };
    });
}

#[test]
fn test_spawn_local_with_handle() {
    let mut scope = LocalScope::new();
    let handle = scope.spawner().spawn_local_scoped_with_handle(async { 42 }).unwrap();
    assert_eq!(block_on(scope.until(handle)), 42);
}
