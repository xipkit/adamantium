use std::{
    borrow::Borrow,
    collections::HashMap,
    hash::{Hash, Hasher},
    ops::Bound,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use adamantium::{Array, Bytes, Deque, Map, Set, Str};

#[derive(Debug)]
struct Counted {
    value: usize,
    clones: Arc<AtomicUsize>,
}

impl Counted {
    fn new(value: usize, clones: &Arc<AtomicUsize>) -> Self {
        Self {
            value,
            clones: Arc::clone(clones),
        }
    }
}

impl Clone for Counted {
    fn clone(&self) -> Self {
        self.clones.fetch_add(1, Ordering::Relaxed);
        Self::new(self.value, &self.clones)
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
        self.value.hash(state);
    }
}

impl Borrow<usize> for Counted {
    fn borrow(&self) -> &usize {
        &self.value
    }
}

#[test]
fn array_shared_clear_does_not_clone_discarded_elements() {
    let clones = Arc::new(AtomicUsize::new(0));
    let original: Array<_> = (0..1_000).map(|i| Counted::new(i, &clones)).collect();
    let mut cleared = original.clone();
    cleared.clear();

    assert_eq!(clones.load(Ordering::Relaxed), 0);
    assert!(cleared.is_empty());
    assert!(!cleared.is_shared());
    assert!(!original.is_shared());
    assert_eq!(cleared.make_mut().capacity(), 0);
    assert!(original.iter().map(|v| v.value).eq(0..1_000));
    cleared.push(Counted::new(7, &clones));
    assert_eq!(original.len(), 1_000);
}

#[test]
fn array_unique_clear_retains_allocation_without_clones() {
    let clones = Arc::new(AtomicUsize::new(0));
    let mut array = Array::with_capacity(2_048);
    array.extend((0..1_000).map(|i| Counted::new(i, &clones)));
    let capacity = array.make_mut().capacity();
    let pointer = array.as_ptr();
    array.clear();

    assert!(array.is_empty());
    assert_eq!(array.make_mut().capacity(), capacity);
    assert_eq!(array.as_ptr(), pointer);
    assert_eq!(clones.load(Ordering::Relaxed), 0);
    array.push(Counted::new(1, &clones));
    assert_eq!(array.as_ptr(), pointer);
}

#[test]
fn invalid_array_updates_preserve_storage() {
    for len in [0, 1_000] {
        for shared in [false, true] {
            let clones = Arc::new(AtomicUsize::new(0));
            let mut array: Array<_> = (0..len).map(|i| Counted::new(i, &clones)).collect();
            let original = shared.then(|| array.clone());
            let pointer = array.as_ptr();
            for index in [len, usize::MAX] {
                let value = Counted::new(42, &clones);
                assert_eq!(array.set(index, value).unwrap_err().value, 42);
                assert!(array.get_mut(index).is_none());
                assert_eq!(clones.load(Ordering::Relaxed), 0);
                assert_eq!(array.is_shared(), shared);
                assert_eq!(array.as_ptr(), pointer);
            }
            assert!(array.iter().map(|v| v.value).eq(0..len));
            assert_eq!(original.as_ref().map(|a| a.len()), shared.then_some(len));
        }
    }
}

#[test]
fn valid_array_updates_still_copy_on_write() {
    let clones = Arc::new(AtomicUsize::new(0));
    let original: Array<_> = (0..3).map(|i| Counted::new(i, &clones)).collect();
    let mut updated = original.clone();
    updated.set(0, Counted::new(10, &clones)).unwrap();
    updated.get_mut(2).unwrap().value = 20;

    assert_eq!(clones.load(Ordering::Relaxed), 3);
    assert!(original.iter().map(|v| v.value).eq([0, 1, 2]));
    assert!(updated.iter().map(|v| v.value).eq([10, 1, 20]));
}

#[test]
fn map_shared_clear_does_not_clone_keys_or_values() {
    let keys = Arc::new(AtomicUsize::new(0));
    let values = Arc::new(AtomicUsize::new(0));
    let original: Map<_, _> = (0..1_000)
        .map(|i| (Counted::new(i, &keys), Counted::new(i, &values)))
        .collect();
    let mut cleared = original.clone();
    cleared.clear();

    assert_eq!(keys.load(Ordering::Relaxed), 0);
    assert_eq!(values.load(Ordering::Relaxed), 0);
    assert!(cleared.is_empty());
    assert_eq!(cleared.as_index_map().capacity(), 0);
    assert!(!original.is_shared());
    assert!(!cleared.is_shared());
    assert!(
        original
            .iter()
            .map(|(k, v)| (k.value, v.value))
            .eq((0..1_000).map(|i| (i, i)))
    );
    cleared.insert(Counted::new(7, &keys), Counted::new(8, &values));
    assert_eq!(original.get(&7).unwrap().value, 7);
}

#[test]
fn map_unique_clear_retains_capacity_without_clones() {
    let keys = Arc::new(AtomicUsize::new(0));
    let values = Arc::new(AtomicUsize::new(0));
    let mut map = Map::with_capacity(2_048);
    map.extend((0..1_000).map(|i| (Counted::new(i, &keys), Counted::new(i, &values))));
    let capacity = map.as_index_map().capacity();
    map.clear();

    assert!(map.is_empty());
    assert_eq!(map.as_index_map().capacity(), capacity);
    assert_eq!(keys.load(Ordering::Relaxed), 0);
    assert_eq!(values.load(Ordering::Relaxed), 0);
    map.insert(Counted::new(7, &keys), Counted::new(8, &values));
    assert_eq!(map.as_index_map().capacity(), capacity);
}

#[test]
fn missing_map_updates_preserve_storage() {
    for len in [0, 1_000] {
        for shared in [false, true] {
            let keys = Arc::new(AtomicUsize::new(0));
            let values = Arc::new(AtomicUsize::new(0));
            let mut map: Map<_, _> = (0..len)
                .map(|i| (Counted::new(i, &keys), Counted::new(i, &values)))
                .collect();
            let original = shared.then(|| map.clone());
            let storage = std::ptr::from_ref(map.as_index_map());
            assert!(map.remove(&len).is_none());
            assert!(map.get_mut(&len).is_none());
            assert_eq!(keys.load(Ordering::Relaxed), 0);
            assert_eq!(values.load(Ordering::Relaxed), 0);
            assert_eq!(map.is_shared(), shared);
            assert_eq!(std::ptr::from_ref(map.as_index_map()), storage);
            assert!(map.keys().map(|k| k.value).eq(0..len));
            assert_eq!(original.as_ref().map(Map::len), shared.then_some(len));
        }
    }
}

#[test]
fn present_map_updates_preserve_order_and_copy_on_write() {
    let original = Map::from([(Str::from("c"), 3), ("a".into(), 1), ("b".into(), 2)]);
    let mut updated = original.clone();
    *updated.get_mut("a").unwrap() = 10;
    assert_eq!(updated.remove("c"), Some(3));
    assert_eq!(
        updated.keys().map(Str::as_str).collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert_eq!(updated.get("a"), Some(&10));
    assert_eq!(original.get("a"), Some(&1));
    assert_eq!(
        original.keys().map(Str::as_str).collect::<Vec<_>>(),
        ["c", "a", "b"]
    );

    let mut removed = original.clone();
    assert_eq!(removed.remove("a"), Some(1));
    assert_eq!(
        removed.keys().map(Str::as_str).collect::<Vec<_>>(),
        ["c", "b"]
    );
}

#[test]
fn str_shared_clear_releases_capacity_and_preserves_original() {
    let original = Str::from("hello".repeat(1_000));
    let pointer = original.as_ptr();
    let mut cleared = original.clone();
    cleared.clear();

    assert!(cleared.is_empty());
    assert_eq!(cleared.make_mut().capacity(), 0);
    assert_eq!(original.as_ptr(), pointer);
    assert_eq!(original, "hello".repeat(1_000));
    cleared.push_str("new");
    assert_eq!(original.len(), 5_000);
}

#[test]
fn str_unique_clear_retains_allocation() {
    let mut buffer = String::with_capacity(2_048);
    buffer.push_str("hello");
    let mut value = Str::from(buffer);
    let capacity = value.make_mut().capacity();
    let pointer = value.as_ptr();
    value.clear();

    assert!(value.is_empty());
    assert_eq!(value.make_mut().capacity(), capacity);
    assert_eq!(value.as_ptr(), pointer);
    value.push_str("new");
    assert_eq!(value.as_ptr(), pointer);
}

#[test]
fn bytes_shared_clear_releases_capacity_and_preserves_original() {
    let original = Bytes::from(vec![42; 1_000]);
    let pointer = original.as_ptr();
    let mut cleared = original.clone();
    cleared.clear();

    assert!(cleared.is_empty());
    assert_eq!(cleared.make_mut().capacity(), 0);
    assert_eq!(original.as_ptr(), pointer);
    assert_eq!(original.as_slice(), vec![42; 1_000]);
    cleared.push(7);
    assert_eq!(original.len(), 1_000);
}

#[test]
fn bytes_unique_clear_retains_allocation() {
    let mut buffer = Vec::with_capacity(2_048);
    buffer.extend_from_slice(b"hello");
    let mut value = Bytes::from(buffer);
    let capacity = value.make_mut().capacity();
    let pointer = value.as_ptr();
    value.clear();

    assert!(value.is_empty());
    assert_eq!(value.make_mut().capacity(), capacity);
    assert_eq!(value.as_ptr(), pointer);
    value.push(7);
    assert_eq!(value.as_ptr(), pointer);
}

#[test]
fn bytes_nested_windows_share_storage() {
    let original = Bytes::from("prefixframefooter");
    let window = original.slice(6..11).unwrap();
    let nested = window.slice(1..=3).unwrap();

    assert!(original.is_shared());
    assert!(window.is_shared());
    assert!(nested.is_shared());
    assert_eq!(window.as_ptr(), original.as_ptr().wrapping_add(6));
    assert_eq!(nested.as_ptr(), window.as_ptr().wrapping_add(1));
    assert_eq!(window.as_slice(), b"frame");
    assert_eq!(nested.as_slice(), b"ram");
    assert_eq!(window.slice(..2).unwrap().as_slice(), b"fr");
    assert_eq!(window.slice(2..).unwrap().as_slice(), b"ame");

    assert!(window.slice(4..6).is_none());
    assert!(window.slice(6..).is_none());
    assert!(window.slice(..=5).is_none());
    let (start, end) = (3, 2);
    assert!(window.slice(start..end).is_none());
    assert!(window.slice(..=usize::MAX).is_none());
    assert!(
        window
            .slice((Bound::Excluded(usize::MAX), Bound::Unbounded))
            .is_none()
    );
}

#[test]
fn bytes_empty_windows_are_valid_at_any_in_bounds_offset() {
    let original = Bytes::from("frame");
    for offset in [0, 2, original.len()] {
        let mut empty = original.slice(offset..offset).unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty.as_ptr(), original.as_ptr().wrapping_add(offset));
        assert!(empty.slice(..).unwrap().is_empty());
        assert!(empty.slice(..1).is_none());
        assert!(empty.clone().into_vec().is_empty());
        empty.push(b'x');
        assert_eq!(empty.as_slice(), b"x");
        assert_eq!(original.as_slice(), b"frame");
    }
    assert!(Bytes::new().slice(..).unwrap().is_empty());
}

#[test]
fn bytes_windows_are_values_of_the_selected_bytes() {
    let original = Bytes::from("zabbac");
    let first = original.slice(1..3).unwrap();
    let second = original.slice(3..5).unwrap();

    assert_eq!(first, Bytes::from("ab"));
    assert!(first < second);
    assert!(first.iter().copied().eq(*b"ab"));
    assert_eq!(first.as_ref(), b"ab");
    assert_eq!(format!("{first:?}"), "b\"ab\"");
    let values = HashMap::from([(first.clone(), 1)]);
    assert_eq!(values.get(b"ab".as_slice()), Some(&1));
    assert_eq!(first.into_vec(), b"ab");
    drop(values);
    drop(original);
    assert_eq!(second.into_vec(), b"ba");
}

#[test]
fn bytes_window_make_mut_isolates_overlapping_windows_and_parent() {
    let mut parent = Bytes::from("prefixframefooter");
    let mut frame = parent.slice(6..11).unwrap();
    let mut overlap = parent.slice(8..13).unwrap();

    let buffer: &mut Vec<u8> = frame.make_mut();
    assert_eq!(buffer.as_slice(), b"frame");
    buffer.reverse();
    buffer.push(b'!');
    assert_eq!(frame.as_slice(), b"emarf!");
    assert!(!frame.is_shared());
    assert_eq!(parent.as_slice(), b"prefixframefooter");
    assert_eq!(overlap.as_slice(), b"amefo");

    parent.make_mut().truncate(2);
    assert_eq!(parent.as_slice(), b"pr");
    assert_eq!(overlap.as_slice(), b"amefo");
    assert!(!overlap.is_shared());
    overlap.extend_from_slice(b"!");
    assert_eq!(overlap.as_slice(), b"amefo!");
}

#[test]
fn bytes_clear_detaches_shared_windows_and_resets_unique_windows() {
    let parent = Bytes::from("prefixframefooter");
    let pointer = parent.as_ptr();
    let mut window = parent.slice(6..11).unwrap();
    let mut shared = window.clone();
    shared.clear();
    assert!(shared.is_empty());
    assert!(!shared.is_shared());
    assert_eq!(window.as_slice(), b"frame");

    drop(parent);
    window.clear();
    assert!(window.is_empty());
    assert_eq!(window.as_ptr(), pointer);
    window.push(b'!');
    assert_eq!(window.as_slice(), b"!");
}

#[test]
fn bytes_full_window_reuses_unique_buffer() {
    let parent = Bytes::from("frame");
    let pointer = parent.as_ptr();
    let mut full = parent.slice(..).unwrap();
    drop(parent);

    full.make_mut().reverse();
    let owned = full.into_vec();
    assert_eq!(owned.as_ptr(), pointer);
    assert_eq!(owned, b"emarf");
}

#[test]
fn deque_shared_clear_does_not_clone_discarded_elements() {
    let clones = Arc::new(AtomicUsize::new(0));
    let original: Deque<_> = (0..1_000).map(|i| Counted::new(i, &clones)).collect();
    let mut cleared = original.clone();
    cleared.clear();

    assert_eq!(clones.load(Ordering::Relaxed), 0);
    assert!(cleared.is_empty());
    assert_eq!(cleared.capacity(), 0);
    assert!(!cleared.is_shared());
    assert!(!original.is_shared());
    assert!(original.iter().map(|v| v.value).eq(0..1_000));
    cleared.push_back(Counted::new(7, &clones));
    assert_eq!(original.len(), 1_000);
    assert_eq!(cleared.front().unwrap().value, 7);
}

#[test]
fn deque_unique_clear_retains_capacity_without_clones() {
    let clones = Arc::new(AtomicUsize::new(0));
    let mut deque = Deque::with_capacity(2_048);
    deque.extend((0..1_000).map(|i| Counted::new(i, &clones)));
    let capacity = deque.capacity();
    deque.clear();

    assert!(deque.is_empty());
    assert_eq!(deque.capacity(), capacity);
    assert_eq!(clones.load(Ordering::Relaxed), 0);
    deque.push_back(Counted::new(7, &clones));
    assert_eq!(deque.capacity(), capacity);
}

#[test]
fn set_shared_clear_does_not_clone_discarded_values() {
    let clones = Arc::new(AtomicUsize::new(0));
    let original: Set<_> = (0..1_000).map(|i| Counted::new(i, &clones)).collect();
    let mut cleared = original.clone();
    cleared.clear();

    assert_eq!(clones.load(Ordering::Relaxed), 0);
    assert!(cleared.is_empty());
    assert_eq!(cleared.as_index_set().capacity(), 0);
    assert!(!cleared.is_shared());
    assert!(!original.is_shared());
    assert!(original.iter().map(|v| v.value).eq(0..1_000));
    assert!(cleared.insert(Counted::new(7, &clones)));
    assert_eq!(original.len(), 1_000);
    assert_eq!(cleared.get(&7).unwrap().value, 7);
}

#[test]
fn set_unique_clear_retains_capacity_without_clones() {
    let clones = Arc::new(AtomicUsize::new(0));
    let mut set = Set::with_capacity(2_048);
    set.extend((0..1_000).map(|i| Counted::new(i, &clones)));
    let capacity = set.as_index_set().capacity();
    set.clear();

    assert!(set.is_empty());
    assert_eq!(set.as_index_set().capacity(), capacity);
    assert_eq!(clones.load(Ordering::Relaxed), 0);
    assert!(set.insert(Counted::new(7, &clones)));
    assert_eq!(set.as_index_set().capacity(), capacity);
}

#[test]
fn clearing_empty_collections_preserves_unique_capacity_and_shared_originals() {
    let mut array = Array::<u8>::with_capacity(128);
    let mut map = Map::<u8, u8>::with_capacity(128);
    let mut string = Str::from(String::with_capacity(128));
    let mut bytes = Bytes::from(Vec::with_capacity(128));
    let mut deque = Deque::<u8>::with_capacity(128);
    let mut set = Set::<u8>::with_capacity(128);
    let map_capacity = map.as_index_map().capacity();
    let set_capacity = set.as_index_set().capacity();
    array.clear();
    map.clear();
    string.clear();
    bytes.clear();
    deque.clear();
    set.clear();
    assert_eq!(array.make_mut().capacity(), 128);
    assert_eq!(map.as_index_map().capacity(), map_capacity);
    assert_eq!(string.make_mut().capacity(), 128);
    assert_eq!(bytes.make_mut().capacity(), 128);
    assert_eq!(deque.capacity(), 128);
    assert_eq!(set.as_index_set().capacity(), set_capacity);

    let mut array_clone = array.clone();
    let mut map_clone = map.clone();
    let mut string_clone = string.clone();
    let mut bytes_clone = bytes.clone();
    let mut deque_clone = deque.clone();
    let mut set_clone = set.clone();
    array_clone.clear();
    map_clone.clear();
    string_clone.clear();
    bytes_clone.clear();
    deque_clone.clear();
    set_clone.clear();
    assert_eq!(array.make_mut().capacity(), 128);
    assert_eq!(map.as_index_map().capacity(), map_capacity);
    assert_eq!(string.make_mut().capacity(), 128);
    assert_eq!(bytes.make_mut().capacity(), 128);
    assert_eq!(deque.capacity(), 128);
    assert_eq!(set.as_index_set().capacity(), set_capacity);
    assert_eq!(array_clone.make_mut().capacity(), 0);
    assert_eq!(map_clone.as_index_map().capacity(), 0);
    assert_eq!(string_clone.make_mut().capacity(), 0);
    assert_eq!(bytes_clone.make_mut().capacity(), 0);
    assert_eq!(deque_clone.capacity(), 0);
    assert_eq!(set_clone.as_index_set().capacity(), 0);
}
