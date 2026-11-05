use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use adamantium::{Entry, Map, OccupiedEntry, Str, VacantEntry};
use indexmap::IndexMap;

#[test]
fn entry_chains_update_or_append_and_preserve_order() {
    for shared in [false, true] {
        let mut map = Map::from([("c", 3), ("a", 1), ("b", 2)]);
        let original = shared.then(|| map.clone());

        *map.entry("a").and_modify(|v| *v += 10).or_insert(99) += 1;
        *map.entry("d").and_modify(|_| panic!("vacant")).or_insert(4) += 1;

        assert!(
            map.iter()
                .map(|(k, v)| (*k, *v))
                .eq([("c", 3), ("a", 12), ("b", 2), ("d", 5),])
        );
        assert!(!map.is_shared());
        if let Some(original) = original {
            assert_eq!(original, Map::from([("c", 3), ("a", 1), ("b", 2)]));
        }
    }
}

#[test]
fn defaults_are_lazy_and_accept_fn_once_closures() {
    for shared in [false, true] {
        let mut map = Map::from([("a", String::from("original"))]);
        let original = shared.then(|| map.clone());
        let mut calls = 0;
        assert_eq!(
            map.entry("a").or_insert_with(|| {
                calls += 1;
                String::from("unused")
            }),
            "original"
        );
        assert_eq!(calls, 0);

        let value = String::from("new");
        map.entry("b").or_insert_with(|| {
            calls += 1;
            value
        });
        assert_eq!(calls, 1);
        assert_eq!(map.get("b").map(String::as_str), Some("new"));

        let suffix = String::from("!");
        map.entry("a")
            .and_modify(|v| v.push_str(&suffix.into_boxed_str()))
            .or_insert_with(|| panic!("occupied"));
        assert_eq!(map.get("a").map(String::as_str), Some("original!"));
        if let Some(original) = original {
            assert_eq!(original.get("a").map(String::as_str), Some("original"));
            assert!(!original.contains_key("b"));
        }
    }
}

#[test]
fn or_default_returns_mutable_access_for_both_variants() {
    for shared in [false, true] {
        let mut map = Map::from([("a", vec![1])]);
        let original = shared.then(|| map.clone());
        map.entry("a").or_default().push(2);
        map.entry("b").or_default().push(3);
        assert_eq!(map.get("a"), Some(&vec![1, 2]));
        assert_eq!(map.get("b"), Some(&vec![3]));
        if let Some(original) = original {
            assert_eq!(original.get("a"), Some(&vec![1]));
            assert!(!original.contains_key("b"));
        }
    }
}

#[test]
fn or_default_constructs_a_value_only_for_vacant_entries() {
    static DEFAULTS: AtomicUsize = AtomicUsize::new(0);

    #[derive(Clone)]
    struct CountedDefault(usize);

    impl Default for CountedDefault {
        fn default() -> Self {
            DEFAULTS.fetch_add(1, Ordering::Relaxed);
            Self(0)
        }
    }

    for shared in [false, true] {
        let mut map = Map::from([("a", CountedDefault(7))]);
        let original = shared.then(|| map.clone());
        let before = DEFAULTS.load(Ordering::Relaxed);

        assert_eq!(map.entry("a").or_default().0, 7);
        assert_eq!(DEFAULTS.load(Ordering::Relaxed), before);

        map.entry("b").or_default().0 += 2;
        assert_eq!(DEFAULTS.load(Ordering::Relaxed), before + 1);
        assert_eq!(map.entry("b").or_default().0, 2);
        assert_eq!(DEFAULTS.load(Ordering::Relaxed), before + 1);

        if let Some(original) = original {
            assert_eq!(original.get("a").map(|v| v.0), Some(7));
            assert!(!original.contains_key("b"));
        }
    }
}

#[test]
fn occupied_handles_expose_and_replace_values_without_moving_keys() {
    for shared in [false, true] {
        let mut map = Map::from([("c", 3), ("a", 1), ("b", 2)]);
        let original = shared.then(|| map.clone());
        let entry = map.entry("a");
        assert_eq!(entry.key(), &"a");
        let Entry::Occupied(mut entry): Entry<'_, &str, i32> = entry else {
            panic!("existing key must be occupied");
        };
        let _: &OccupiedEntry<'_, &str, i32> = &entry;
        assert_eq!(entry.key(), &"a");
        assert_eq!(entry.get(), &1);
        *entry.get_mut() = 10;
        assert_eq!(entry.insert(20), 10);
        *entry.into_mut() += 1;

        assert_eq!(map.get("a"), Some(&21));
        assert!(map.keys().copied().eq(["c", "a", "b"]));
        if let Some(original) = original {
            assert_eq!(original.get("a"), Some(&1));
        }
    }
}

#[test]
fn occupied_removal_preserves_relative_order_and_reinsertion_appends() {
    for shared in [false, true] {
        for return_key in [false, true] {
            let mut map = Map::from([("a", 1), ("b", 2), ("c", 3), ("d", 4)]);
            let original = shared.then(|| map.clone());
            let Entry::Occupied(entry) = map.entry("b") else {
                panic!("existing key must be occupied");
            };
            if return_key {
                assert_eq!(entry.remove_entry(), ("b", 2));
            } else {
                assert_eq!(entry.remove(), 2);
            }
            assert!(map.keys().copied().eq(["a", "c", "d"]));
            map.entry("b").or_insert(20);
            assert!(map.keys().copied().eq(["a", "c", "d", "b"]));
            if let Some(original) = original {
                assert!(original.keys().copied().eq(["a", "b", "c", "d"]));
                assert_eq!(original.get("b"), Some(&2));
            }
        }
    }
}

#[test]
fn vacant_handles_return_the_key_or_append_a_value() {
    for shared in [false, true] {
        for empty in [false, true] {
            let mut map = if empty {
                Map::new()
            } else {
                Map::from([(Str::from("a"), 1)])
            };
            let original = shared.then(|| map.clone());
            let entry = map.entry(Str::from("b"));
            assert_eq!(entry.key().as_str(), "b");
            let Entry::Vacant(entry) = entry else {
                panic!("missing key must be vacant");
            };
            let _: &VacantEntry<'_, Str, i32> = &entry;
            assert_eq!(entry.key().as_str(), "b");
            assert_eq!(entry.into_key(), "b");
            assert!(!map.contains_key("b"));
            assert!(!map.is_shared());

            let Entry::Vacant(entry) = map.entry(Str::from("b")) else {
                panic!("returned key must remain absent");
            };
            *entry.insert(2) += 1;
            assert_eq!(map.last(), Some((&Str::from("b"), &3)));
            assert_eq!(map.len(), if empty { 1 } else { 2 });
            if let Some(original) = original {
                assert!(!original.contains_key("b"));
                assert_eq!(original.len(), usize::from(!empty));
            }
        }
    }
}

#[derive(Debug)]
struct Counted {
    value: usize,
    clones: Arc<AtomicUsize>,
    hashes: Arc<AtomicUsize>,
}

impl Counted {
    fn new(value: usize, clones: &Arc<AtomicUsize>, hashes: &Arc<AtomicUsize>) -> Self {
        Self {
            value,
            clones: Arc::clone(clones),
            hashes: Arc::clone(hashes),
        }
    }
}

impl Clone for Counted {
    fn clone(&self) -> Self {
        self.clones.fetch_add(1, Ordering::Relaxed);
        Self::new(self.value, &self.clones, &self.hashes)
    }
}

impl PartialEq for Counted {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl Eq for Counted {}

impl Hash for Counted {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.hashes.fetch_add(1, Ordering::Relaxed);
        self.value.hash(state);
    }
}

#[test]
fn entry_detaches_once_and_does_not_repeat_lookups_or_clone_the_supplied_key() {
    for shared in [false, true] {
        for key in [1, 3] {
            let keys = Arc::new(AtomicUsize::new(0));
            let values = Arc::new(AtomicUsize::new(0));
            let supplied = Arc::new(AtomicUsize::new(0));
            let hashes = Arc::new(AtomicUsize::new(0));
            let mut map = Map::with_capacity(8);
            map.extend((0..3).map(|i| {
                (
                    Counted::new(i, &keys, &hashes),
                    Counted::new(i, &values, &hashes),
                )
            }));
            let original = shared.then(|| map.clone());
            let storage = std::ptr::from_ref(map.as_index_map());
            hashes.store(0, Ordering::Relaxed);

            let entry = map.entry(Counted::new(key, &supplied, &hashes));
            let expected_clones = if shared { 3 } else { 0 };
            assert_eq!(keys.load(Ordering::Relaxed), expected_clones);
            assert_eq!(values.load(Ordering::Relaxed), expected_clones);
            assert_eq!(supplied.load(Ordering::Relaxed), 0);
            assert_eq!(hashes.load(Ordering::Relaxed), 1);
            assert_eq!(entry.key().value, key);
            assert!(Arc::ptr_eq(
                &entry.key().clones,
                if key == 1 { &keys } else { &supplied }
            ));

            entry
                .and_modify(|v| v.value += 10)
                .or_insert_with(|| Counted::new(20, &values, &hashes));
            assert_eq!(hashes.load(Ordering::Relaxed), 1);
            map.entry(Counted::new(key, &supplied, &hashes))
                .or_insert_with(|| panic!("now occupied"));
            assert_eq!(hashes.load(Ordering::Relaxed), 2);
            assert_eq!(keys.load(Ordering::Relaxed), expected_clones);
            assert_eq!(values.load(Ordering::Relaxed), expected_clones);
            assert_eq!(supplied.load(Ordering::Relaxed), 0);
            assert!(!map.is_shared());
            assert_eq!(std::ptr::from_ref(map.as_index_map()) == storage, !shared);
            assert_eq!(map.len(), if key == 1 { 3 } else { 4 });
            assert_eq!(
                map.get_index(key).map(|(_, v)| v.value),
                Some(if key == 1 { 11 } else { 20 })
            );
            if let Some(original) = original {
                assert!(original.values().map(|v| v.value).eq(0..3));
                assert!(original.keys().map(|k| k.value).eq(0..3));
            }
        }
    }
}

#[test]
fn inspecting_or_dropping_an_entry_still_detaches_shared_storage() {
    for key in [1, 3] {
        let keys = Arc::new(AtomicUsize::new(0));
        let values = Arc::new(AtomicUsize::new(0));
        let hashes = Arc::new(AtomicUsize::new(0));
        let original: Map<_, _> = (0..3)
            .map(|i| {
                (
                    Counted::new(i, &keys, &hashes),
                    Counted::new(i, &values, &hashes),
                )
            })
            .collect();
        let mut map = original.clone();
        let entry = map.entry(Counted::new(key, &keys, &hashes));
        assert_eq!(keys.load(Ordering::Relaxed), 3);
        assert_eq!(values.load(Ordering::Relaxed), 3);
        assert_eq!(entry.key().value, key);
        drop(entry);
        assert!(!map.is_shared());
        assert!(!original.is_shared());
        assert_eq!(map, original);
        assert!(map.keys().map(|k| k.value).eq(0..3));
    }
}

#[test]
fn entry_updates_work_with_existing_conversions_and_owned_iteration() {
    let entries = [(Str::from("c"), 3), (Str::from("a"), 1)];
    let ordered = IndexMap::from(entries.clone());
    let unordered = HashMap::from(entries.clone());
    let maps = [
        Map::from(entries.clone()),
        Map::from(ordered),
        Map::from(unordered),
        entries.into_iter().collect(),
    ];
    for mut map in maps {
        let order = map.keys().cloned().collect::<Vec<_>>();
        let original = map.clone();
        map.entry("a".into()).and_modify(|v| *v = 10).or_insert(0);
        map.entry("b".into()).or_insert(2);
        let expected: Vec<_> = order.into_iter().chain([Str::from("b")]).collect();
        assert_eq!(map.keys().cloned().collect::<Vec<_>>(), expected);
        assert_eq!(original.get("a"), Some(&1));
        assert!(!original.contains_key("b"));

        let index_map: IndexMap<_, _> = map.clone().into();
        assert_eq!(index_map.keys().cloned().collect::<Vec<_>>(), expected);
        assert_eq!(map.clone().into_index_map(), index_map);
        assert_eq!(map.into_iter().collect::<IndexMap<_, _>>(), index_map);
        assert_eq!(index_map.get("a"), Some(&10));
        assert_eq!(index_map.get("b"), Some(&2));
    }
}

#[cfg(feature = "serde")]
#[test]
fn entries_work_before_and_after_serialization() {
    let mut map: Map<Str, i32> = serde_json::from_str(r#"{"b":2,"a":1}"#).unwrap();
    let original = map.clone();
    map.entry("b".into()).and_modify(|v| *v += 1).or_insert(0);
    map.entry("c".into()).or_insert(4);
    assert_eq!(
        serde_json::to_string(&map).unwrap(),
        r#"{"b":3,"a":1,"c":4}"#
    );
    assert_eq!(
        serde_json::to_string(&original).unwrap(),
        r#"{"b":2,"a":1}"#
    );
    let mut back: Map<Str, i32> =
        serde_json::from_str(&serde_json::to_string(&map).unwrap()).unwrap();
    *back.entry("b".into()).or_default() += 1;
    assert_eq!(back.get("b"), Some(&4));
    assert!(back.keys().map(Str::as_str).eq(["b", "a", "c"]));
}
