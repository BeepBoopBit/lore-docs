# Publishing

GitHub Actions checks changes and publishes `lore-docs` when you push a version
tag. Users then install it with `cargo install lore-docs --locked` and run `lore`.

## One-time setup

In the GitHub repository, open **Settings → Secrets and variables → Actions →
New repository secret**. Save your crates.io publishing token as
`CARGO_REGISTRY_TOKEN`. Your crates.io email must be verified.

Commit and push the project, including `.github/workflows/release.yml` and
`Cargo.lock`. The workflow runs on pushes to `main`, pull requests, and version
tags. You can also run **CI and publish** manually from the Actions tab to check
the project without publishing.

## Release

For the first release, keep version `0.1.0`. For later releases, update `version`
in `Cargo.toml` and run `cargo check` to update `Cargo.lock`. Commit and push those
changes before tagging:

```sh
git tag v0.1.0
git push origin v0.1.0
```

The tag must exactly match `v` followed by the package version. The workflow:

1. Checks the tag, formatting, Clippy, Rust tests, and installer tests.
2. Builds the executable on Rust 1.88 and checks its name and version.
3. Lists the packaged files and runs `cargo publish --dry-run --locked`.
4. Publishes to crates.io only after both Rust jobs pass.

Follow the run in the repository's **Actions** tab. Branch pushes, pull requests,
and manual runs never publish. For setup or network failures, fix the issue and
rerun the failed jobs. A rerun uses the same tagged commit; source fixes need a
new tag. Published versions cannot be overwritten.

The GitHub repository can remain private, but the packaged source on crates.io
is public. Repository and guide links become publicly accessible when you make
GitHub public. Local indexes, models, and evaluations are excluded from the crate.

See [GitHub's secret setup guide](https://docs.github.com/en/actions/how-tos/write-workflows/choose-what-workflows-do/use-secrets)
and [Cargo's publishing guide](https://doc.rust-lang.org/cargo/reference/publishing.html).
