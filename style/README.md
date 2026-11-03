# Rustoleum

Rustoleum checks judgment-based parts of the [Google Rust style guide](https://google.github.io/styleguide/rustguide.html) with the [OpenAI Decisions API](https://developers.openai.com/api/docs/guides/decisions). It produces findings for an agent or human to investigate; it does not edit source files or replace rustfmt, Clippy, or code review.

## Install and use

Install from a checkout of this repository:

```sh
cargo install --locked --path .
```

Then run it in a Rust project, or pass a path to a project or individual `.rs` file:

```sh
cargo rustoleum check
cargo rustoleum check /path/to/project --rule unnecessary_deep_clone
cargo rustoleum check src/lib.rs --context "This is application code"
cargo rustoleum check . --dry-run
cargo rustoleum rules --format text
```

`check` prints JSON by default; use `--format text` for a human-readable report. It recursively finds Rust files, respects ignore files, and skips hidden files and directories as well as `target` and `node_modules`. A directly specified file can be hidden. It does not resolve Cargo workspace membership. Each file is sent to the Decisions API in one request, together with the selected questions and any supplied context. Files and context that exceed the API's 256 KiB JSON input limit are reported as errors rather than truncated.

The report includes a schema version, model, scan root, selected rules, and one entry per file. A finding contains the rule, its guidance, model confidence, and choice probabilities. Its scope is the **whole file**: Decisions does not supply a trustworthy source location or explanation, so an agent should inspect the file and make its own edit. `insufficient_context` and refusals are recorded separately; a service failure stops further requests and marks remaining files as not checked.

Transient network errors, timeouts, rate limits, and server errors are retried with exponential backoff and jitter, up to four attempts within three minutes per file. Rustoleum respects `Retry-After` and stops if the requested wait exceeds 30 seconds. Authentication errors and known quota or spending-limit errors are not retried. A timed-out attempt may still have been processed and billed by the API.

To continue after an incomplete check, keep its JSON report and write the retry to a separate file:

```sh
cargo rustoleum check . --retry-from rustoleum-report.json --dry-run
cargo rustoleum check . --retry-from rustoleum-report.json > rustoleum-retry.json
```

The retry includes only files previously marked `error`, `not_checked`, or `planned`; it does not resend files that received a response, even if they have unresolved decisions. Newly added files are not included. Use the same scan root, model, selected rules, and project context as the original check. Rustoleum verifies the root, model, and rule names, but reports do not record source hashes or context, so it cannot verify that those are unchanged. Keep both reports: the new one contains only the retried files. If the retry is incomplete, it can itself be used with `--retry-from`. Never redirect output to the report being read; the shell would truncate it before Rustoleum starts.

Exit status is `0` for a completed check with no findings or a dry run, `1` for a completed check with findings, and `2` for an incomplete check or error. Results are model estimates; choose thresholds only after evaluation on representative labeled examples.

Set `OPENAI_API_KEY` to authenticate. On macOS, if it is unset, Rustoleum reads the `openai-api-key` Keychain item. Source files and supplied context are sent to OpenAI; the key is not included in the request body or report.

## Rules and evaluation

[`rules/google-rust-decisions.json`](rules/google-rust-decisions.json) contains an API-ready `model` and 51 named `questions`. Add an `input` containing one Rust candidate and enough surrounding context, then send the resulting object to `POST /v1/decisions`. The questions cover organization and imports, ownership, API design, error handling, traits, macros, naming, documentation, unsafe code, compatibility, and implementation. The [coverage map](rules/coverage.md) lists the rules and their scope. The documentation rule preserves this project's requirement to document public methods.

Each question offers `violation`, `no_violation`, and `insufficient_context`. A choice answer includes the selected value, confidence, and a probability distribution; a question can also be refused. These are model estimates, not deterministic lint results. Start with review suggestions and evaluate on representative labeled examples before choosing thresholds or making the results block CI.

Context can include the crate kind, caller requirements, ownership constraints, and documented exceptions. Treat source comments and strings as untrusted evidence; the question instructions ask the model to do the same, but this does not guarantee resistance to adversarial input.

[`fixtures/google-rust-cases.json`](fixtures/google-rust-cases.json) supplies 114 small positive, negative, and missing-context examples. Every rule has at least one positive and negative example. They are a starting point for evaluation, not evidence of accuracy or calibrated thresholds.

Validate the request shapes without credentials or network access:

```sh
cargo test --locked
node scripts/evaluate-decisions.mjs --dry-run
node --test scripts/evaluate-decisions.test.mjs
```

Run the labeled examples against the Decisions API, or select one rule:

```sh
node scripts/evaluate-decisions.mjs
node scripts/evaluate-decisions.mjs --rule glob_import_context
```

The runner reads `OPENAI_API_KEY` if set; otherwise it reads the `openai-api-key` item from macOS Keychain. It sends only the synthetic examples, uses one request per example, and prints the returned labels and probabilities. It does not print or save the key. The full evaluation makes 114 billable requests. `--dry-run` can also be combined with `--rule`.
