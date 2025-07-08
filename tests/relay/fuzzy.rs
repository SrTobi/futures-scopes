use std::hint::black_box;

use futures::{
    FutureExt, SinkExt, StreamExt,
    executor::{ThreadPool, block_on},
    future::BoxFuture,
    task::SpawnExt,
};
use futures_scopes::{
    ScopedSpawnExt, SpawnScope,
    relay::{RelayScopeSpawner, new_relay_scope},
};

async fn func(spawner: RelayScopeSpawner<'static>, depth: usize) {
    let (sx1, rx1) = futures::channel::oneshot::channel::<()>();
    let (sx2, mut rx2) = futures::channel::mpsc::channel::<()>(1000);
    let rx1: futures::future::Shared<futures::channel::oneshot::Receiver<()>> = rx1.shared();
    const N: usize = 20;
    for i in 0..N {
        let rx1 = rx1.clone();
        let mut sx2 = sx2.clone();
        let inner_spawner = spawner.clone();
        spawner
            .spawn_scoped(async move {
                rx1.await.unwrap();

                let inner_scope = new_relay_scope!(inner_spawner);

                if depth > 0 {
                    let fut = call_func(inner_scope.spawner(), depth - 1);

                    if i % 4 > 0 {
                        fut.await;
                    }
                }
                if i % 5 == 0 {
                    for _ in 0..10000 {
                        black_box(());
                    }
                }
                if i % 2 == 0 {
                    sx2.send(()).await.ok();
                }

                // if i % 10 == 3 {
                //     panic!("test panic");
                // }
            })
            .unwrap();
    }

    sx1.send(()).ok();

    for _ in 0..N / 2 {
        rx2.next().await.unwrap();
    }
}

fn call_func(spawner: RelayScopeSpawner<'static>, depth: usize) -> BoxFuture<'static, ()> {
    Box::pin(func(spawner, depth))
}

#[test]
fn test_relay_fuzzy() {
    let pool = ThreadPool::new().unwrap();
    let scope = new_relay_scope!(pool);

    scope.spawner().spawn(func(scope.spawner(), 3)).unwrap();

    block_on(scope.until_empty());
}
