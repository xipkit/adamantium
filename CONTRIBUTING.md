# Contributing

## Development

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
cargo build --locked -p adamantium --no-default-features
cargo clippy --locked -p adamantium --all-targets --no-default-features -- -D warnings
cargo test --locked -p adamantium --no-default-features
node --test style/scripts/evaluate-decisions.test.mjs
```

[GitHub Actions](.github/workflows/ci.yml) runs the same commands on pull requests and pushes to `main`.

The repository follows its own rules. Where the library needs restricted features, such as the `JoinSet` inside `Tasks` and the `Mutex` inside `Actor`, the code uses an `allow` with a reason. The CLI tests run `cargo adamantium check` against fixture crates with and without rule violations. They require Clippy and network access on the first run.

## Releasing

Both crates use the MIT license and share the version in `[workspace.package]`.
Pull requests run `cargo publish --dry-run` for each crate to verify that its
archive builds independently of this checkout. To inspect the archives locally:

```sh
cargo package --list -p adamantium
cargo package --list -p cargo-adamantium
cargo publish --locked --registry crates-io -p adamantium --all-features --dry-run
cargo publish --locked --registry crates-io -p cargo-adamantium --dry-run
```

Pushing a tag named `v` followed by the workspace version, such as `v0.1.0`,
starts the [publish workflow](.github/workflows/publish.yml). It checks the tag
against both crate versions, requires its commit to have been merged into
`main`, runs the Rust tests and Clippy, and verifies both archives before
publishing. Verification and publishing both check out the triggering commit
SHA, so moving the tag cannot change the source between jobs. Each crate
publishes in its own job so a failed upload can be retried without uploading
the other crate again.

The publish jobs use crates.io [Trusted Publishing](https://crates.io/docs/trusted-publishing):
each job exchanges a GitHub OIDC identity for a crates.io token that lasts for
that job, so the repository stores no registry credential. Both crates trust
only `publish.yml` running in the `crates-io` environment, which makes the
environment the release approval boundary. A repository administrator must
configure its protections in GitHub:

- Protect `main` so release commits enter it through reviewed pull requests
  with passing checks.
- Create an active tag ruleset for `v*` that restricts tag creation, updates and
  deletion to authorized release maintainers.
- Create the `crates-io` environment with authorized release maintainers as
  required reviewers, disable protection-rule bypasses, and restrict deployment
  refs to selected **tags** matching `v*`.
- On crates.io, give each crate a Trusted Publisher for this repository with
  workflow `publish.yml` and environment `crates-io`. Omitting the environment
  would let any run of the workflow publish without approval.
- Don't store a `CARGO_REGISTRY_TOKEN` secret. A repository or organization
  secret is available to other workflows and would bypass the release approval
  boundary.

An environment reviewer must check that the pending job's commit SHA is the
approved release commit before allowing either publish job to run. A job
waiting for approval has no OIDC identity and so no crates.io token.
Referencing an environment in YAML does not configure these protections: GitHub
automatically creates a missing environment without reviewers. See GitHub's
[environment setup](https://docs.github.com/en/actions/how-tos/deploy/configure-and-manage-deployments/manage-environments)
and [tag rules](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-rulesets/available-rules-for-rulesets).

Version 0.1.0 of both crates was published by hand to create them, because
Trusted Publishing only works for crates that already exist. It has no tag:
bump the workspace version before pushing the first release tag. Prepare and
review the release before pushing its tag: published crate versions cannot be
overwritten or deleted.
