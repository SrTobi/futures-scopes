[![Build](https://github.com/SrTobi/futures-scopes/actions/workflows/rust.yml/badge.svg)](https://github.com/SrTobi/futures-scopes)
[![Creates.io](https://img.shields.io/crates/v/futures-scopes?style)](https://crates.io/crates/futures-scopes)
[![Docs](https://docs.rs/futures-scopes/badge.svg)](https://docs.rs/futures-scopes/)

# futures-scopes

An extension to [futures-rs](https://github.com/rust-lang/futures-rs) that offers scopes.
Scopes can be used to spawn non-static futures that can reference variables on the stack
that where created before the scope was created.
The scope will relay these futures onto one of multiple underlying spawns.
This enables the combination of multiple Spawns into a single one.
When the scope is dropped, all futures of that spawn will be immediately dropped as well.

As a scope is a `Spawn` itself, it can spawn further nested scopes inside it.

## Example

```rust
// An async example function that has access to some kind of spawner
async fn example(spawn: &(impl Spawn + Clone + Send + 'static)) {
    let counter = AtomicUsize::new(0);

    // Create a scope.
    // Futures spawned on `scope` can reference everything before new_relay_scope!()
    let scope = new_relay_scope!();

    // We spawn the new scope on the given spawn.
    // This could be a ThreadPool, for example.
    // It is also possible to spawn on multiple Spawns to share the work between them
    //
    // The scope will spawn a single future to the given Spawn.
    // This future will self-replicate to fill the Spawn, but not overwhelm it either.
    spawn.spawn_scope(scope);

    // Create a new Spawner that spawns futures on the scope.
    // `spawner` is Spawn+Clone+Send+'static,
    // so another nested scope can be spawned inside our scope
    // with `spawner.spawn_scope(another_nested_scope)`.
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

    // Wait until all futures have been finished.
    // This does not block the thread, but returns a future that we can await!
    scope.until_empty().await;

    // Counter: 505000
    println!("Counter: {}", counter.load(SeqCst));

    // The scope is dropped here.
    // If we wouldn't have waited for all futures to be processed,
    // the drop would stop the execution of all scope-futures and drop them.
    // The drop blocks the current thread only minimally
    // until all currently running futures are have left their poll method
    // and all futures are destroyed.
}

fn main() {
    // Run `example` with a ThreadPool.
    let pool = ThreadPool::new().unwrap();
    block_on(example(&pool));
}
```