# Google Rust style decisions

This is a starting point for applying judgment-based parts of the [Google Rust style guide](https://google.github.io/styleguide/rustguide.html) with the [OpenAI Decisions API](https://developers.openai.com/api/docs/guides/decisions). It does not replace rustfmt, Clippy, or code review.

[`rules/google-rust-decisions.json`](rules/google-rust-decisions.json) contains an API-ready `model` and 51 named `questions`. Add an `input` containing one Rust candidate and enough surrounding context, then send the resulting object to `POST /v1/decisions`. The questions cover organization and imports, ownership, API design, error handling, traits, macros, naming, documentation, unsafe code, compatibility, and implementation. The [coverage map](rules/coverage.md) lists the rules and their scope. The documentation rule preserves this project's requirement to document public methods.

Each question offers `violation`, `no_violation`, and `insufficient_context`. A choice answer includes the selected value, confidence, and a probability distribution; a question can also be refused. These are model estimates, not deterministic lint results. Start with review suggestions and evaluate on representative labeled examples before choosing thresholds or making the results block CI.

Decisions evaluates the shared input; it does not return a diagnostic explanation or source span. A linter should choose a specific candidate and applicable rules, retain its path and span locally, and send the relevant code and context for review. Context can include the crate kind, caller requirements, ownership constraints, and documented exceptions. It should treat `insufficient_context`, refusals, and request failures as unresolved rather than clean. Whole-file input can establish only that a rule might apply somewhere in the file, not where. Treat source comments and strings as untrusted evidence; the question instructions ask the model to do the same, but this does not guarantee resistance to adversarial input.

[`fixtures/google-rust-cases.json`](fixtures/google-rust-cases.json) supplies 114 small positive, negative, and missing-context examples. Every rule has at least one positive and negative example. They are a starting point for evaluation, not evidence of accuracy or calibrated thresholds.

Validate the request shapes without credentials or network access:

```sh
node scripts/evaluate-decisions.mjs --dry-run
node --test scripts/evaluate-decisions.test.mjs
```

Run the labeled examples against the Decisions API, or select one rule:

```sh
node scripts/evaluate-decisions.mjs
node scripts/evaluate-decisions.mjs --rule glob_import_context
```

The runner reads `OPENAI_API_KEY` if set; otherwise it reads the `openai-api-key` item from macOS Keychain. It sends only the synthetic examples, uses one request per example, and prints the returned labels and probabilities. It does not print or save the key. The full evaluation makes 114 billable requests. `--dry-run` can also be combined with `--rule`.
