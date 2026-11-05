use std::{
    borrow::Borrow,
    collections::{HashMap, VecDeque, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use adamantium::Deque;

fn wrapped() -> Deque<i32> {
    let mut deque = Deque::with_capacity(8);
    let capacity = deque.capacity();
    deque.extend(0..8);
    for value in 0..6 {
        assert_eq!(deque.pop_front(), Some(value));
    }
    deque.extend(8..12);
    assert_eq!(deque.capacity(), capacity);
    assert!(!deque.as_slices().0.is_empty());
    assert!(!deque.as_slices().1.is_empty());
    deque
}

#[test]
fn queue_and_sliding_window() {
    let mut queue = Deque::new();
    queue.push_back("first");
    queue.push_back("second");
    assert_eq!(queue.pop_front(), Some("first"));
    queue.push_front("urgent");
    assert_eq!(queue.front(), Some(&"urgent"));
    assert_eq!(queue.back(), Some(&"second"));
    assert_eq!(queue.pop_back(), Some("second"));
    assert_eq!(queue.pop_front(), Some("urgent"));
    assert!(queue.is_empty());

    let mut window = wrapped();
    let snapshot = window.clone();
    for value in 12..1_000 {
        window.push_back(value);
        assert_eq!(window.pop_front(), Some(value - 6));
        assert_eq!(window.len(), 6);
        assert_eq!(window.iter().sum::<i32>(), (value - 5..=value).sum::<i32>());
    }
    assert_eq!(Vec::from(window), [994, 995, 996, 997, 998, 999]);
    assert_eq!(Vec::from(snapshot), [6, 7, 8, 9, 10, 11]);
}

#[test]
fn empty_and_invalid_operations() {
    let mut deque = Deque::<String>::default();
    assert!(deque.is_empty());
    assert_eq!(deque.len(), 0);
    assert_eq!(deque.front(), None);
    assert_eq!(deque.back(), None);
    assert_eq!(deque.front_mut(), None);
    assert_eq!(deque.back_mut(), None);
    assert_eq!(deque.pop_front(), None);
    assert_eq!(deque.pop_back(), None);
    assert_eq!(deque.remove(0), None);
    assert_eq!(deque.get_mut(0), None);
    assert_eq!(deque.set(0, "set".into()), Err("set".into()));
    assert_eq!(deque.insert(1, "insert".into()), Err("insert".into()));
    deque.clear();
    deque.truncate(usize::MAX);
    assert_eq!(deque.insert(0, "valid".into()), Ok(()));

    let original = deque.clone();
    for index in [deque.len(), usize::MAX] {
        assert_eq!(deque.get(index), None);
        assert_eq!(deque.get_mut(index), None);
        assert_eq!(deque.remove(index), None);
        let value = String::from("returned");
        let allocation = value.as_ptr();
        let returned = deque.set(index, value).unwrap_err();
        assert_eq!(returned.as_ptr(), allocation);
    }
    assert_eq!(deque.insert(usize::MAX, "bad".into()), Err("bad".into()));
    assert!(deque.is_shared());
    assert_eq!(deque, original);
}

#[test]
fn positional_edits_across_the_wrap() {
    let mut deque = wrapped();
    assert_eq!(deque.get(2), Some(&8));
    assert_eq!(deque.insert(0, 5), Ok(()));
    assert_eq!(deque.insert(deque.len(), 12), Ok(()));
    assert_eq!(deque.remove(3), Some(8));
    assert_eq!(deque.set(3, 80), Ok(()));
    *deque.get_mut(4).unwrap() = 100;
    assert_eq!(Vec::from(deque), [5, 6, 7, 80, 100, 11, 12]);
}

#[test]
fn every_mutation_preserves_shared_storage() {
    type Edit = fn(&mut Deque<i32>);
    let edits: &[Edit] = &[
        |d| d.push_front(5),
        |d| d.push_back(12),
        |d| assert_eq!(d.pop_front(), Some(6)),
        |d| assert_eq!(d.pop_back(), Some(11)),
        |d| *d.front_mut().unwrap() = 60,
        |d| *d.back_mut().unwrap() = 110,
        |d| assert_eq!(d.insert(2, 80), Ok(())),
        |d| assert_eq!(d.remove(2), Some(8)),
        |d| assert_eq!(d.set(2, 80), Ok(())),
        |d| *d.get_mut(2).unwrap() = 80,
        |d| d.retain(|v| v % 2 == 0),
        |d| d.truncate(2),
        |d| d.clear(),
        |d| d.extend([12, 13]),
        |d| d.make_mut().rotate_left(2),
    ];
    for edit in edits {
        let original = wrapped();
        let mut copy = original.clone();
        assert!(original.is_shared());
        assert!(std::ptr::eq(original.as_vec_deque(), copy.as_vec_deque()));
        edit(&mut copy);
        assert_eq!(
            original.iter().copied().collect::<Vec<_>>(),
            [6, 7, 8, 9, 10, 11]
        );
        assert_ne!(copy, original);
        assert!(!original.is_shared());
        assert!(!copy.is_shared());
    }
}

#[test]
fn filtering_truncating_and_clearing_keep_logical_order() {
    let mut deque = wrapped();
    deque.retain(|value| value % 2 == 0);
    assert_eq!(deque.iter().copied().collect::<Vec<_>>(), [6, 8, 10]);
    deque.truncate(2);
    assert_eq!(deque.iter().copied().collect::<Vec<_>>(), [6, 8]);
    deque.truncate(usize::MAX);
    assert_eq!(deque.len(), 2);
    deque.clear();
    assert!(deque.is_empty());
    deque.push_front(2);
    deque.push_back(3);
    assert_eq!(Vec::from(deque), [2, 3]);
}

#[derive(Debug)]
struct Counted(Arc<AtomicUsize>);

impl Clone for Counted {
    fn clone(&self) -> Self {
        self.0.fetch_add(1, Ordering::Relaxed);
        Self(Arc::clone(&self.0))
    }
}

#[test]
fn cloning_and_ownership_only_copy_elements_when_shared() {
    let copies = Arc::new(AtomicUsize::new(0));
    let value = || Counted(Arc::clone(&copies));
    let mut deque = Deque::from([value(), value(), value()]);
    deque.push_front(value());
    deque.push_back(value());
    deque.pop_front();
    deque.pop_back();
    deque.get_mut(0).unwrap();
    let original = deque.clone();
    assert_eq!(copies.load(Ordering::Relaxed), 0);
    deque.push_back(value());
    assert_eq!(copies.load(Ordering::Relaxed), 3);
    deque.pop_front();
    deque.into_vec_deque();
    assert_eq!(copies.load(Ordering::Relaxed), 3);
    let shared = original.clone();
    let owned: VecDeque<_> = shared.into();
    assert_eq!(owned.len(), 3);
    assert_eq!(copies.load(Ordering::Relaxed), 6);
    let pointer = original.front().unwrap() as *const Counted;
    let owned = original.into_vec_deque();
    assert_eq!(owned.front().unwrap() as *const Counted, pointer);
    assert_eq!(copies.load(Ordering::Relaxed), 6);
}

#[test]
fn reading_and_cloning_do_not_require_clone_elements() {
    #[derive(Debug, PartialEq)]
    struct NotClone(i32);
    let original = Deque::from([NotClone(1), NotClone(2)]);
    let copy = original.clone();
    assert_eq!(copy.front(), Some(&NotClone(1)));
    assert_eq!(copy.back(), Some(&NotClone(2)));
    assert_eq!(copy.iter().map(|v| v.0).collect::<Vec<_>>(), [1, 2]);
    assert_eq!(copy, original);
}

#[test]
fn conversions_and_iteration_preserve_logical_order() {
    let expected = vec![6, 7, 8, 9, 10, 11];
    let deque = wrapped();
    assert_eq!(Deque::from(expected.clone()), deque);
    assert_eq!(Deque::from(expected.as_slice()), deque);
    assert_eq!(Deque::from([6, 7, 8, 9, 10, 11]), deque);
    assert_eq!((6..12).collect::<Deque<_>>(), deque);
    let borrowed: &VecDeque<_> = deque.borrow();
    assert_eq!(borrowed, deque.as_ref());
    let back: VecDeque<_> = deque.clone().into();
    assert_eq!(Deque::from(back), deque);
    assert_eq!(Vec::from(deque.clone()), expected);
    assert_eq!((&deque).into_iter().copied().collect::<Vec<_>>(), expected);
    let mut iter = deque.clone().into_iter();
    assert_eq!(iter.len(), 6);
    assert_eq!(iter.next(), Some(6));
    assert_eq!(iter.next_back(), Some(11));
    assert_eq!(iter.collect::<Vec<_>>(), [7, 8, 9, 10]);
    assert_eq!(
        deque.into_iter().rev().collect::<Vec<_>>(),
        [11, 10, 9, 8, 7, 6]
    );
}

#[test]
fn value_traits_ignore_physical_layout() {
    let wrapped = wrapped();
    let contiguous = Deque::from([6, 7, 8, 9, 10, 11]);
    assert_eq!(wrapped, contiguous);
    assert_eq!(wrapped.cmp(&contiguous), std::cmp::Ordering::Equal);
    assert!(wrapped < Deque::from([6, 7, 9]));
    assert_eq!(format!("{wrapped:?}"), "[6, 7, 8, 9, 10, 11]");
    let hash = |value: &Deque<i32>| {
        let mut state = DefaultHasher::new();
        value.hash(&mut state);
        state.finish()
    };
    assert_eq!(hash(&wrapped), hash(&contiguous));
    let map = HashMap::from([(wrapped, "found")]);
    assert_eq!(map.get(contiguous.as_vec_deque()), Some(&"found"));
    assert_eq!(
        Deque::from([f64::NAN]).partial_cmp(&Deque::from([1.0])),
        None
    );
}

#[test]
fn send_and_sync_follow_the_elements() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Deque<String>>();
    assert_send_sync::<Deque<Deque<i32>>>();
}

#[cfg(feature = "serde")]
#[test]
fn serde_round_trips_wrapped_shared_and_empty_deques() {
    let original = wrapped();
    let mut copy = original.clone();
    let json = serde_json::to_string(&copy).unwrap();
    assert_eq!(json, "[6,7,8,9,10,11]");
    let mut back: Deque<i32> = serde_json::from_str(&json).unwrap();
    assert_eq!(back, original);
    back.push_front(5);
    copy.push_back(12);
    assert_eq!(original.len(), 6);
    assert_eq!(back.front(), Some(&5));
    assert_eq!(copy.back(), Some(&12));
    assert_eq!(
        serde_json::to_string(&Deque::<String>::new()).unwrap(),
        "[]"
    );
    assert!(
        serde_json::from_str::<Deque<String>>("[]")
            .unwrap()
            .is_empty()
    );
    let strings = Deque::from([String::from("hello"), String::from("world")]);
    assert_eq!(
        serde_json::from_str::<Deque<String>>(&serde_json::to_string(&strings).unwrap()).unwrap(),
        strings
    );
    assert!(serde_json::from_str::<Deque<i32>>("[1,\"bad\"]").is_err());
}
