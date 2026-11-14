#[path = "../benches/support/persistent.rs"]
mod persistent;

use adamantium::{Array, Deque, Map, Set};
use persistent::{Collection, PersistentDeque, SIZES};

#[allow(
    clippy::panic,
    clippy::unreachable,
    reason = "test workload exhaustiveness checks"
)]
fn verify<T: Collection>() {
    for size in SIZES {
        let original = T::build(size);
        let original_sum = size * (size - 1) / 2;
        let expected_sum = match T::NAME {
            "array" | "map" => original_sum + size - size / 2,
            "deque" | "set" => original_sum + size,
            other => panic!("unrecognized workload {other}"),
        };
        assert_eq!(original.checksum(), original_sum);
        assert_eq!(original.lookup(size / 2), Some(size / 2));
        let mut changed = original.clone();
        changed.edit(size);
        assert_eq!(
            changed.checksum(),
            expected_sum,
            "{} {} {size}",
            T::NAME,
            T::BACKEND
        );
        assert_eq!(original.checksum(), original_sum);
        match T::NAME {
            "array" | "map" => assert_eq!(changed.lookup(size / 2), Some(size)),
            "deque" => {
                assert_eq!(changed.lookup(0), Some(1));
                assert_eq!(changed.lookup(size - 1), Some(size));
            }
            "set" => assert_eq!(changed.lookup(size), Some(size)),
            _ => unreachable!(),
        }
        let mut unique = T::build(size);
        unique.edit(size);
        assert_eq!(unique.checksum(), expected_sum);
        persistent::profile_snapshots(&original, size, 2);
        assert_eq!(original.checksum(), original_sum);
    }
}

#[test]
fn array_workload() {
    verify::<Array<usize>>();
    verify::<im::Vector<usize>>();
}
#[test]
fn deque_workload() {
    verify::<Deque<usize>>();
    verify::<PersistentDeque>();
}
#[test]
fn map_workload() {
    verify::<Map<usize, usize>>();
    verify::<im::HashMap<usize, usize>>();
}
#[test]
fn set_workload() {
    verify::<Set<usize>>();
    verify::<im::HashSet<usize>>();
}
