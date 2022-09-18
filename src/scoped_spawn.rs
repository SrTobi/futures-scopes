use std::future::Future;

use futures::task::{FutureObj, SpawnError};

pub trait ScopedSpawn<'a, T> {
    fn spawn_obj_scoped(&self, future: FutureObj<'a, T>) -> Result<(), SpawnError>;

    #[inline]
    fn status_scoped(&self) -> Result<(), SpawnError> {
        Ok(())
    }
}

pub trait ScopedSpawnExt<'a, T>: ScopedSpawn<'a, T> {
    fn spawn_scoped<Fut>(&self, future: Fut) -> Result<(), SpawnError>
    where
        Fut: Future<Output = T> + Send + 'a,
    {
        self.spawn_obj_scoped(FutureObj::new(Box::new(future)))
    }
}

impl<'sc, T, Sp: ?Sized> ScopedSpawnExt<'sc, T> for Sp where Sp: ScopedSpawn<'sc, T> {}
