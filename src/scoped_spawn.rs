use futures::task::{FutureObj, Spawn, SpawnError};
use std::future::Future;

pub trait ScopedSpawn<'a>: Spawn {
    fn spawn_obj_scoped(&self, future: FutureObj<'a, ()>) -> Result<(), SpawnError>;

    #[inline]
    fn status_scoped(&self) -> Result<(), SpawnError> {
        Ok(())
    }
}

pub trait ScopedSpawnExt<'a>: ScopedSpawn<'a> {
    fn spawn_scoped<Fut>(&self, future: Fut) -> Result<(), SpawnError>
    where
        Fut: Future<Output = ()> + Send + 'a,
    {
        self.spawn_obj_scoped(FutureObj::new(Box::new(future)))
    }
}
