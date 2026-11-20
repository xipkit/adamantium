use std::collections::VecDeque;

use adamantium::{Actor, BoundedMap, Inserted, Str, random};

fn order<K: Clone, V>(map: &BoundedMap<K, V>) -> Vec<K> {
    map.iter().map(|(key, _)| key.clone()).collect()
}

fn reverse_order<K: Clone, V>(map: &BoundedMap<K, V>) -> Vec<K> {
    map.iter().rev().map(|(key, _)| key.clone()).collect()
}

#[test]
fn capacity_zero_holds_nothing_and_returns_every_insert() {
    let mut map = BoundedMap::new(0);
    assert_eq!(map.capacity(), 0);
    assert!(map.is_full());
    assert_eq!(map.insert("a", 1), Inserted::Evicted("a", 1));
    assert_eq!(map.insert("a", 2), Inserted::Evicted("a", 2));
    assert!(map.is_empty());
    assert_eq!(map.len(), 0);
    assert_eq!(map.get("a"), None);
    assert_eq!(map.peek("a"), None);
    assert!(!map.contains_key("a"));
    assert_eq!(map.remove("a"), None);
    assert_eq!(map.pop_lru(), None);
    assert_eq!(map.iter().next(), None);
    assert_eq!(map.iter().next_back(), None);
}

#[test]
fn capacity_one_replaces_its_entry_on_every_new_key() {
    let mut map = BoundedMap::new(1);
    assert!(!map.is_full());
    assert_eq!(map.insert("a", 1), Inserted::Added);
    assert!(map.is_full());
    assert_eq!(map.insert("a", 2), Inserted::Replaced(1));
    assert_eq!(map.insert("b", 3), Inserted::Evicted("a", 2));
    assert_eq!(order(&map), ["b"]);
    assert_eq!(reverse_order(&map), ["b"]);
    assert_eq!(map.get("b"), Some(&3));
    assert_eq!(map.remove("b"), Some(3));
    assert!(map.is_empty());
    assert_eq!(map.insert("c", 4), Inserted::Added);
    assert_eq!(map.pop_lru(), Some(("c", 4)));
    assert_eq!(map.pop_lru(), None);
}

#[test]
fn replacing_a_value_refreshes_it_without_evicting() {
    let mut map = BoundedMap::new(3);
    for (key, value) in [("a", 1), ("b", 2), ("c", 3)] {
        assert_eq!(map.insert(key, value), Inserted::Added);
    }
    assert_eq!(map.insert("a", 10), Inserted::Replaced(1));
    assert_eq!(map.len(), 3);
    assert_eq!(order(&map), ["a", "c", "b"]);
    assert_eq!(map.insert("d", 4), Inserted::Evicted("b", 2));
    assert_eq!(order(&map), ["d", "a", "c"]);
    assert_eq!(map.peek("a"), Some(&10));
}

#[test]
fn replacing_keeps_the_stored_key() {
    #[derive(Clone, Debug)]
    struct Key(u8, &'static str);
    impl PartialEq for Key {
        fn eq(&self, other: &Self) -> bool {
            self.0 == other.0
        }
    }
    impl Eq for Key {}
    impl std::hash::Hash for Key {
        fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
            self.0.hash(state);
        }
    }

    let mut map = BoundedMap::new(2);
    drop(map.insert(Key(1, "first"), 'a'));
    assert_eq!(map.insert(Key(1, "second"), 'b'), Inserted::Replaced('a'));
    assert_eq!(
        map.iter().map(|(key, _)| key.1).collect::<Vec<_>>(),
        ["first"]
    );
}

#[test]
fn get_and_get_mut_refresh_but_peek_and_contains_do_not() {
    let mut map = BoundedMap::new(3);
    for key in ["a", "b", "c"] {
        drop(map.insert(key, 0));
    }
    assert_eq!(order(&map), ["c", "b", "a"]);

    assert_eq!(map.peek("a"), Some(&0));
    assert!(map.contains_key("a"));
    assert_eq!(map.iter().count(), 3);
    assert_eq!(order(&map), ["c", "b", "a"]);

    assert_eq!(map.get("a"), Some(&0));
    assert_eq!(order(&map), ["a", "c", "b"]);

    if let Some(value) = map.get_mut("b") {
        *value = 7;
    }
    assert_eq!(order(&map), ["b", "a", "c"]);
    assert_eq!(reverse_order(&map), ["c", "a", "b"]);

    assert_eq!(map.get("z"), None);
    assert_eq!(map.get_mut("z"), None);
    assert_eq!(order(&map), ["b", "a", "c"]);

    assert_eq!(map.insert("d", 0), Inserted::Evicted("c", 0));
    assert_eq!(map.insert("e", 0), Inserted::Evicted("a", 0));
    assert_eq!(map.insert("f", 0), Inserted::Evicted("b", 7));
}

#[test]
fn getting_the_most_recent_entry_keeps_the_order() {
    let mut map = BoundedMap::new(3);
    for key in ["a", "b", "c"] {
        drop(map.insert(key, ()));
    }
    assert_eq!(map.get("c"), Some(&()));
    assert_eq!(order(&map), ["c", "b", "a"]);
    assert_eq!(reverse_order(&map), ["a", "b", "c"]);
}

#[test]
fn removing_from_each_position_keeps_the_links() {
    let keys = ["a", "b", "c", "d", "e"];
    for touched in 0..keys.len() {
        for removed in keys {
            let mut map = BoundedMap::new(keys.len());
            for key in keys {
                drop(map.insert(key, key));
            }
            for key in keys.iter().cycle().skip(touched).take(keys.len()) {
                assert_eq!(map.get(*key), Some(key));
            }
            let mut expected = order(&map);
            expected.retain(|key| *key != removed);

            assert_eq!(map.remove(removed), Some(removed));
            assert_eq!(map.remove(removed), None);
            assert_eq!(order(&map), expected);
            let mut reversed = expected.clone();
            reversed.reverse();
            assert_eq!(reverse_order(&map), reversed);
            for key in &expected {
                assert_eq!(map.peek(*key), Some(key));
            }

            for key in reversed {
                assert_eq!(map.pop_lru(), Some((key, key)));
            }
            assert!(map.is_empty());
        }
    }
}

#[test]
fn clear_empties_the_map_and_keeps_the_capacity() {
    let mut map = BoundedMap::new(2);
    drop(map.insert(1, 'a'));
    drop(map.insert(2, 'b'));
    map.clear();
    assert!(map.is_empty());
    assert_eq!(map.capacity(), 2);
    assert_eq!(map.iter().next(), None);
    assert_eq!(map.pop_lru(), None);
    assert_eq!(map.insert(3, 'c'), Inserted::Added);
    assert_eq!(map.insert(4, 'd'), Inserted::Added);
    assert_eq!(map.insert(5, 'e'), Inserted::Evicted(3, 'c'));
}

#[test]
fn lookups_borrow_the_key() {
    let mut map = BoundedMap::new(2);
    drop(map.insert(Str::from("token"), 1));
    assert_eq!(map.get("token"), Some(&1));
    assert_eq!(map.peek("token"), Some(&1));
    assert!(map.contains_key("token"));
    assert_eq!(map.get_mut("token").map(|value| *value), Some(1));
    assert_eq!(map.remove("token"), Some(1));
}

#[test]
fn iteration_meets_in_the_middle_and_reports_its_length() {
    let mut map = BoundedMap::new(4);
    for key in 1..=4 {
        drop(map.insert(key, key * 10));
    }
    let mut iter = map.iter();
    assert_eq!(iter.len(), 4);
    assert_eq!(iter.next(), Some((&4, &40)));
    assert_eq!(iter.next_back(), Some((&1, &10)));
    assert_eq!(iter.len(), 2);
    assert_eq!(iter.next_back(), Some((&2, &20)));
    assert_eq!(iter.next(), Some((&3, &30)));
    assert_eq!(iter.next(), None);
    assert_eq!(iter.next_back(), None);
    assert_eq!((&map).into_iter().count(), 4);
}

#[test]
fn debug_shows_the_capacity_and_the_entries_by_recency() {
    let mut map = BoundedMap::new(3);
    drop(map.insert("a", 1));
    drop(map.insert("b", 2));
    assert_eq!(
        format!("{map:?}"),
        r#"BoundedMap { capacity: 3, entries: {"b": 2, "a": 1} }"#
    );
    assert_eq!(format!("{:?}", map.iter()), r#"[("b", 2), ("a", 1)]"#);
}

#[test]
fn clones_are_independent() {
    let mut map = BoundedMap::new(2);
    drop(map.insert(1, Str::from("one")));
    let mut copy = map.clone();
    drop(copy.insert(2, Str::from("two")));
    drop(copy.insert(3, Str::from("three")));
    assert_eq!(order(&map), [1]);
    assert_eq!(order(&copy), [3, 2]);
}

#[test]
fn is_send_sync_and_static() {
    fn assert_send_sync<T: Send + Sync + 'static>() {}
    assert_send_sync::<BoundedMap<Str, Str>>();
    assert_send_sync::<Inserted<Str, Str>>();
    assert_send_sync::<Actor<BoundedMap<Str, Str>>>();
}

#[test]
fn works_inside_an_actor() {
    let cache = Actor::new(BoundedMap::new(2));
    let evicted = cache.with(|cache| {
        drop(cache.insert("a", 1));
        drop(cache.insert("b", 2));
        assert_eq!(cache.get("a"), Some(&1));
        cache.insert("c", 3)
    });
    assert_eq!(evicted, Inserted::Evicted("b", 2));
    assert_eq!(cache.read(BoundedMap::len), 2);
    assert_eq!(cache.read(|cache| cache.peek("a").copied()), Some(1));
}

#[test]
fn a_huge_capacity_allocates_only_what_is_used() {
    let mut map = BoundedMap::new(usize::MAX);
    for key in 0..100 {
        assert_eq!(map.insert(key, ()), Inserted::Added);
    }
    assert_eq!(map.len(), 100);
    assert!(!map.is_full());
}

struct Model {
    capacity: usize,
    entries: VecDeque<(u8, u32)>,
}

impl Model {
    fn position(&self, key: u8) -> Option<usize> {
        self.entries.iter().position(|(k, _)| *k == key)
    }

    fn touch(&mut self, key: u8) -> Option<&mut u32> {
        let position = self.position(key)?;
        let entry = self.entries.remove(position)?;
        self.entries.push_front(entry);
        self.entries.front_mut().map(|(_, value)| value)
    }

    fn insert(&mut self, key: u8, value: u32) -> Inserted<u8, u32> {
        if let Some(old) = self.touch(key) {
            return Inserted::Replaced(std::mem::replace(old, value));
        }
        if self.capacity == 0 {
            return Inserted::Evicted(key, value);
        }
        let evicted = if self.entries.len() == self.capacity {
            self.entries.pop_back()
        } else {
            None
        };
        self.entries.push_front((key, value));
        match evicted {
            Some((key, value)) => Inserted::Evicted(key, value),
            None => Inserted::Added,
        }
    }
}

fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn run_against_model(seed: u64) {
    let mut state = seed | 1;
    let capacity = usize::try_from(next(&mut state) % 7).unwrap_or(0);
    let mut map = BoundedMap::new(capacity);
    let mut model = Model {
        capacity,
        entries: VecDeque::new(),
    };
    for step in 0..400 {
        let key = u8::try_from(next(&mut state) % 10).unwrap_or(0);
        let value = u32::try_from(next(&mut state) % 1000).unwrap_or(0);
        let context = format!("seed {seed}, capacity {capacity}, step {step}");
        match next(&mut state) % 20 {
            0..=6 => assert_eq!(
                map.insert(key, value),
                model.insert(key, value),
                "{context}"
            ),
            7..=9 => assert_eq!(
                map.get(&key).copied(),
                model.touch(key).copied(),
                "{context}"
            ),
            10..=11 => {
                let got = map.get_mut(&key).map(|v| {
                    *v += 1;
                    *v
                });
                let want = model.touch(key).map(|v| {
                    *v += 1;
                    *v
                });
                assert_eq!(got, want, "{context}");
            }
            12 => {
                let want = model.position(key).and_then(|p| model.entries.get(p));
                assert_eq!(map.peek(&key), want.map(|(_, v)| v), "{context}");
                assert_eq!(map.contains_key(&key), want.is_some(), "{context}");
            }
            13..=15 => {
                let want = model
                    .position(key)
                    .and_then(|p| model.entries.remove(p))
                    .map(|(_, v)| v);
                assert_eq!(map.remove(&key), want, "{context}");
            }
            16..=18 => assert_eq!(map.pop_lru(), model.entries.pop_back(), "{context}"),
            _ => {
                map.clear();
                model.entries.clear();
            }
        }
        let forward: VecDeque<_> = map.iter().map(|(k, v)| (*k, *v)).collect();
        assert_eq!(forward, model.entries, "{context}");
        let mut backward: Vec<_> = map.iter().rev().map(|(k, v)| (*k, *v)).collect();
        backward.reverse();
        assert_eq!(model.entries, backward, "{context}");
        assert_eq!(map.len(), model.entries.len(), "{context}");
        assert_eq!(map.is_full(), model.entries.len() == capacity, "{context}");
    }
}

#[test]
fn matches_a_naive_list_over_random_operations() {
    for seed in 0..300 {
        run_against_model(seed);
    }
    for _ in 0..50 {
        run_against_model(random::u64());
    }
}
