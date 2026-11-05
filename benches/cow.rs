use std::{hint::black_box, time::Duration};

use adamantium::{Array, Bytes, Map, Str};
use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};

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
    targets = cow
}
criterion_main!(benches);
