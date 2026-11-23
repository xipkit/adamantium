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

#[cfg(feature = "tokio")]
mod expiry_sync {
    use std::time::Duration;

    use adamantium::{BoundedMap, Inserted};

    #[test]
    fn with_ttl_sets_default_ttl_and_zero_capacity_still_evicts_input() {
        let ttl = Duration::from_secs(30);
        let map = BoundedMap::<&str, u32>::new(2);
        assert_eq!(map.ttl(), None);

        let mut zero = BoundedMap::with_ttl(0, ttl);
        assert_eq!(zero.capacity(), 0);
        assert_eq!(zero.ttl(), Some(ttl));
        assert!(zero.is_full());
        assert!(zero.is_empty());
        assert_eq!(zero.insert("a", 1), Inserted::Evicted("a", 1));
        assert_eq!(
            zero.insert_with_ttl("b", 2, Duration::from_secs(5)),
            Inserted::Evicted("b", 2)
        );
        assert_eq!(zero.remove_expired(), 0);
    }

    #[test]
    fn zero_ttl_expires_immediately_without_evicting_live_entries() {
        let mut map = BoundedMap::with_ttl(2, Duration::ZERO);
        assert_eq!(map.insert("a", 1), Inserted::Added);
        assert!(map.is_empty());
        assert_eq!(map.len(), 0);
        assert_eq!(map.get("a"), None);
        assert_eq!(map.remove_expired(), 0);

        let mut mixed = BoundedMap::new(2);
        assert_eq!(mixed.insert("a", 1), Inserted::Added);
        assert_eq!(mixed.insert("b", 2), Inserted::Added);
        assert_eq!(
            mixed.insert_with_ttl("c", 3, Duration::ZERO),
            Inserted::Added
        );
        assert_eq!(mixed.len(), 2);
        assert_eq!(mixed.peek("a"), Some(&1));
        assert_eq!(mixed.peek("b"), Some(&2));
        assert_eq!(mixed.peek("c"), None);

        assert_eq!(
            mixed.insert_with_ttl("a", 10, Duration::ZERO),
            Inserted::Replaced(1)
        );
        assert_eq!(mixed.len(), 1);
        assert_eq!(mixed.peek("a"), None);
        assert_eq!(mixed.peek("b"), Some(&2));
    }

    #[test]
    fn duration_max_ttl_does_not_overflow_or_panic() {
        let mut map = BoundedMap::with_ttl(2, Duration::MAX);
        assert_eq!(map.insert("a", 1), Inserted::Added);
        assert_eq!(map.insert_with_ttl("b", 2, Duration::MAX), Inserted::Added);
        assert_eq!(map.get("a"), Some(&1));
        assert_eq!(map.len(), 2);
        assert_eq!(map.remove_expired(), 0);
    }

    #[test]
    fn debug_includes_ttl_only_when_configured() {
        let ttl = Duration::from_secs(60);
        let mut map = BoundedMap::with_ttl(3, ttl);
        drop(map.insert("a", 1));
        assert_eq!(
            format!("{map:?}"),
            r#"BoundedMap { capacity: 3, ttl: 60s, entries: {"a": 1} }"#
        );
    }
}

#[cfg(all(feature = "tokio", not(target_os = "wasi")))]
mod expiry_paused_clock {
    use std::{collections::VecDeque, time::Duration};

    use adamantium::{BoundedMap, Inserted, random, time};
    use tokio::time::Instant;

    use super::{next, order, reverse_order};

    #[tokio::test(start_paused = true)]
    async fn expiry_on_read_peek_contains_iter_len_and_lazy_removal() {
        let ttl = Duration::from_secs(10);
        let mut map = BoundedMap::with_ttl(3, ttl);
        assert_eq!(map.insert("a", 1), Inserted::Added);
        assert_eq!(map.insert("b", 2), Inserted::Added);

        tokio::time::advance(Duration::from_secs(6)).await;
        // Reading refreshes recency, not TTL.
        assert_eq!(map.get("a"), Some(&1));
        assert_eq!(order(&map), ["a", "b"]);

        tokio::time::advance(Duration::from_secs(3) + Duration::from_nanos(999_999_999)).await;
        assert_eq!(map.len(), 2);
        assert!(!map.is_empty());
        assert_eq!(map.peek("a"), Some(&1));
        assert!(map.contains_key("b"));

        // At the exact TTL boundary, both entries expire.
        tokio::time::advance(Duration::from_nanos(1)).await;
        assert_eq!(map.len(), 0);
        assert!(map.is_empty());
        assert!(!map.is_full());
        assert_eq!(map.peek("a"), None);
        assert!(!map.contains_key("a"));
        assert_eq!(map.iter().len(), 0);
        assert_eq!(map.iter().next(), None);
        assert_eq!(map.iter().next_back(), None);
        assert_eq!(
            format!("{map:?}"),
            r#"BoundedMap { capacity: 3, ttl: 10s, entries: {} }"#
        );

        // Lazy removal on get and get_mut drops the expired entries immediately.
        assert_eq!(map.get("a"), None);
        assert_eq!(map.get_mut("b"), None);
        assert_eq!(map.remove_expired(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn expired_entry_is_removed_on_insert_before_evicting_a_live_entry() {
        let mut map = BoundedMap::with_ttl(2, Duration::from_secs(60));
        assert_eq!(map.insert("long", 1), Inserted::Added);
        assert_eq!(
            map.insert_with_ttl("short", 2, Duration::from_secs(10)),
            Inserted::Added
        );
        // "short" is MRU and "long" is LRU, so capacity eviction would pick "long".
        assert_eq!(order(&map), ["short", "long"]);
        assert!(map.is_full());

        tokio::time::advance(Duration::from_secs(10)).await;
        assert!(!map.is_full());
        assert_eq!(map.len(), 1);

        // Inserting into the full backing storage removes expired "short" instead
        // of evicting live "long".
        assert_eq!(map.insert("new", 3), Inserted::Added);
        assert_eq!(order(&map), ["new", "long"]);
        assert_eq!(map.get("long"), Some(&1));
        assert_eq!(map.get("new"), Some(&3));
        assert_eq!(map.get("short"), None);
        assert_eq!(map.remove_expired(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn mixed_permanent_and_expiring_entries_on_map_without_default_ttl() {
        let mut map = BoundedMap::new(2);
        assert_eq!(map.insert("forever", 1), Inserted::Added);
        assert_eq!(
            map.insert_with_ttl("temp", 2, Duration::from_secs(5)),
            Inserted::Added
        );

        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(map.len(), 1);
        assert!(!map.is_empty());
        assert!(!map.is_full());
        assert_eq!(map.insert("second", 3), Inserted::Added);
        assert_eq!(order(&map), ["second", "forever"]);
        assert_eq!(map.insert("third", 4), Inserted::Evicted("forever", 1));

        // Replacing an expiring entry with plain insert clears its TTL.
        assert_eq!(
            map.insert_with_ttl("third", 40, Duration::from_secs(5)),
            Inserted::Replaced(4)
        );
        assert_eq!(map.insert("third", 400), Inserted::Replaced(40));
        tokio::time::advance(Duration::from_secs(60)).await;
        assert_eq!(map.get("third"), Some(&400));
    }

    #[tokio::test(start_paused = true)]
    async fn replacing_live_entry_resets_ttl_and_replacing_expired_entry_adds_fresh() {
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

        let mut map = BoundedMap::with_ttl(2, Duration::from_secs(10));
        assert_eq!(map.insert(Key(1, "first"), 'a'), Inserted::Added);

        tokio::time::advance(Duration::from_secs(7)).await;
        // Replacing a live entry keeps the stored key and starts a new TTL.
        assert_eq!(map.insert(Key(1, "second"), 'b'), Inserted::Replaced('a'));
        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(map.get(&Key(1, "")), Some(&'b'));
        assert_eq!(map.iter().map(|(k, _)| k.1).collect::<Vec<_>>(), ["first"]);

        // After the refreshed TTL elapses, inserting treats the key as absent.
        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(map.insert(Key(1, "third"), 'c'), Inserted::Added);
        assert_eq!(map.iter().map(|(k, _)| k.1).collect::<Vec<_>>(), ["third"]);
    }

    #[tokio::test(start_paused = true)]
    async fn remove_expired_sweeps_all_expired_entries_and_preserves_live_order() {
        let mut map = BoundedMap::new(5);
        drop(map.insert_with_ttl("a", 1, Duration::from_secs(5)));
        drop(map.insert_with_ttl("b", 2, Duration::from_secs(20)));
        drop(map.insert_with_ttl("c", 3, Duration::from_secs(10)));
        drop(map.insert("d", 4));
        drop(map.insert_with_ttl("e", 5, Duration::from_secs(10)));
        assert_eq!(order(&map), ["e", "d", "c", "b", "a"]);

        tokio::time::advance(Duration::from_secs(10)).await;
        assert_eq!(map.len(), 2);
        assert_eq!(order(&map), ["d", "b"]);
        assert_eq!(reverse_order(&map), ["b", "d"]);

        assert_eq!(map.remove_expired(), 3);
        assert_eq!(map.remove_expired(), 0);
        assert_eq!(map.len(), 2);
        assert_eq!(order(&map), ["d", "b"]);
        assert_eq!(reverse_order(&map), ["b", "d"]);
    }

    #[tokio::test(start_paused = true)]
    async fn pop_lru_and_remove_ignore_expired_entries() {
        let mut map = BoundedMap::new(3);
        drop(map.insert_with_ttl("a", 1, Duration::from_secs(5)));
        drop(map.insert_with_ttl("b", 2, Duration::from_secs(20)));
        drop(map.insert_with_ttl("c", 3, Duration::from_secs(5)));

        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(map.remove("c"), None);
        assert_eq!(map.pop_lru(), Some(("b", 2)));
        assert_eq!(map.pop_lru(), None);
        assert_eq!(map.remove_expired(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn iteration_skips_expired_entries_from_both_ends_with_exact_length() {
        let mut map = BoundedMap::new(5);
        drop(map.insert_with_ttl(1, 10, Duration::from_secs(5)));
        drop(map.insert_with_ttl(2, 20, Duration::from_secs(20)));
        drop(map.insert_with_ttl(3, 30, Duration::from_secs(5)));
        drop(map.insert_with_ttl(4, 40, Duration::from_secs(20)));
        drop(map.insert_with_ttl(5, 50, Duration::from_secs(5)));

        tokio::time::advance(Duration::from_secs(5)).await;
        let mut iter = map.iter();
        assert_eq!(iter.len(), 2);
        assert_eq!(format!("{iter:?}"), "[(4, 40), (2, 20)]");
        assert_eq!(iter.next(), Some((&4, &40)));
        assert_eq!(iter.len(), 1);
        assert_eq!(iter.next_back(), Some((&2, &20)));
        assert_eq!(iter.len(), 0);
        assert_eq!(iter.next(), None);
        assert_eq!(iter.next_back(), None);
    }

    struct TtlModel {
        capacity: usize,
        default_ttl: Option<Duration>,
        entries: VecDeque<(u8, u32, Option<Instant>)>,
    }

    impl TtlModel {
        fn sweep(&mut self, now: Instant) -> usize {
            let before = self.entries.len();
            self.entries
                .retain(|(_, _, exp)| exp.is_none_or(|at| at > now));
            before - self.entries.len()
        }

        fn position(&self, key: u8) -> Option<usize> {
            self.entries.iter().position(|(k, _, _)| *k == key)
        }

        fn touch(&mut self, key: u8, now: Instant) -> Option<&mut u32> {
            self.sweep(now);
            let position = self.position(key)?;
            let entry = self.entries.remove(position)?;
            self.entries.push_front(entry);
            self.entries.front_mut().map(|(_, value, _)| value)
        }

        fn insert(
            &mut self,
            key: u8,
            value: u32,
            ttl: Option<Duration>,
            now: Instant,
        ) -> Inserted<u8, u32> {
            self.sweep(now);
            if self.capacity == 0 {
                return Inserted::Evicted(key, value);
            }
            if let Some(position) = self.position(key) {
                let Some((_, old, _)) = self.entries.remove(position) else {
                    return Inserted::Added;
                };
                if !ttl.is_some_and(|ttl| ttl.is_zero()) {
                    let expires_at = ttl.and_then(|ttl| now.checked_add(ttl));
                    self.entries.push_front((key, value, expires_at));
                }
                return Inserted::Replaced(old);
            }
            if ttl.is_some_and(|ttl| ttl.is_zero()) {
                return Inserted::Added;
            }
            let evicted = if self.entries.len() == self.capacity {
                self.entries.pop_back().map(|(k, v, _)| (k, v))
            } else {
                None
            };
            let expires_at = ttl.and_then(|ttl| now.checked_add(ttl));
            self.entries.push_front((key, value, expires_at));
            match evicted {
                Some((k, v)) => Inserted::Evicted(k, v),
                None => Inserted::Added,
            }
        }
    }

    async fn run_ttl_model(seed: u64) {
        let mut state = seed | 1;
        let capacity = usize::try_from(next(&mut state) % 6).unwrap_or(0);
        let default_ttl = match next(&mut state) % 3 {
            0 => None,
            1 => Some(Duration::from_millis((next(&mut state) % 8) + 1)),
            _ => Some(Duration::ZERO),
        };
        let mut map = match default_ttl {
            Some(ttl) => BoundedMap::with_ttl(capacity, ttl),
            None => BoundedMap::new(capacity),
        };
        let mut model = TtlModel {
            capacity,
            default_ttl,
            entries: VecDeque::new(),
        };

        for step in 0..250 {
            let key = u8::try_from(next(&mut state) % 8).unwrap_or(0);
            let value = u32::try_from(next(&mut state) % 1000).unwrap_or(0);
            let context =
                format!("seed {seed}, capacity {capacity}, ttl {default_ttl:?}, step {step}");
            let now = time::now();
            match next(&mut state) % 24 {
                0..=3 => {
                    let got = map.insert(key, value);
                    let want = model.insert(key, value, model.default_ttl, now);
                    assert_eq!(got, want, "{context}");
                }
                4..=7 => {
                    let ttl = Duration::from_millis(next(&mut state) % 10);
                    let got = map.insert_with_ttl(key, value, ttl);
                    let want = model.insert(key, value, Some(ttl), now);
                    assert_eq!(got, want, "{context}");
                }
                8..=10 => {
                    assert_eq!(
                        map.get(&key).copied(),
                        model.touch(key, now).copied(),
                        "{context}"
                    );
                }
                11..=12 => {
                    let got = map.get_mut(&key).map(|v| {
                        *v += 1;
                        *v
                    });
                    let want = model.touch(key, now).map(|v| {
                        *v += 1;
                        *v
                    });
                    assert_eq!(got, want, "{context}");
                }
                13 => {
                    model.sweep(now);
                    let want = model.position(key).and_then(|p| model.entries.get(p));
                    assert_eq!(map.peek(&key), want.map(|(_, v, _)| v), "{context}");
                    assert_eq!(map.contains_key(&key), want.is_some(), "{context}");
                }
                14..=15 => {
                    model.sweep(now);
                    let want = model
                        .position(key)
                        .and_then(|p| model.entries.remove(p))
                        .map(|(_, v, _)| v);
                    assert_eq!(map.remove(&key), want, "{context}");
                }
                16..=17 => {
                    model.sweep(now);
                    let want = model.entries.pop_back().map(|(k, v, _)| (k, v));
                    assert_eq!(map.pop_lru(), want, "{context}");
                }
                18 => {
                    let removed = map.remove_expired();
                    assert!(removed <= capacity, "{context}");
                    assert_eq!(map.remove_expired(), 0, "{context}");
                    model.sweep(now);
                }
                19..=22 => {
                    let advance = Duration::from_millis(next(&mut state) % 6);
                    tokio::time::advance(advance).await;
                }
                _ => {
                    map.clear();
                    model.entries.clear();
                }
            }

            let now = time::now();
            model.sweep(now);
            let expected: Vec<_> = model.entries.iter().map(|(k, v, _)| (*k, *v)).collect();
            let forward: Vec<_> = map.iter().map(|(k, v)| (*k, *v)).collect();
            assert_eq!(forward, expected, "{context}");
            let mut backward: Vec<_> = map.iter().rev().map(|(k, v)| (*k, *v)).collect();
            backward.reverse();
            assert_eq!(backward, expected, "{context}");
            assert_eq!(map.len(), expected.len(), "{context}");
            assert_eq!(map.is_empty(), expected.is_empty(), "{context}");
            assert_eq!(map.is_full(), expected.len() == capacity, "{context}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn ttl_matches_a_naive_model_over_random_operations() {
        for seed in 0..100 {
            run_ttl_model(seed).await;
        }
        for _ in 0..20 {
            run_ttl_model(random::u64()).await;
        }
    }
}

#[cfg(feature = "deterministic")]
mod expiry_deterministic {
    use std::time::Duration;

    use adamantium::{
        Actor, BoundedMap, Cancellation, Inserted, Tasks,
        testing::{self, Config},
        time::{self, Interval},
    };

    const HOUR: Duration = Duration::from_secs(60 * 60);

    #[test]
    fn deterministic_virtual_time_drives_expiry_eviction_and_periodic_sweep() {
        let config = Config {
            seed: 258,
            iterations: 50,
            max_steps: 10_000,
            ..Config::default()
        };
        let factory = || async {
            let cache = Actor::new(BoundedMap::with_ttl(2, HOUR));

            // 1. Expiry on read under virtual time.
            assert_eq!(cache.with(|c| c.insert("session-a", 1)), Inserted::Added);
            assert_eq!(
                cache.with(|c| c.insert_with_ttl("session-b", 2, HOUR * 3)),
                Inserted::Added
            );
            // Make session-a MRU while session-b is LRU (with a longer TTL).
            assert_eq!(cache.with(|c| c.get("session-a").copied()), Some(1));

            time::sleep(HOUR).await;
            assert_eq!(cache.read(BoundedMap::len), 1);
            assert_eq!(cache.read(|c| c.peek("session-a").copied()), None);
            assert!(!cache.read(|c| c.contains_key("session-a")));
            assert_eq!(cache.read(|c| c.peek("session-b").copied()), Some(2));

            // 2. Insert removes expired session-a instead of evicting live session-b.
            assert_eq!(cache.with(|c| c.insert("session-c", 3)), Inserted::Added);
            assert_eq!(cache.read(BoundedMap::len), 2);
            assert_eq!(cache.with(|c| c.get("session-b").copied()), Some(2));
            assert_eq!(cache.with(|c| c.get("session-c").copied()), Some(3));

            // 3. Periodic sweep removes expired entries on virtual ticks.
            let swept = Actor::new(0usize);
            let shutdown = Cancellation::new();
            let sweep_cache = cache.clone();
            let sweep_count = swept.clone();
            let sweep_cancel = shutdown.clone();
            let result = Tasks::<()>::run(async move |tasks| {
                let mut ticks = Interval::at(time::now() + HOUR, HOUR);
                tasks.spawn(async move {
                    while ticks.tick_until(&sweep_cancel).await.is_some() {
                        let removed = sweep_cache.with(BoundedMap::remove_expired);
                        sweep_count.with(|n| *n += removed);
                    }
                    Ok(())
                });

                // session-c expires at +1h; session-b expires at +2h.
                time::sleep(HOUR * 2 + Duration::from_secs(1)).await;
                shutdown.cancel();
                Ok(())
            })
            .await;
            assert_eq!(result, Ok(()));
            assert_eq!(swept.snapshot(), 2);
            assert_eq!(cache.read(BoundedMap::len), 0);
            assert_eq!(cache.with(BoundedMap::remove_expired), 0);
        };

        let record = testing::check(config, factory).unwrap();
        assert_eq!(testing::check(config, factory).unwrap(), record);
        assert_eq!(testing::replay(&record, factory).unwrap(), record);
    }
}
