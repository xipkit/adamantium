#[path = "support/persistent.rs"]
mod persistent;

use std::{hint::black_box, time::Duration};

use adamantium::{Array, Bytes, Deque, Map, Set, Str};
use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group};

fn bench_value<T>(
    c: &mut Criterion,
    name: &str,
    shared: bool,
    setup: impl Fn() -> T,
    operation: impl Fn(&mut T),
) {
    c.bench_with_input(
        BenchmarkId::new(name, if shared { "shared" } else { "unique" }),
        &shared,
        |b, _| {
            b.iter_batched_ref(
                &setup,
                |value| operation(black_box(value)),
                BatchSize::SmallInput,
            );
        },
    );
}

fn cow(c: &mut Criterion) {
    let array: Array<usize> = (0..1_000).collect();
    let map: Map<usize, usize> = (0..1_000).map(|i| (i, i)).collect();
    let deque: Deque<usize> = (0..1_000).collect();
    let set: Set<usize> = (0..1_000).collect();
    let string = Str::from("x".repeat(65_536));
    let bytes = Bytes::from(vec![42; 65_536]);

    for shared in [true, false] {
        let array_setup = || {
            if shared {
                array.clone()
            } else {
                Array::from(array.as_slice().to_vec())
            }
        };
        let map_setup = || {
            if shared {
                map.clone()
            } else {
                Map::from(map.as_index_map().clone())
            }
        };
        bench_value(c, "array/clear/1000", shared, array_setup, Array::clear);
        bench_value(c, "map/clear/1000", shared, map_setup, Map::clear);
        bench_value(
            c,
            "deque/clear/1000",
            shared,
            || {
                if shared {
                    deque.clone()
                } else {
                    Deque::from(deque.as_vec_deque().clone())
                }
            },
            Deque::clear,
        );
        bench_value(
            c,
            "set/clear/1000",
            shared,
            || {
                if shared {
                    set.clone()
                } else {
                    Set::from(set.as_index_set().clone())
                }
            },
            Set::clear,
        );
        bench_value(
            c,
            "str/clear/65536",
            shared,
            || {
                if shared {
                    string.clone()
                } else {
                    Str::from(string.as_str().to_owned())
                }
            },
            Str::clear,
        );
        bench_value(
            c,
            "bytes/clear/65536",
            shared,
            || {
                if shared {
                    bytes.clone()
                } else {
                    Bytes::from(bytes.as_slice().to_vec())
                }
            },
            Bytes::clear,
        );
        bench_value(c, "array/set_invalid/1000", shared, array_setup, |value| {
            black_box(value.set(black_box(1_000), 0)).ok();
        });
        bench_value(
            c,
            "array/get_mut_invalid/1000",
            shared,
            array_setup,
            |value| {
                black_box(value.get_mut(black_box(1_000)));
            },
        );
        for key in [1_000, 500] {
            let kind = if key == 1_000 { "missing" } else { "present" };
            bench_value(
                c,
                &format!("map/remove_{kind}/1000"),
                shared,
                map_setup,
                |value| {
                    black_box(value.remove(black_box(&key)));
                },
            );
            bench_value(
                c,
                &format!("map/get_mut_{kind}/1000"),
                shared,
                map_setup,
                |value| {
                    black_box(value.get_mut(black_box(&key)));
                },
            );
        }
    }
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_millis(300))
        .measurement_time(Duration::from_secs(1))
        .sample_size(30);
    targets = cow, persistent_collections
}
#[allow(
    clippy::exit,
    reason = "invalid standalone profiler arguments must fail"
)]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--profile") {
        if !profile(&args) {
            eprintln!(
                "usage: cow --profile <array|deque|map|set> <cow|im> <size> <repeats>; size/repeats must be positive, size <= 65536"
            );
            std::process::exit(2);
        }
    } else {
        benches();
        Criterion::default().configure_from_args().final_summary();
    }
}

fn persistent_collections(c: &mut Criterion) {
    compare::<Array<usize>>(c);
    compare::<im::Vector<usize>>(c);
    compare::<Deque<usize>>(c);
    compare::<persistent::PersistentDeque>(c);
    compare::<Map<usize, usize>>(c);
    compare::<im::HashMap<usize, usize>>(c);
    compare::<Set<usize>>(c);
    compare::<im::HashSet<usize>>(c);
}

fn compare<T: persistent::Collection>(c: &mut Criterion) {
    let mut group = c.benchmark_group(format!("persistent/{}", T::NAME));
    for size in persistent::SIZES {
        let original = T::build(size);
        group.bench_function(format!("snapshot_edit/{}/{size}", T::BACKEND), |b| {
            b.iter(|| persistent::snapshot_edit(&original, size));
        });
        group.bench_function(format!("unique_edit/{}/{size}", T::BACKEND), |b| {
            b.iter_batched_ref(
                || T::build(size),
                |value| black_box(value).edit(size),
                BatchSize::PerIteration,
            );
        });
        group.bench_function(format!("lookup/{}/{size}", T::BACKEND), |b| {
            b.iter(|| black_box(black_box(&original).lookup(black_box(size / 2))));
        });
        group.bench_function(format!("iterate/{}/{size}", T::BACKEND), |b| {
            b.iter(|| black_box(black_box(&original).checksum()));
        });
    }
    group.finish();
}

fn profile(args: &[String]) -> bool {
    let (Some(collection), Some(backend), Some(size), Some(repeats)) =
        (args.get(2), args.get(3), args.get(4), args.get(5))
    else {
        return false;
    };
    let (Ok(size), Ok(repeats)) = (size.parse::<usize>(), repeats.parse::<usize>()) else {
        return false;
    };
    if size == 0 || size > 65_536 || repeats == 0 || args.len() != 6 {
        return false;
    }
    macro_rules! run {
        ($ty:ty) => {{
            let original = <$ty as persistent::Collection>::build(size);
            persistent::profile_snapshots(&original, size, repeats);
        }};
    }
    match (collection.as_str(), backend.as_str()) {
        ("array", "cow") => run!(Array<usize>),
        ("array", "im") => run!(im::Vector<usize>),
        ("deque", "cow") => run!(Deque<usize>),
        ("deque", "im") => run!(persistent::PersistentDeque),
        ("map", "cow") => run!(Map<usize, usize>),
        ("map", "im") => run!(im::HashMap<usize, usize>),
        ("set", "cow") => run!(Set<usize>),
        ("set", "im") => run!(im::HashSet<usize>),
        _ => return false,
    }
    true
}
