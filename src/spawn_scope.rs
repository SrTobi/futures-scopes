use crate::ScopedSpawn;

pub trait SpawnScope<'a, T> {
    type Spawner: ScopedSpawn<'a, T>;

    fn spawner(&'a self) -> Self::Spawner;
}
