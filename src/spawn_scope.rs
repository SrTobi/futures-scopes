use crate::ScopedSpawn;

pub trait SpawnScope<'a, T> {
    type Spawner: ScopedSpawn<'a, T>;

    fn spawner(&self) -> Self::Spawner;
}
