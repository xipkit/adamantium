use adamantium::{Actor, Array, Bytes, Channel, Map, Str};
#[cfg(feature = "tokio")]
use adamantium::{Task, Tasks};

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn everything_crosses_a_task() {
    assert_send_sync::<Str>();
    assert_send_sync::<Bytes>();
    assert_send_sync::<Array<Str>>();
    assert_send_sync::<Map<Str, Array<i64>>>();
    assert_send_sync::<Actor<Vec<u8>>>();
    assert_send_sync::<Channel<Str>>();
    #[cfg(feature = "tokio")]
    {
        assert_send_sync::<Tasks<String>>();
        assert_send_sync::<Task<i64, String>>();
    }
}

#[test]
fn str_copies_on_write() {
    let a = Str::from("hi");
    let mut b = a.clone();
    assert!(a.is_shared());
    b.push_str("!");
    assert_eq!(a, "hi");
    assert_eq!(b, "hi!");
    assert!(!a.is_shared());
    assert!(!b.is_shared());
    b += "?";
    assert_eq!(b.as_str(), "hi!?");
    assert_eq!(b.len(), 4);
    assert_eq!(format!("{b} {b:?}"), "hi!? \"hi!?\"");
    assert_eq!(b.into_string(), "hi!?");
}

#[test]
fn str_borrows_as_str_for_map_keys() {
    let mut map = Map::<Str, i64>::new();
    map.insert("a".into(), 1);
    assert_eq!(map.get("a"), Some(&1));
    assert!(map.contains_key("a"));
}

#[test]
fn bytes_copies_on_write() {
    let a = Bytes::from(b"ab".as_slice());
    let mut b = a.clone();
    b.push(b'c');
    assert_eq!(a.as_slice(), b"ab");
    assert_eq!(b.as_slice(), b"abc");
    assert_eq!(format!("{b:?}"), "b\"abc\"");
    assert_eq!(Bytes::from(Str::from("hi")).into_vec(), b"hi");
}

#[test]
fn array_copies_on_write() {
    let a = Array::from([1, 2]);
    let mut b = a.clone();
    assert!(b.is_shared());
    b.push(3);
    assert_eq!(a.as_slice(), &[1, 2]);
    assert_eq!(b.as_slice(), &[1, 2, 3]);
    assert!(!a.is_shared());

    let mut c = b.clone();
    if let Some(first) = c.get_mut(0) {
        *first = 10;
    }
    assert_eq!(b.first(), Some(&1));
    assert_eq!(c.first(), Some(&10));
}

#[test]
fn array_positional_writes_do_not_panic() {
    let mut a = Array::from([1, 2, 3]);
    assert_eq!(a.insert(5, 9), Err(9));
    assert_eq!(a.insert(3, 4), Ok(()));
    assert_eq!(a.remove(10), None);
    assert_eq!(a.remove(0), Some(1));
    assert_eq!(a.set(7, 0), Err(0));
    assert_eq!(a.set(0, 20), Ok(()));
    assert_eq!(a.as_slice(), &[20, 3, 4]);
    assert_eq!(a.get(99), None);
    a.sort_by(|x, y| y.cmp(x));
    assert_eq!(a.as_slice(), &[20, 4, 3]);
    assert_eq!(a.iter().sum::<i32>(), 27);
    assert_eq!(a.into_iter().collect::<Vec<_>>(), vec![20, 4, 3]);
}

#[test]
fn map_keeps_insertion_order_and_copies_on_write() {
    let mut a = Map::new();
    a.insert("c", 3);
    a.insert("a", 1);
    a.insert("b", 2);
    assert_eq!(a.keys().copied().collect::<Vec<_>>(), vec!["c", "a", "b"]);

    let mut b = a.clone();
    assert!(b.is_shared());
    b.remove("a");
    b.insert("c", 30);
    b.insert("d", 4);
    assert_eq!(a.keys().copied().collect::<Vec<_>>(), vec!["c", "a", "b"]);
    assert_eq!(a.get("c"), Some(&3));
    assert_eq!(b.keys().copied().collect::<Vec<_>>(), vec!["c", "b", "d"]);
    assert_eq!(b.get("c"), Some(&30));
    assert_eq!(b.first(), Some((&"c", &30)));
    assert_eq!(b.get_index(1), Some((&"b", &2)));

    let same_entries = Map::from([("d", 4), ("b", 2), ("c", 30)]);
    assert_eq!(b, same_entries);
}

#[cfg(feature = "serde")]
#[test]
fn values_round_trip_through_json() {
    let mut map = Map::<Str, Array<i64>>::new();
    map.insert("b".into(), Array::from([2, 3]));
    map.insert("a".into(), Array::from([1]));
    let json = serde_json::to_string(&map).unwrap();
    assert_eq!(json, r#"{"b":[2,3],"a":[1]}"#);
    let back: Map<Str, Array<i64>> = serde_json::from_str(&json).unwrap();
    assert_eq!(back, map);
    assert_eq!(back.keys().map(Str::as_str).collect::<Vec<_>>(), ["b", "a"]);

    let bytes = Bytes::from([1u8, 2]);
    let json = serde_json::to_string(&bytes).unwrap();
    assert_eq!(json, "[1,2]");
    assert_eq!(serde_json::from_str::<Bytes>(&json).unwrap(), bytes);
}
