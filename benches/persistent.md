# Persistent collections: ADAM-127

## Recommendation

Keep the existing `Array`, `Deque`, `Map`, and `Set` backings. Persistent
collections are worth considering as **separate opt-in types for large,
frequently forked values**, not as a transparent backing change or an automatic
size-based switch. This evaluation adds only benchmark dependencies and fixtures;
it does not change the library's storage or public API. A production persistent
type needs a separate design decision and Mauricio's agreement.

The key tradeoff is the number of edits made while another owner exists. These
measurements deliberately fork for every edit. A caller that forks once and then
performs many writes pays the current full copy only once; the unique-write
controls are more relevant to its subsequent writes. Merely having a large
collection is not enough to justify persistence.

## Contract constraints

- `Array` dereferences to `[T]`, borrows slices, and exposes `&mut Vec<T>` through
  `make_mut`. A tree cannot provide those views without flattening.
- `Deque` exposes `VecDeque` through `Deref`, borrowing, and `make_mut`. An RRB
  vector supports queue operations but cannot implement those storage views.
- `Map` and `Set` promise insertion order, ordered serialization, index access,
  and access to their underlying `IndexMap`/`IndexSet`. The candidate hash tries
  have a different iteration order. Their numbers below do **not** price the
  additional structure needed to preserve insertion order.
- All candidates use the thread-safe `im` crate, not `im-rc`, to keep the
  existing Send/Sync expectation. This is an evaluation of `im = 15.1.0`, not a
  dependency maintenance or production-readiness review.

An opt-in API would need to name its iteration guarantees, avoid contiguous
storage promises, and charge explicitly for conversion to/from the existing
values. No ordered persistent map/set implementation was built here.

## Workloads and timing boundaries

`benches/cow.rs` compares the existing values (`cow`) with `im::Vector`,
`im::HashMap`, and `im::HashSet` (`im`). Vector supplies both the array and deque
candidate. Inputs are `usize` values `0..n`, or map entries `(i, i)`, at 16, 64,
256, 1,024, 4,096, 16,384, and 65,536 elements.

| Workload | Edit on the new snapshot |
| --- | --- |
| Array | Replace the middle element with `n` |
| Map | Replace the middle key's value with `n` |
| Deque | Pop the front, then push `n` at the back |
| Set | Insert the absent value `n` |

`snapshot_edit` keeps the original alive and measures clone + edit + destruction
of the edited snapshot. Construction and destruction of the original are outside
timing. This covers the allocation and cleanup costs of transient branches;
long-lived snapshot retention and peak memory are not measured.

`unique_edit` constructs fresh, genuinely unique storage for each edit using
Criterion's `iter_batched_ref` with `PerIteration`. Construction and final drop
are excluded, but per-iteration timer overhead remains. The COW deque and set
reserve `n + 1` capacity to avoid accidental unique-storage growth. Persistent
nodes retain their natural capacities: chunk boundaries are part of the cost.
These one-edit controls are not steady-state throughput measurements.

`lookup` reads the middle element/key; `iterate` sums all elements/map values.
Both use an unchanged collection. Inputs, returned values, and snapshots pass
through `black_box`. Successful edits and preservation of the original snapshot
are checked in `tests/persistent_workloads.rs` for both backends at every size.
The original clear/missing-key benchmarks remain available separately.

Both backends use their normal randomized hashers. Each timed case reuses one
constructed original; three independent Callgrind processes per case sample
different hash layouts. Timings can move with hash layout, allocation behavior,
cache locality, payload clone cost, and fleet load. In particular, expensive
payload clones can move a crossover to smaller collections. Powers of four are
sampling points, not exact crossover thresholds.

## Wall-clock measurements

Measured 2026-11-14 UTC on Apple M4, macOS 27.0.1, rustc 1.98.1
(Homebrew), optimized bench profile with overflow checks. Baseline library:
`bfa0f78`. These are side-by-side current/candidate measurements, not an applied
runtime optimization. Values are Criterion **mean** point estimates. The fleet
can move timings by about 10%; small differences are not a decision boundary.

Snapshot clone + edit + drop, nanoseconds, current COW → im:

| Elements | Array | Deque | Map (unordered candidate) | Set (unordered candidate) |
| ---: | ---: | ---: | ---: | ---: |
| 16 | 34.9 → 57.3 | 49.6 → 56.4 | 116.7 → 131.3 | 107.3 → 156.2 |
| 64 | 55.0 → 65.9 | 68.3 → 119.6 | 152.3 → 259.5 | 142.5 → 302.3 |
| 256 | 65.8 → 62.3 | 93.3 → 312.1 | 300.7 → 291.0 | 285.5 → 304.9 |
| 1,024 | 141.0 → 168.7 | 177.7 → 293.9 | 2,190.1 → 428.9 | 909.2 → 384.0 |
| 4,096 | 566.9 → 431.4 | 538.5 → 394.8 | 4,783.9 → 405.6 | 3,961.6 → 417.5 |
| 16,384 | 2,362.0 → 541.5 | 2,033.4 → 489.1 | 24,615.4 → 620.0 | 24,841.4 → 580.2 |
| 65,536 | 8,439.4 → 418.3 | 7,572.0 → 518.0 | 136,184.6 → 618.7 | 117,973.7 → 495.8 |

The useful sampled time bracket is **1,024–4,096 for Array/Deque** and
**64–1,024 for Map/Set**, with COW competitive at the lower hash-collection
sample and persistence clearly ahead at the upper one. At 256, the hash collections are close enough to be
noise-sensitive. Array's apparent small win at 256 also disappears at 1,024;
`im::Vector` changes representation as it grows. Do not infer a universal
threshold or dispatch on collection length from these data.

The snapshot-only repeat kept the clear wins at the upper ends of those brackets,
although Map at 64 was now tied. Some timings changed by more than the expected
10% fleet noise; these runs also change allocator history and hash layout, so
the data do not isolate one cause. Repeat means below are in nanoseconds
(current COW → im). Both runs, including 95% confidence
intervals for their means and all control cases from the full run, are preserved
in [adam-127-time.csv](results/adam-127-time.csv).

| Collection | Lower bracket | Upper bracket | 65,536 elements |
| --- | ---: | ---: | ---: |
| Array | 1,024: 128.6 → 157.6 | 4,096: 624.1 → 263.5 | 7,397.1 → 354.1 |
| Deque | 1,024: 137.5 → 294.4 | 4,096: 526.2 → 404.7 | 8,919.5 → 523.4 |
| Map | 64: 150.3 → 152.1 | 1,024: 2,135.2 → 304.1 | 134,118.2 → 584.4 |
| Set | 64: 145.4 → 284.9 | 1,024: 927.8 → 384.8 | 120,878.1 → 575.9 |

Controls at 65,536 elements, nanoseconds, current COW → im:

| Collection | Fresh unique edit | Middle lookup | Full iteration |
| --- | ---: | ---: | ---: |
| Array | 214.9 → 64.0 | 1.0 → 7.9 | 18,538.7 → 117,014.7 |
| Deque | 193.3 → 68.5 | 1.0 → 7.1 | 16,615.0 → 104,642.0 |
| Map | 52.5 → 70.0 | 7.8 → 7.8 | 16,740.6 → 1,938,574.9 |
| Set | 38.2 → 51.5 | 7.1 → 6.3 | 16,754.4 → 1,694,159.0 |

At this size, persistence makes full iteration roughly 6× slower for the vectors
and 100–116× slower for the hash collections in this sum workload. Hash lookups
are comparable; vector lookups are slower. Unique-edit results are mixed:
setup can leave different cache footprints for a large contiguous buffer and
for tree leaves, so the vector unique numbers must not be read as steady-state
write speedups. A branch that immediately
scans its entire collection needs its own end-to-end benchmark before adopting
persistence. This report does not evaluate long edit batches, realistic payloads,
retained-snapshot memory, or an insertion-ordered persistent hash collection.

## Instruction measurements

Measured 2026-11-14 UTC on Linux x86_64 (Intel Core Ultra 9 285H),
rustc 1.99.1 (Arch), Valgrind 3.25.1. Optimized bench profile, overflow checks
on, debug symbols enabled; no target-native flags. Each cell is the median
instructions per snapshot edit across three processes of 100 edits each,
rounded to whole instructions. Full per-run totals for all seven sizes are in
[adam-127-instructions.csv](results/adam-127-instructions.csv).

| Collection | Elements | Current COW (Ir/edit) | im (Ir/edit) | COW / im |
| --- | ---: | ---: | ---: | ---: |
| Array | 1,024 | 1,303 | 2,364 | 0.55 |
| Array | 4,096 | 33,281 | 4,670 | 7.13 |
| Array | 65,536 | 524,817 | 6,585 | 79.70 |
| Deque | 1,024 | 2,468 | 3,962 | 0.62 |
| Deque | 4,096 | 7,849 | 6,266 | 1.25 |
| Deque | 65,536 | 115,385 | 8,403 | 13.73 |
| Map | 64 | 2,926 | 3,978 | 0.74 |
| Map | 256 | 7,940 | 5,800 | 1.37 |
| Map | 1,024 | 27,961 | 6,344 | 4.41 |
| Map | 65,536 | 1,828,128 | 9,465 | 193.15 |
| Set | 64 | 2,978 | 3,531 | 0.84 |
| Set | 256 | 7,507 | 4,275 | 1.76 |
| Set | 1,024 | 25,613 | 5,747 | 4.46 |
| Set | 65,536 | 1,664,502 | 9,096 | 182.99 |

The larger-size instruction crossover lies between 1,024 and 4,096 for
Array/Deque, matching their time bracket. For Map/Set, COW wins at 64 and im
wins at 1,024, also corroborating the time bracket; the instruction advantage
already appears at 256, where Mac timing is still close. This supports the
large-collection direction independently of scheduling noise without claiming
identical time and instruction crossovers across platforms.

## Reproduction

Wall-clock run (30 samples, 300 ms warm-up, 1 s measurement per case):

```sh
export CARGO_TARGET_DIR=/Volumes/AI/Build/tron-adam-127/target
cargo bench --locked -p adamantium --bench cow -- persistent --save-baseline adam127
cargo bench --locked -p adamantium --bench cow -- 'persistent/.*/snapshot_edit' --save-baseline adam127-repeat
```

Instruction counts require Linux with Valgrind/Callgrind and Python 3. Compile
with symbols, then use the executable path printed by Cargo:

```sh
CARGO_PROFILE_BENCH_DEBUG=1 cargo bench --locked -p adamantium --bench cow --no-run
python3 benches/profile-persistent.py /absolute/path/to/cow-binary /tmp/cow-counts
```

The script accepts `--sizes`, `--repeats`, `--runs`, and `--valgrind`; it preserves
each raw Callgrind profile and stderr alongside `instructions.csv`. The binary's
standalone mode is `cow-binary --profile array cow 1024 100`. Callgrind starts
with collection disabled and toggles only inside the non-inlined
`profile_snapshots` function, so setup, runtime startup, and original destruction
are excluded. Counts include the loop and edited-snapshot destruction, matching
the snapshot timing boundary. A scaling check at Array/COW/1,024 gave 130,313
instructions for 100 iterations and 260,613 for 200 (13 fixed instructions).

Counts are executed guest instructions (`Ir`), **not CPU cycles**, and must not
be converted to Apple M4 time. Different architectures, compilers, libc copy
routines, and Callgrind's treatment of repeated string instructions can change
the ratio. Their purpose is to check work growth independently of host load.

References: [im 15.1.0](https://docs.rs/im/15.1.0/im/),
[Vector representation](https://docs.rs/im/15.1.0/im/vector/struct.Vector.html),
[Callgrind collection boundaries](https://valgrind.org/docs/manual/cl-manual.html).
