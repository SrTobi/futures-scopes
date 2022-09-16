use crate::ScopedSpawn;

pub trait SpawnScope<'a> {
    type Spawner: ScopedSpawn<'a>;

    fn spawner(&'a self) -> Self::Spawner;
}
