# Copy-on-write benchmarks

Run `cargo bench --locked -p adamantium --bench cow`. Each operation uses a
fresh value: shared cases retain an original owner, and unique cases own a
separate allocation. Criterion's `iter_batched_ref` excludes setup and final
destruction from the measured operation. Clearing still includes dropping the
contents of uniquely owned storage.

To compare two implementations with this same harness and target directory,
run the first command before changing the library and the second afterward:

```sh
cargo bench --locked -p adamantium --bench cow -- --save-baseline before
cargo bench --locked -p adamantium --bench cow -- --baseline before
```

Array and Map inputs contain 1,000 `usize` elements/entries; Str and Bytes
contain 65,536 bytes. Invalid array indices and missing map keys are 1,000;
present map keys are 500. Inputs and return values are passed through
`black_box`. The harness uses 30 samples, a 300 ms warm-up, and a 1 s
measurement window per case. Criterion saves its estimates and samples under
`target/criterion` (or `$CARGO_TARGET_DIR/criterion`).

## ADAM-105 measurements

Local Apple M4, macOS 27.0.1, rustc 1.98.1, release profile with overflow
checks. Before: library at `0443989`, using the same new harness. After: the
ADAM-105 changes. Values below are Criterion's mean point estimates in
nanoseconds per operation; small differences in unique cases are sensitive
to noise and setup's effect on caches.

| Operation | Shared before → after (ns) | Unique before → after (ns) |
| --- | ---: | ---: |
| Array clear | 422.82 → 6.22 | 1.99 → 2.29 |
| Map clear | 2696.79 → 8.35 | 162.00 → 166.89 |
| Str clear | 1540.86 → 6.78 | 2.37 → 2.15 |
| Bytes clear | 1415.40 → 6.96 | 2.98 → 2.16 |
| Array set, invalid | 447.23 → 1.79 | 2.48 → 2.15 |
| Array get_mut, invalid | 437.12 → 1.67 | 2.39 → 2.11 |
| Map remove, missing | 2015.74 → 6.51 | 20.87 → 17.46 |
| Map get_mut, missing | 1905.73 → 6.62 | 18.46 → 17.50 |
| Map remove, present | 3863.72 → 2335.56 | 1346.57 → 1362.31 |
| Map get_mut, present | 1572.49 → 1615.33 | 54.05 → 35.37 |

Separate clone-counter tests avoid timing instrumentation overhead. For a
shared clear, Array element clones fell from 1,000 to zero; Map key clones
and value clones each fell from 1,000 to zero. Unique clears clone nothing
and retain capacity. Shared clears start with zero capacity and preserve the
original value. Invalid or missing updates retain their existing storage in
both ownership cases, including empty collections.
