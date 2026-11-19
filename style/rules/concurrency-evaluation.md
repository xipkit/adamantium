# Semantic concurrency Decisions evaluation

Held-out evaluation of the OpenAI Decisions API (`gpt-6-luna`) on four candidate semantic concurrency rules for Adamantium:

1. `stale_read_across_await`
2. `partial_mutation_on_cancellation`
3. `split_actor_invariant`
4. `unbounded_collection_growth`

The candidate rule definitions live in [`concurrency-decisions.json`](concurrency-decisions.json) and the synthetic labeled corpus lives in [`../fixtures/concurrency-cases.json`](../fixtures/concurrency-cases.json).

## Corpus and protocol

- **Synthetic only.** Every example is newly written synthetic Rust 2024 code (no repository or customer source was sent to OpenAI). All 104 snippets parse cleanly with `rustfmt --edition 2024`.
- **Splits.** The corpus is split into 16 `dev` cases (4 per rule) and 88 `held_out` cases (22 per rule). All 104 cases were written before any API request was sent.
- **Held-out composition per rule (22 cases):**
  - 8 `semantic_only` violations not caught by existing Adamantium Clippy or experimental Dylint lints.
  - 8 `semantic_only` paired safe cases (minimal fixes using `KeyedLimit`, `AsyncActor`, `Drop` guards, atomic `Actor::with` updates, version/epoch checks, bounded eviction, or fixed-enum keys).
  - 2 `lint_overlap` violations: genuine violations of the semantic rule that existing Adamantium lints also catch (`std::sync::Mutex`, `LocalKey::with`, `tokio::time::timeout`, `adamantium_race_with_timer`, `tokio::spawn`).
  - 2 `lint_decoy` negatives (`no_violation`): snippets that violate an existing Adamantium lint (`tokio::sync::Mutex` held across await, `std::thread::sleep` / `adamantium_blocking_in_async`, drop-safe `tokio::time::timeout` / `tokio::select!`, single-state `Rc<RefCell<_>>`, bounded `tokio::sync::broadcast`, bounded `Mutex<VecDeque>`) without committing the target semantic bug.
  - 2 `insufficient_context` cases where external types, guards, or omitted methods prevent a sound judgment.
- **Dev tuning discipline.** One dev run (16 requests) scored 15/16; the single miss was `unbounded_dev_profile_cache_bug`, where `unbounded_collection_growth` abstained as `insufficient_context` because callers were not shown. Only `unbounded_collection_growth`'s instructions were clarified (explicitly distinguishing complete single-file modules with open-ended keys from incomplete excerpts), re-verified on its 4 dev cases (4/4), and then all four rules were frozen before running the single untouched 88-case held-out pass.
- **Metric definition.** Precision and recall treat `violation` as the positive class (matching `cargo adamantium style`, which emits a finding only on `violation` and records `insufficient_context` or `refusal` as unresolved). Paired discrimination counts pairs where the buggy case predicted `violation` and its paired safe case predicted `no_violation`.

## Billable request count

Evaluated on 2026-11-19 via [`../scripts/evaluate-decisions.mjs`](../scripts/evaluate-decisions.mjs) against `https://api.openai.com/v1/decisions` (`model: "gpt-6-luna"`):

| Run | Cases | Billable requests | Input tokens | Refusals |
| --- | --- | ---: | ---: | ---: |
| Dev round 1 (`--split dev`) | 4 rules x 4 | 16 | 8,513 | 0 |
| Dev round 2 (`--split dev --rule unbounded_collection_growth`) | 1 rule x 4 | 4 | 2,404 | 0 |
| Held-out run (`--split held_out`, frozen rules) | 4 rules x 22 | 88 | 50,391 | 0 |
| Held-out fixture-fix rerun (4 revised cases, same frozen rules) | 4 revised cases | 4 | 2,427 | 0 |
| **Total** | | **112** | **63,735** | **0** |

Dry-run reproduction without API calls:

```sh
node style/scripts/evaluate-decisions.mjs --dry-run \
  --rules style/rules/concurrency-decisions.json \
  --cases style/fixtures/concurrency-cases.json \
  --split held_out
```

Live held-out reproduction (88 billable requests):

```sh
node style/scripts/evaluate-decisions.mjs --metrics \
  --rules style/rules/concurrency-decisions.json \
  --cases style/fixtures/concurrency-cases.json \
  --split held_out
```

## Held-out results per rule

Using the raw argmax verdict returned by Decisions (`choice == "violation"`):

| Rule | Precision | Recall | F1 | Exact 3-class | Paired (`bug`+`safe`) |
| --- | ---: | ---: | ---: | ---: | ---: |
| `split_actor_invariant` | 100.0% (10/10) | 100.0% (10/10) | 100.0% | 20/22 (90.9%) | 8/8 (100.0%) |
| `stale_read_across_await` | 100.0% (9/9) | 90.0% (9/10) | 94.7% | 21/22 (95.5%) | 7/8 (87.5%) |
| `partial_mutation_on_cancellation` | 83.3% (10/12) | 100.0% (10/10) | 90.9% | 20/22 (90.9%) | 6/8 (75.0%) |
| `unbounded_collection_growth` | 100.0% (8/8) | 80.0% (8/10) | 88.9% | 20/22 (90.9%) | 7/8 (87.5%) |
| **Overall** | **94.9% (37/39)** | **92.5% (37/40)** | **93.7%** | **81/88 (92.0%)** | **28/32 (87.5%)** |

### Breakdown by slice (including existing-lint cases)

| Rule | `semantic_only` (TP / FP / FN of 8+8) | `lint_overlap` recall (of 2) | `lint_decoy` FP (of 2) | `insufficient_context` FP (of 2) |
| --- | --- | ---: | ---: | ---: |
| `split_actor_invariant` | 8 / 0 / 0 | 2/2 (100%) | 0/2 | 0/2 |
| `stale_read_across_await` | 7 / 0 / 1 | 2/2 (100%) | 0/2 | 0/2 |
| `partial_mutation_on_cancellation` | 8 / 2 / 0 | 2/2 (100%) | 0/2 | 0/2 |
| `unbounded_collection_growth` | 7 / 0 / 1 | 1/2 (50%) | 0/2 | 0/2 |
| **Overall** | **30 / 2 / 2** | **7/8 (87.5%)** | **0/8 (0%)** | **0/8 (0%)** |

Key observations on existing-lint overlap:

- **No cross-lint false positives.** Decisions produced 0/8 false positives on snippets that triggered unrelated Clippy or experimental Dylint rules (`tokio::sync::Mutex` held across await, `std::thread::sleep` in async, drop-safe `tokio::time::timeout` / `tokio::select!`, single-state `Rc<RefCell<_>>`, bounded `broadcast::channel(64)`, and bounded `Mutex<VecDeque>`).
- **Expected overlap on dual-hit code.** When a snippet both violates a semantic rule and uses a disallowed primitive (`std::sync::Mutex`, `LocalKey::with`, `tokio::time::timeout`, `adamantium_race_with_timer`, `tokio::spawn`), Decisions flagged 7/8 cases. Because Decisions reviews source without running Clippy, users running the concurrency pack on code that has not yet passed `cargo adamantium check` will see both findings.

### Probability threshold check (no extra requests)

Both false positives in the entire held-out run were low-confidence `partial_mutation_on_cancellation` predictions (`P(violation)` = 0.53 and 0.55; `confidence` = 0.30 and 0.33). Gating emitted findings on `P(violation) >= 0.60` eliminates all false positives while dropping one low-confidence true positive (`cancel_ho_pop_before_send_bug`, `P(violation)` = 0.42):

| Threshold (`P(violation) >= t`) | `split_actor_invariant` | `stale_read_across_await` | `partial_mutation_on_cancellation` | `unbounded_collection_growth` | Overall |
| --- | --- | --- | --- | --- | --- |
| Argmax (CLI default) | P 100%, R 100% | P 100%, R 90% | P 83.3%, R 100% | P 100%, R 80% | P 94.9%, R 92.5% |
| `t = 0.60` | P 100%, R 100% | P 100%, R 90% | P 100.0%, R 90% | P 100%, R 80% | P 100.0%, R 90.0% |
| `t = 0.70` | P 100%, R 100% | P 100%, R 80% | P 100.0%, R 90% | P 100%, R 80% | P 100.0%, R 87.5% |
| `t = 0.80` | P 100%, R 90% | P 100%, R 80% | P 100.0%, R 90% | P 100%, R 70% | P 100.0%, R 82.5% |

## Representative mistakes per rule

All 7 held-out mismatches (5 finding-level errors + 2 non-finding label confusions) are listed below.

### `stale_read_across_await` (1 mistake: 1 FN, 0 FP)

- **False negative — `stale_ho_cache_write_through_bug`** (`semantic_only`, predicted `no_violation`, `P(violation)=0.340`, `confidence=0.480`):
  `get_or_load` checks the cache (`read` returns `None`), awaits `load_from_db(&key)`, and then unconditionally inserts the loaded value, clobbering any concurrent `put(key, newer)` that arrived while the DB load was in flight. Because the pre-await read returned `None` rather than a value carried into the write, Decisions treated it as a benign cache fill. Even on the paired safe version (`stale_ho_cache_write_through_safe`), `P(violation)` stayed elevated at `0.410`, showing weak separation on cache-miss write-through races.

### `partial_mutation_on_cancellation` (2 mistakes: 0 FN, 2 FP)

- **False positive — `cancel_ho_job_status_stuck_safe`** (`semantic_only`, predicted `violation`, `P(violation)=0.530`, `confidence=0.300`):
  Sets `Status::Running`, constructs a `RunningGuard` whose `Drop` resets `Status::Pending` when `!self.finished`, awaits `execute_remote_job`, and synchronously sets `Status::Done` and `guard.finished = true`. Decisions still leaned slightly toward `violation` (`0.53` vs `0.46`), whereas it cleanly passed the structurally similar `cancel_ho_inventory_reservation_safe` (`P(violation)=0.040`).
- **False positive — `cancel_ho_pop_before_send_safe`** (`semantic_only`, predicted `violation`, `P(violation)=0.550`, `confidence=0.330`):
  Peeks `outbox.read(|q| q.front().cloned())`, awaits `tx.send(msg)`, and pops only after `send` returns. There is no pre-await mutation, yet Decisions flagged it with low confidence—and on the paired buggy case (`cancel_ho_pop_before_send_bug`, which pops *before* `tx.send().await`), it barely returned `violation` (`P(violation)=0.420`, `confidence=0.130`). Decisions has weak discrimination on queue-peek vs queue-pop ordering across channel sends.

### `split_actor_invariant` (2 non-finding label confusions: 0 FN, 0 FP)

Both mismatches had `P(violation)=0.000` and would never emit a false finding:

- **Over-conservative abstention — `split_ho_decoy_blocking_independent_actors`** (`lint_decoy` with `std::thread::sleep` / `adamantium_blocking_in_async`, expected `no_violation`, predicted `insufficient_context`, `P(violation)=0.000`, `confidence=0.970`):
  Despite context stating `feature_flags` and `health_pings` are unrelated subsystems, Decisions abstained as `insufficient_context` (`0.98`) rather than `no_violation`. In the CLI this would surface as `unresolved` rather than clean.
- **Under-abstention — `split_ho_insuff_actor_and_flag`** (`insufficient_context`, expected `insufficient_context`, predicted `no_violation`, `P(violation)=0.000`, `confidence=0.530`):
  `Service { inner: Actor<Inner>, dirty: AtomicBool }` omits `Inner` and the readers of `dirty`, but Decisions leaned `no_violation` (`0.69`) over `insufficient_context` (`0.31`).

### `unbounded_collection_growth` (2 mistakes: 2 FN, 0 FP)

- **False negative — `unbounded_ho_pending_rpc_map_bug`** (`semantic_only`, predicted `no_violation`, `P(violation)=0.230`, `confidence=0.660`):
  `RpcMux` inserts a `Reply` in `register` and removes it in `on_response`, with no `Drop` cleanup, sweep, or cap when callers cancel or remote peers never reply. Seeing a `remove` method on the happy path caused Decisions to treat the map as bounded and miss the leak on cancellation/timeout paths.
- **False negative — `unbounded_ho_lint_spawn_log_vec`** (`lint_overlap` with `tokio::spawn`, predicted `no_violation`, `P(violation)=0.290`, `confidence=0.460`):
  A long-running spawned loop allocates `let mut log = Vec::new()` locally and pushes every received message forever. Because `log` is a local variable inside the async block rather than a struct field, Decisions treated it as a local collection. Clippy already flags `tokio::spawn`, though the same unbounded local `Vec` inside a long-running `Tasks::spawn` loop would be missed by both Clippy and this rule.

## Recommendations for ADAM-208

1. **`split_actor_invariant` — Ship.** 100% precision, 100% recall, 8/8 pairs, 0 false positives on decoys and `insufficient_context`. Note that unrelated multi-actor helpers may occasionally report `unresolved (insufficient_context)` when the file lacks enough context to prove independence.
2. **`stale_read_across_await` — Ship.** 100% precision, 90% recall, 7/8 pairs, 0 false positives. Reliably catches check-then-act, snapshot-overwrite, Watch writeback, atomic read-modify-write, and term-check races while recognizing `KeyedLimit`, `AsyncActor`, epoch/version checks, and locks held across await. Document that cache-miss write-through clobber races are a known blind spot.
3. **`unbounded_collection_growth` — Ship.** 100% precision, 80% recall, 7/8 pairs, 0 false positives. Cleanly catches unbounded caches, dedup sets, dead-letter deques, rate tables, and raw-path metrics without flagging bounded ring buffers, fixed-enum maps, `Drop`-guarded registries, or channels. Document two recall gaps: happy-path-only `remove` leaks on cancellation/timeout, and unbounded local `Vec`s inside long-lived task loops.
4. **`partial_mutation_on_cancellation` — Ship with a probability threshold (`P(violation) >= 0.60`), or defer until ADAM-208 adds threshold gating.** At raw argmax it had the only false positives in the evaluation (83.3% precision, 6/8 pairs), both at `P(violation) <= 0.55` (`confidence <= 0.33`). Adding a `P(violation) >= 0.60` gate (reporting sub-threshold `violation` choices as `unresolved` or clean) raises precision to 100% (9/9) at 90% recall (9/10) across this rule and 100% precision (36/36) / 90% recall (36/40) across the full four-rule pack.
