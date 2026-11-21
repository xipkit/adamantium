use std::hint::black_box;

use adamantium::{Array, Deque, Map, Set};

pub const SIZES: [usize; 7] = [16, 64, 256, 1_024, 4_096, 16_384, 65_536];

pub trait Collection: Clone {
    const NAME: &'static str;
    const BACKEND: &'static str;
    fn build(size: usize) -> Self;
    fn edit(&mut self, size: usize);
    fn lookup(&self, index: usize) -> Option<usize>;
    fn checksum(&self) -> usize;
}

macro_rules! collection {
    ($ty:ty, $name:literal, $backend:literal, $build:expr, $edit:expr, $lookup:expr, $sum:expr) => {
        impl Collection for $ty {
            const NAME: &'static str = $name;
            const BACKEND: &'static str = $backend;
            fn build(size: usize) -> Self {
                ($build)(size)
            }
            fn edit(&mut self, size: usize) {
                ($edit)(self, size);
            }
            fn lookup(&self, index: usize) -> Option<usize> {
                ($lookup)(self, index)
            }
            fn checksum(&self) -> usize {
                ($sum)(self)
            }
        }
    };
}

collection!(
    Array<usize>,
    "array",
    "cow",
    |n| (0..n).collect(),
    |v: &mut Self, n| {
        v.set(black_box(n / 2), black_box(n)).ok();
    },
    |v: &Self, i| v.get(i).copied(),
    |v: &Self| v.iter().copied().sum()
);

collection!(
    im::Vector<usize>,
    "array",
    "im",
    |n| (0..n).collect(),
    |v: &mut Self, n| {
        v.set(black_box(n / 2), black_box(n));
    },
    |v: &Self, i| v.get(i).copied(),
    |v: &Self| v.iter().copied().sum()
);

collection!(
    Deque<usize>,
    "deque",
    "cow",
    |n| {
        let mut v = Self::with_capacity(n + 1);
        v.extend(0..n);
        v
    },
    |v: &mut Self, n| {
        black_box(v.pop_front());
        v.push_back(black_box(n));
    },
    |v: &Self, i| v.get(i).copied(),
    |v: &Self| v.iter().copied().sum()
);

/// Uses a persistent vector for the deque benchmark.
#[derive(Clone)]
pub struct PersistentDeque(im::Vector<usize>);
collection!(
    PersistentDeque,
    "deque",
    "im",
    |n| Self((0..n).collect()),
    |v: &mut Self, n| {
        black_box(v.0.pop_front());
        v.0.push_back(black_box(n));
    },
    |v: &Self, i| v.0.get(i).copied(),
    |v: &Self| v.0.iter().copied().sum()
);

collection!(Map<usize, usize>, "map", "cow",
    |n| (0..n).map(|i| (i, i)).collect(),
    |v: &mut Self, n| { black_box(v.insert(black_box(n / 2), black_box(n))); },
    |v: &Self, i| v.get(&i).copied(),
    |v: &Self| v.values().copied().sum());

collection!(im::HashMap<usize, usize>, "map", "im",
    |n| (0..n).map(|i| (i, i)).collect(),
    |v: &mut Self, n| { black_box(v.insert(black_box(n / 2), black_box(n))); },
    |v: &Self, i| v.get(&i).copied(),
    |v: &Self| v.values().copied().sum());

collection!(
    Set<usize>,
    "set",
    "cow",
    |n| {
        let mut v = Self::with_capacity(n + 1);
        v.extend(0..n);
        v
    },
    |v: &mut Self, n| {
        black_box(v.insert(black_box(n)));
    },
    |v: &Self, i| v.contains(&i).then_some(i),
    |v: &Self| v.iter().copied().sum()
);

collection!(
    im::HashSet<usize>,
    "set",
    "im",
    |n| (0..n).collect(),
    |v: &mut Self, n| {
        black_box(v.insert(black_box(n)));
    },
    |v: &Self, i| v.contains(&i).then_some(i),
    |v: &Self| v.iter().copied().sum()
);

/// Clones, edits, and drops one snapshot.
pub fn snapshot_edit<T: Collection>(original: &T, size: usize) {
    let mut changed = black_box(original).clone();
    changed.edit(size);
    drop(black_box(changed));
}

/// Runs the edit loop measured by Callgrind.
#[inline(never)]
pub fn profile_snapshots<T: Collection>(original: &T, size: usize, repeats: usize) {
    for _ in 0..repeats {
        snapshot_edit(original, size);
    }
}
