use std::{
    collections::HashSet,
    sync::atomic::{AtomicUsize, Ordering},
};

use adamantium::{Set, Str};
use indexmap::IndexSet;

#[test]
fn insertion_removal_and_borrowed_lookup_preserve_order() {
    let mut set = Set::with_capacity(4);
    assert!(set.is_empty());
    assert_eq!(set.first(), None);
    assert_eq!(set.last(), None);
    assert!(set.insert(Str::from("c")));
    assert!(set.insert(Str::from("a")));
    assert!(set.insert(Str::from("b")));
    assert!(!set.insert(Str::from("a")));
    assert_eq!(set.len(), 3);
    assert!(set.contains("a"));
    assert_eq!(set.get("a").map(Str::as_str), Some("a"));
    assert_eq!(set.get("missing"), None);
    assert_eq!(set.first().map(Str::as_str), Some("c"));
    assert_eq!(set.last().map(Str::as_str), Some("b"));
    assert_eq!(set.get_index(1).map(Str::as_str), Some("a"));
    assert_eq!(set.get_index(3), None);

    assert!(set.remove("a"));
    assert!(!set.remove("missing"));
    assert!(set.insert(Str::from("a")));
    assert_eq!(
        set.iter().map(Str::as_str).collect::<Vec<_>>(),
        ["c", "b", "a"]
    );
    assert_eq!(format!("{set:?}"), r#"{"c", "b", "a"}"#);
}

#[test]
fn every_mutation_keeps_shared_original_unchanged() {
    let original = Set::from([3, 1, 2]);

    let mut inserted = original.clone();
    assert!(original.is_shared());
    assert!(std::ptr::eq(
        original.as_index_set(),
        inserted.as_index_set()
    ));
    inserted.insert(4);
    assert!(!original.is_shared());
    assert!(!inserted.is_shared());
    assert_eq!(inserted.into_iter().collect::<Vec<_>>(), [3, 1, 2, 4]);

    let mut removed = original.clone();
    assert!(removed.remove(&1));
    assert_eq!(removed.into_iter().collect::<Vec<_>>(), [3, 2]);

    let mut retained = original.clone();
    retained.retain(|value| *value != 1);
    assert_eq!(retained.into_iter().collect::<Vec<_>>(), [3, 2]);

    let mut cleared = original.clone();
    cleared.clear();
    assert!(cleared.is_empty());

    let mut extended = original.clone();
    extended.extend([1, 4, 4, 5]);
    assert_eq!(extended.into_iter().collect::<Vec<_>>(), [3, 1, 2, 4, 5]);

    let mut direct = original.clone();
    direct.make_mut().shift_remove(&3);
    assert_eq!(direct.into_iter().collect::<Vec<_>>(), [1, 2]);
    assert_eq!(original.into_iter().collect::<Vec<_>>(), [3, 1, 2]);
}

#[test]
fn writes_and_ownership_conversion_clone_only_when_needed() {
    static CLONES: AtomicUsize = AtomicUsize::new(0);

    #[derive(Debug, Hash, PartialEq, Eq)]
    struct Counted(u16);

    impl Clone for Counted {
        fn clone(&self) -> Self {
            CLONES.fetch_add(1, Ordering::SeqCst);
            Self(self.0)
        }
    }

    {
        let original: Set<_> = (0..1_000).map(Counted).collect();
        let mut duplicate = original.clone();
        assert!(!duplicate.insert(Counted(500)));
        assert_eq!(CLONES.load(Ordering::SeqCst), 0);
        assert!(duplicate.is_shared());
        assert!(std::ptr::eq(
            original.as_index_set(),
            duplicate.as_index_set()
        ));

        let mut missing = original.clone();
        assert!(!missing.remove(&Counted(1_000)));
        assert_eq!(CLONES.load(Ordering::SeqCst), 0);
        assert!(missing.is_shared());
        assert!(std::ptr::eq(
            original.as_index_set(),
            missing.as_index_set()
        ));

        let mut cleared = original.clone();
        cleared.clear();
        assert!(cleared.is_empty());
        assert!(!cleared.is_shared());
        assert_eq!(original.len(), 1_000);
        assert_eq!(CLONES.load(Ordering::SeqCst), 0);
    }

    let mut original = Set::from([Counted(1), Counted(2)]);
    original.insert(Counted(3));
    original.remove(&Counted(2));
    original.retain(|_| true);
    original.extend([Counted(4)]);
    original.make_mut().insert(Counted(5));
    assert_eq!(CLONES.load(Ordering::SeqCst), 0);

    let mut changed = original.clone();
    assert_eq!(CLONES.load(Ordering::SeqCst), 0);
    changed.insert(Counted(6));
    assert_eq!(CLONES.load(Ordering::SeqCst), 4);
    changed.remove(&Counted(1));
    changed.clear();
    assert_eq!(CLONES.load(Ordering::SeqCst), 4);

    let shared = original.clone().into_index_set();
    assert_eq!(CLONES.load(Ordering::SeqCst), 8);
    let owned: IndexSet<_> = original.into();
    assert_eq!(owned, shared);
    assert_eq!(CLONES.load(Ordering::SeqCst), 8);
    assert_eq!(Set::from(owned).into_iter().count(), 4);
    assert_eq!(CLONES.load(Ordering::SeqCst), 8);
}

#[test]
fn equality_compares_membership_and_operations_have_predictable_order() {
    let left = Set::from([3, 1, 2, 1]);
    let right = Set::from([2, 4, 3, 5]);
    assert_eq!(left, Set::from([2, 3, 1]));
    assert_ne!(left, right);
    assert_ne!(left, Set::from([3, 1]));
    assert_eq!(
        left.union(&right).copied().collect::<Vec<_>>(),
        [3, 1, 2, 4, 5]
    );
    assert_eq!(
        left.intersection(&right).copied().collect::<Vec<_>>(),
        [3, 2]
    );
    assert_eq!(left.difference(&right).copied().collect::<Vec<_>>(), [1]);
    assert_eq!(
        left.symmetric_difference(&right)
            .copied()
            .collect::<Vec<_>>(),
        [1, 4, 5]
    );
    let union: Set<_> = left.union(&right).copied().collect();
    assert!(union.is_superset(&left));
    assert!(left.is_subset(&union));
    assert!(!right.is_subset(&left));
    assert!(!left.is_superset(&right));
    assert!(!left.is_disjoint(&right));
    assert!(left.is_disjoint(&Set::from([4, 5])));
    assert_eq!(left.iter().copied().collect::<Vec<_>>(), [3, 1, 2]);
    assert_eq!(right.iter().copied().collect::<Vec<_>>(), [2, 4, 3, 5]);
}

#[test]
fn operations_cover_empty_and_identical_sets() {
    let empty = Set::<u8>::default();
    let values = Set::from([2, 1]);
    assert!(empty.is_subset(&values));
    assert!(values.is_superset(&empty));
    assert!(empty.is_disjoint(&empty));
    assert_eq!(empty.union(&values).copied().collect::<Vec<_>>(), [2, 1]);
    assert_eq!(values.union(&empty).copied().collect::<Vec<_>>(), [2, 1]);
    assert_eq!(empty.intersection(&values).count(), 0);
    assert_eq!(empty.difference(&values).count(), 0);
    assert_eq!(
        values.difference(&empty).copied().collect::<Vec<_>>(),
        [2, 1]
    );
    assert_eq!(
        values
            .symmetric_difference(&empty)
            .copied()
            .collect::<Vec<_>>(),
        [2, 1]
    );
    assert_eq!(values.union(&values).count(), 2);
    assert_eq!(values.intersection(&values).count(), 2);
    assert_eq!(values.difference(&values).count(), 0);
    assert_eq!(values.symmetric_difference(&values).count(), 0);
}

#[test]
fn conversions_deduplicate_and_preserve_source_iteration_order() {
    let collected: Set<_> = [3, 1, 3, 2].into_iter().collect();
    assert_eq!(
        (&collected).into_iter().copied().collect::<Vec<_>>(),
        [3, 1, 2]
    );
    let index_set = IndexSet::from([3, 1, 2]);
    assert_eq!(
        Set::from(index_set).into_iter().collect::<Vec<_>>(),
        [3, 1, 2]
    );

    let hash_set = HashSet::from([3, 1, 2]);
    let source_order = hash_set.iter().copied().collect::<Vec<_>>();
    assert_eq!(
        Set::from(hash_set).into_iter().collect::<Vec<_>>(),
        source_order
    );

    let shared = collected.clone();
    assert_eq!(shared.into_iter().collect::<Vec<_>>(), [3, 1, 2]);
    assert_eq!(collected.into_iter().collect::<Vec<_>>(), [3, 1, 2]);
}

#[test]
fn construction_reads_and_cheap_clones_do_not_require_clone_elements() {
    #[derive(Debug, Hash, PartialEq, Eq)]
    struct NonClone(u8);

    let original: Set<_> = [NonClone(2), NonClone(1), NonClone(2)]
        .into_iter()
        .collect();
    let cloned = original.clone();
    assert!(original.is_shared());
    assert_eq!(original, cloned);
    assert!(cloned.contains(&NonClone(1)));
    assert_eq!(cloned.get(&NonClone(2)), Some(&NonClone(2)));
    assert_eq!(cloned.first(), Some(&NonClone(2)));
    assert_eq!(cloned.last(), Some(&NonClone(1)));
    assert_eq!(cloned.get_index(2), None);
    assert_eq!(cloned.iter().count(), 2);
    assert_eq!(original.union(&cloned).count(), 2);
    assert_eq!(original.intersection(&cloned).count(), 2);
    assert_eq!(original.difference(&cloned).count(), 0);
    assert_eq!(original.symmetric_difference(&cloned).count(), 0);
}

#[cfg(feature = "serde")]
#[test]
fn serde_uses_insertion_order_and_deduplicates_without_affecting_clones() {
    let original = Set::from([Str::from("b"), Str::from("a")]);
    let json = serde_json::to_string(&original).unwrap();
    assert_eq!(json, r#"["b","a"]"#);
    let mut back: Set<Str> = serde_json::from_str(&json).unwrap();
    assert_eq!(back, original);
    assert!(!back.is_shared());
    let cloned = back.clone();
    back.insert(Str::from("c"));
    assert_eq!(serde_json::to_string(&cloned).unwrap(), json);
    assert_eq!(serde_json::to_string(&back).unwrap(), r#"["b","a","c"]"#);

    let duplicates: Set<Str> = serde_json::from_str(r#"["b","a","b","c","a"]"#).unwrap();
    assert_eq!(duplicates, back);
    assert_eq!(
        duplicates.iter().map(Str::as_str).collect::<Vec<_>>(),
        ["b", "a", "c"]
    );
    assert_eq!(serde_json::to_string(&Set::<u8>::new()).unwrap(), "[]");
    assert!(serde_json::from_str::<Set<u8>>("[]").unwrap().is_empty());
    assert!(serde_json::from_str::<Set<u8>>("{}").is_err());
}
