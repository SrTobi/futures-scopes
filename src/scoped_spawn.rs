use futures::task::{FutureObj, SpawnError};

pub trait ScopedSpawn<'a> {
    fn spawn_scoped(&self, future: FutureObj<'a, ()>) -> Result<(), SpawnError>;

    #[inline]
    fn status_scoped(&self) -> Result<(), SpawnError> {
        Ok(())
    }
}
