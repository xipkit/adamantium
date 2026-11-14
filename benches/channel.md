# Channel scheduling replacement gate

ADAM-159 measurements on OTIS, Apple M4, macOS 27.0.1, rustc 1.99.1,
2026-11-14. Release profile, overflow checks enabled, default features.
The baseline is `ce1ffb9`, using exactly the same benchmark as the candidate.
The current candidate includes the retained-stream wakeup correction and uses
one Channel algorithm over the bounded-storage and wait-queue
seams. Production storage is concurrent-queue 2.5.0, already used by
async-channel; modeled storage is a locked FIFO. Watch shares the wait queue.

Each invocation creates a four-worker Tokio runtime, starts producers and
receivers at a barrier, sends numbered `u64` messages, closes by dropping the
last sender and drains the queue. The harness checks both count and checksum.
Runtime construction is outside the timed region; channel/task construction,
message delivery and joining are inside it.

Three warmups per binary precede 20 paired A/B observations. The order alternates
AB/BA. Twenty same-baseline A/A observations are interleaved, also order-balanced;
each workload has its own control. Measurements below are elapsed milliseconds
(lower is better). Total message counts differ across workloads and are not
cross-workload comparisons.

| Producers/receivers | Capacity | Messages | Baseline median | Candidate median | Paired B/A median | A/A median [quartiles] |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| 4/4 | 64 | 4,000,000 | 345.749 | 278.269 | 0.787 | 1.015 [0.961, 1.180] |
| 1/1 | 64 | 2,000,000 | 118.758 | 103.149 | 0.878 | 0.999 [0.852, 1.116] |
| 4/4 | 1 | 400,000 | 344.980 | 102.788 | 0.294 | 1.022 [1.003, 1.075] |

The candidate's paired median is below the A/A median in every workload. Both
4/4 medians are below the A/A interquartile range; the 1/1 median falls within
that range. There is no regression beyond the measured control spread, so the
production replacement gate still passes. These are local measurements on a
shared machine, not a universal speedup claim.

Stream listeners now use a separate instance of the same WaitQueue, notified
with wake-all on send and close. Cancelling `next()` retains the listener on its
receiver, so letting it compete for receive's single notification could leave an
active receive or another stream asleep with a message queued. Each woken stream
rechecks the same bounded queue; each message still goes to one receiver.
The table above reruns the complete gate with that correction.

The initial mutex-only storage candidate failed the gate: 378.258 → 437.424 ms
for 4/4 at capacity 64; median paired B/A was 1.213 versus an A/A median of 0.963.
Flynn requested a storage-only probe before adding a bounded primitive to the
seam. That probe held endpoint counts under the existing lock and changed only
storage to concurrent-queue: 339.633 → 226.781 ms; paired B/A 0.633 versus A/A
0.999. The final implementation keeps Channel's closure, cancellation handoff
and notification logic shared, changing only bounded storage inside an execution.

[Raw pairs and environment](results/adam-159/) include every observation, order,
load average, summary and binary SHA-256. The `stream-fix-*.jsonl` files and
`stream-fix` binary hash correspond to the current table; earlier measurements
are retained separately. The first mutex experiment used a fixed
4/4 benchmark; subsequent probes and final runs use the parameterized harness.

To reproduce, build `cargo build --locked --release --bench channel` at the
baseline and candidate revisions, using the same `benches/channel.rs` and bench
manifest entry, and copy each executable out of `target/release/deps/` before
building the other revision. Then run:

```sh
python3 benches/compare-channel.py BASELINE CANDIDATE
python3 benches/compare-channel.py BASELINE CANDIDATE --producers 1 --receivers 1 --count 2000000
python3 benches/compare-channel.py BASELINE CANDIDATE --capacity 1 --count 100000
```

The executable arguments are capacity, messages per producer, producer count
and receiver count. Run builds and other local tests before timing to avoid
adding avoidable load to the comparison.
