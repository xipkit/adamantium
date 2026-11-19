# Style review

`cargo adamantium style check` reviews Rust files against the judgment-based parts of the [Google Rust style guide](https://google.github.io/styleguide/rustguide.html). It uses the [OpenAI Decisions API](https://developers.openai.com/api/docs/guides/decisions).

The command returns findings for review. It does not edit source. It does not replace rustfmt, Clippy, or code review.

`--pack concurrency` reviews against concurrency rules instead. See [The concurrency pack](#the-concurrency-pack).

```sh
cargo adamantium style check                      # every .rs file under the current directory
cargo adamantium style check src/lib.rs --context "application code"
cargo adamantium style check . --rule unnecessary_deep_clone --dry-run
cargo adamantium style check --changed=main --cache-from last.json > next.json
cargo adamantium style check src --pack concurrency --format text
cargo adamantium style rules --format text
cargo adamantium style rules --pack concurrency --format text
```

## Setup

Set `OPENAI_API_KEY`. On macOS, the command uses the `openai-api-key` Keychain item when the variable is not set.

## Privacy and accuracy

- The command sends the source files and the context to OpenAI.
- The results are model estimates. Evaluate them on labeled examples before you use them as a blocking check.
- The questions tell the model to treat source comments and strings as untrusted evidence. This does not guarantee resistance to adversarial input.

## Which files it reviews

- The command finds Rust files recursively.
- It respects ignore files. It skips hidden directories, `target`, and `node_modules`.
- It does not resolve Cargo workspace membership.

## Requests

- The command sends each file to the provider in one request, with the selected rules and any context.
- `--provider` selects the service that answers the rules. `decisions`, the OpenAI Decisions API, is the default and currently the only provider.
- The file, the path, and the context must fit within the 256 KiB JSON input limit of the API.
- Each request is billable.

For each file with findings, two more requests ask Decisions for the location. Decisions answers only choices, so the first request selects a window of lines. The second selects the first and last line within that window.

## Output

The command prints JSON by default. `--format text` prints a readable report.

- A report records the provider and the pack.
- A finding's `guidance` says what goes wrong in code that breaks the rule.
- A finding that was located with confidence has `lines` with `start_line` and `end_line`, numbered from 1.
- A finding without `lines` concerns the whole file, or Decisions could not locate it.
- The `input_tokens` of a file includes the location requests. When the location step fails, the file has no `input_tokens`, because the earlier requests might have been billed.

### Exit status

| Status | Meaning |
| --- | --- |
| `0` | A clean check, or a dry run |
| `1` | Findings |
| `2` | An incomplete check |

## Failures and retries

- The command retries transient failures with exponential backoff and jitter. It makes at most four attempts within three minutes for each file.
- It obeys `Retry-After` up to 30 seconds.
- It does not retry authentication, quota, or spending-limit errors.
- A service failure stops further requests. The remaining files are marked as unchecked.

`--retry-from previous.json` retries the failed and unchecked files in a new report.

- Supply the same scan root and the same `--rule` options. Pass `--context` again if you used it.
- `--retry-from` refuses a report from a different provider or pack.
- Do not redirect the output to the report that the command reads.

## Review fewer files

Two options limit a run to the files that need a review.

### `--changed`

- `--changed` reviews only the Rust files that differ from `HEAD`: staged, unstaged, and untracked files that are not ignored.
- `--changed=BASE` adds the files committed since the current branch left `BASE`. An example is `--changed=main`.
- Deleted files are left out.

### `--cache-from`

`--cache-from previous.json` reuses the results of a previous report. It reuses the result for a file when the source has the same SHA-256 and the answer came from the same provider, pack, model, rule definitions, and context.

- Each report records `source_sha256` for each file, and `rules_sha256` and `context_sha256` for the run. A change to the wording of a rule changes `rules_sha256`.
- A reused file keeps its findings. It is marked `"cached": true` and has no `input_tokens`.
- A report lists reused files together with reviewed files. It can thus be the cache for the next run.
- Files that failed, or that were never sent, are reviewed again.
- The cached report must have schema version 4 and the same scan root.

`--retry-from` cannot be combined with `--changed` or `--cache-from`.

## The rules

- The rules are in [`cli/src/style/google-rust-decisions.json`](../cli/src/style/google-rust-decisions.json).
- Their coverage of the guide is in [`style/rules/coverage.md`](../style/rules/coverage.md).
- The labeled examples are in [`style/fixtures/google-rust-cases.json`](../style/fixtures/google-rust-cases.json).

To evaluate the examples against the API, run `node style/scripts/evaluate-decisions.mjs`. It also accepts `--dry-run` and `--rule`. The full run makes 114 billable requests.

## The concurrency pack

`--pack concurrency` asks about concurrency bugs that Clippy and the Adamantium lints can't see. It currently has one rule, `split_actor_invariant`. The rule finds state that must satisfy a joint invariant but is kept in separate Actors, locks, or atomics and updated in separate critical sections. An example is a balance and its ledger.

- The rule is in [`cli/src/style/concurrency-decisions.json`](../cli/src/style/concurrency-decisions.json). A test keeps it identical to the evaluated wording in [`style/rules/concurrency-decisions.json`](../style/rules/concurrency-decisions.json).
- The evaluation is in [`style/rules/concurrency-evaluation.md`](../style/rules/concurrency-evaluation.md). That file also covers three more rules, which the pack leaves out. Run through this command on whole files without per-case context, `stale_read_across_await` and `unbounded_collection_growth` flagged correct code. `partial_mutation_on_cancellation` had false positives even in the evaluation. These rules return to the pack only after they pass an evaluation that runs through this command.
- On the 88 held-out cases, the shipped rule found 9 of 10 bugs and flagged no correct code.

The pack is not a blocking check yet. The rule answers `insufficient_context` when a file has two shared states but doesn't show how they relate. It did so for 4 of the 88 cases. Any unresolved answer makes the run exit 2, even when the run has findings. Read the findings and the unresolved rules, and don't fail a build on the exit status.
