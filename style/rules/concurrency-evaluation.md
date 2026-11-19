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

## Whole-file CLI evaluation (ADAM-222)

When ADAM-208 replayed the original 88 snippets through `cargo adamantium style check --pack concurrency` (developer message + `input_json`, all rules together, no per-case `--context`), `stale_read_across_await` and `unbounded_collection_growth` flagged correct code (`stale_ho_quota_check_safe`, `stale_ho_leader_term_safe`, `cancel_ho_pop_before_send_safe`, `split_ho_cache_and_lru_safe`, `split_ho_lease_active_and_owner_safe`). Only `split_actor_invariant` shipped in ADAM-208.

### Scorer and corpus changes

- **CLI-only scoring.** [`../scripts/evaluate-decisions.mjs`](../scripts/evaluate-decisions.mjs) no longer builds its own Decisions API requests or calls OpenAI directly. It writes each case to a temporary `.rs` file, runs the built `cargo adamantium style check --pack <pack> --format json`, and scores the CLI's JSON report (including line ranges and unresolved answers). Cases pass `--context` only when their label depends on it and record why in `context_reason`.
- **Fresh whole-file held-out split.** All 104 original snippets were moved to `split: "dev"` (with unnecessary per-case `context` removed). Before any wording was edited, 44 new synthetic Rust 2024 whole-file cases were written and committed as `split: "held_out"` (57–178 lines, median 96 lines, 10 files >= 150 lines; all validated with `rustfmt --edition 2024`). Every held-out case is labeled for all three rules evaluated together (`split_actor_invariant`, `stale_read_across_await`, `unbounded_collection_growth`), with pairs covering every confusion and known miss from ADAM-207/ADAM-208:
  - check-and-charge in one `Actor::with` before `.await` (`wf_stale_budget_charge`),
  - post-await epoch/role recheck (`wf_stale_primary_epoch`),
  - cache-miss write-through on a bounded cache (`wf_stale_catalog_cache`),
  - capped map with companion `VecDeque` eviction (`wf_unbounded_dns_cache` and cross-rule in `wf_split_asset_cache` / `wf_stale_catalog_cache`),
  - synchronous map with paired `remove` (`wf_unbounded_lock_table` and cross-rule in `wf_split_worker_leases`),
  - happy-path-only `remove` on an async RPC map vs capped + `Drop` guard (`wf_unbounded_rpc_inflight`).
- **Wording discipline.** `split_actor_invariant` was not touched. `stale_read_across_await` and `unbounded_collection_growth` were revised against `dev` only, rebuilding `cargo-adamantium` for each edit, then frozen before running `held_out` once.

### Held-out results through the CLI (44 whole files, 132 rule evaluations)

Single frozen 3-rule held-out run (`193,157` input tokens):

| Rule | Precision | Recall | F1 | Exact 3-class | Pairs (`bug`+`safe`) | Unresolved | Located true findings | Safe/decoy FP |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `unbounded_collection_growth` | **100.0% (7/7)** | **100.0% (7/7)** | **100.0%** | **44/44 (100.0%)** | **6/6** | 1 (1 true `insufficient_context`) | 5/7 | **0** |
| `stale_read_across_await` | 70.0% (7/10) | 100.0% (7/7) | 82.4% | 40/44 (90.9%) | 3/6 | 1 (`wf_ho_stale_flag_patch_safe`) | 7/7 | **2** (+1 `insufficient_context` FP) |
| `split_actor_invariant` (untouched) | 60.0% (6/10) | 100.0% (6/6) | 75.0% | 40/44 (90.9%) | 5/5 | 1 (1 true `insufficient_context`) | 5/6 | **4** (all cross-rule on `stale_*`) |
| **Overall (3-rule)** | **74.1% (20/27)** | **100.0% (20/20)** | **85.1%** | **124/132 (93.9%)** | **14/17** | **3** | **17/20** | **6** |

Verification of the shipped 2-rule binary (`split_actor_invariant` + `unbounded_collection_growth`, `134,211` input tokens on the same 44 held-out files) produced identical verdicts and probabilities to three decimal places:

- `unbounded_collection_growth`: **100.0% precision (7/7)**, **100.0% recall (7/7)**, **44/44 exact**, **0 false positives**, **1 unresolved** (`wf_ho_unbounded_insuff_external_store`), **5/7 located** (`wf_ho_unbounded_lock_table_bug` and `wf_ho_unbounded_rpc_inflight_bug` returned file scope without a line range).
- `split_actor_invariant`: **6/6 recall**, **0 false positives** on its own 13 cases and all 15 `unbounded_collection_growth` cases, **1 unresolved** (`wf_ho_split_insuff_external_actors`), **5/6 located** (`wf_ho_split_asset_cache_bug` had no line range), and the same **4 cross-rule false positives** on single-Actor / single-Atomic / single-Mutex async files (`wf_ho_stale_budget_charge_bug` at 0.750, `wf_ho_stale_cursor_advance_bug` at 0.900, `wf_ho_stale_cursor_advance_safe` at 0.750, `wf_ho_stale_lint_dropped_mutex_bug` at 0.670). Running `--rule split_actor_invariant` alone on `wf_ho_stale_cursor_advance_safe` reproduced the false positive (confidence 0.630), confirming it is pre-existing in `split_actor_invariant` on whole files and independent of `unbounded_collection_growth` (filed as ADAM-223).
- On Adamantium's own `src/` (36 files, `151,544` input tokens), the shipped 2-rule binary exited `0` with **0 findings** and **0 unresolved**.

### Ship decisions (ADAM-222)

1. **`unbounded_collection_growth` — Added to `--pack concurrency`.** Passed the whole-file CLI gate cleanly (7/7 TP, 0 FP across all 37 negative/cross-rule/insufficient cases, 6/6 pairs, 0 findings on `adamantium/src/`), including both prior recall misses (happy-path-only `remove` on async RPC maps and local `Vec`s in long-running receive loops).
2. **`stale_read_across_await` — Held out of `--pack concurrency`.** Despite reaching 100% precision and 100% recall on `dev`, on the untouched whole-file held-out split it flagged 2 safe files (`wf_ho_stale_primary_epoch_safe` at P=0.810 despite explicit post-await epoch/role recheck, and `wf_ho_stale_cursor_advance_safe` at P=0.520 despite atomic `fetch_add` before await) plus 1 `insufficient_context` file (`wf_ho_stale_insuff_external_ledger` at P=0.500), and left `wf_ho_stale_flag_patch_safe` unresolved.
3. **`split_actor_invariant` — Untouched per ADAM-222; follow-up filed as ADAM-223.**

## Single-primitive `split_actor_invariant` revision (ADAM-223)

### Corpus and tuning discipline

- **Fresh whole-file held-out split.** Before editing any rule wording, all 44 ADAM-222 whole-file cases were moved to `split: "dev"` (bringing `dev` to 148 cases) and 40 fresh synthetic Rust 2024 whole-file cases were written and committed as `split: "held_out"` (36–213 lines, median 76 lines, 7 files >= 150 lines; all validated with `rustfmt --edition 2024`). Every held-out case is labeled for all three pack candidate rules and covers all four slices (`semantic_only`, `lint_overlap`, `lint_decoy`, `insufficient_context`), including 14 async single-primitive files combining one `Actor`, `AtomicU64`, `Arc<std::sync::Mutex<_>>`, `Arc<std::sync::RwLock<_>>`, or `Arc<tokio::sync::Mutex<_>>` with an immutable config struct.
- **Dev tuning (`split_actor_invariant` only).** `split_actor_invariant` was revised in [`concurrency-decisions.json`](concurrency-decisions.json) (and kept byte-identical in [`../../cli/src/style/concurrency-decisions.json`](../../cli/src/style/concurrency-decisions.json)) to clarify that a single `Actor`, `Atomic`, `Mutex`, `RwLock`, or `RefCell` alongside immutable fields or config is never a split invariant—even across multiple methods, closures, lock acquisitions, or `.await` points—and to exclude unrelated single-primitive bugs such as stale reads or dropped mutex guards across `.await`. `unbounded_collection_growth` was not touched.
  - Dev round 1 (`--split dev --rule split_actor_invariant`, 75 cases, `110,497` input tokens): 100.0% precision (16/16), 94.1% recall (16/17), 44/44 exact on the ADAM-222 whole files (0 FP across all 4 prior false positives).
  - Dev round 2 (tightened wording, `--split dev --rule split_actor_invariant`, 75 cases, `107,473` input tokens): 100.0% precision (16/16), 94.1% recall (16/17), 14/14 pairs, 44/44 exact on the ADAM-222 whole files.
  - Full 2-rule check on the 44 ADAM-222 dev whole files (`125,104` input tokens): **88/88 exact (100.0%)**, 100.0% precision (13/13), 100.0% recall (13/13), 11/11 pairs, 0 false positives.

### Frozen held-out results through the CLI (40 fresh whole files, 80 rule evaluations)

Single frozen 2-rule held-out run (`105,380` input tokens):

| Rule | Precision | Recall | F1 | Exact 3-class | Pairs (`bug`+`safe`) | Unresolved | Located true findings | Safe/decoy FP |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `split_actor_invariant` | **100.0% (7/7)** | **100.0% (7/7)** | **100.0%** | **40/40 (100.0%)** | **6/6** | 1 (1 true `insufficient_context`) | 1/7 | **0** |
| `unbounded_collection_growth` | **100.0% (5/5)** | **100.0% (5/5)** | **100.0%** | **40/40 (100.0%)** | **4/4** | 1 (1 true `insufficient_context`) | 5/5 | **0** |
| **Overall (2-rule)** | **100.0% (12/12)** | **100.0% (12/12)** | **100.0%** | **80/80 (100.0%)** | **10/10** | **2** | **6/12** | **0** |

- `split_actor_invariant` flagged all 7 split-invariant bugs (`P(violation)` from `0.950` to `1.000`), produced **0/14 false positives** across the fresh single-primitive async + immutable-config files and **0/11** on the `unbounded_collection_growth` files, and returned `insufficient_context` only on `wf2_ho_split_insuff_external_stores`. (Six of seven true findings reported file scope rather than a line range because split invariants span struct fields and multiple methods.)
- `unbounded_collection_growth` remained untouched and scored 40/40 exact (5/5 TP, 5/5 located, 0 FP, 1/1 true `insufficient_context`).
- On Adamantium's own `src/` (36 files), `cargo-adamantium style check src --pack concurrency` exited `0` with **0 findings** and **0 unresolved**.

Reproduction through the built CLI:

```sh
cargo build --locked -p cargo-adamantium
node style/scripts/evaluate-decisions.mjs --dry-run --pack concurrency --split held_out
node style/scripts/evaluate-decisions.mjs --pack concurrency --cli target/debug/cargo-adamantium --split held_out --metrics
```
