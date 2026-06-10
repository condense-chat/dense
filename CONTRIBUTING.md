# Contributing to dense

## Commit messages — Conventional Commits

Every commit follows [Conventional Commits](https://www.conventionalcommits.org):

```
<type>(<scope>): <subject>
```

`type` is one of `feat`, `fix`, `docs`, `chore`, `refactor`, `perf`, `test`,
`ci`. A `feat!`/`fix!` or a `BREAKING CHANGE:` footer marks a breaking change.
git-cliff parses these to build the changelog and compute the next version, so
the type is load-bearing — `feat` bumps the minor, `fix` the patch, a breaking
change the major.

Do **not** add `Co-Authored-By` (or any co-author trailer) to commits.

## Quality gates

Before pushing, all of these must pass (CI enforces them):

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings   # no unwrap / expect / panic / unsafe
cargo test
```

Fallible code returns `crate::Result` (the thiserror `Error` in `error.rs`,
with `.ctx(..)` for human context); `unwrap`/`expect`/`panic` are denied
crate-wide (tests may). Keep comments minimal — inline notes, real TODOs, or a short
clarification only; no narrative or historical paragraphs.

## Releasing

Releases are cut from `main` with [git-cliff](https://git-cliff.org),
through a PR (main is branch-protected):

1. Actions → **cut-release** → Run workflow (leave `version` empty to
   compute it from the commit log). It runs `scripts/release.sh --no-tag`
   on a `release/vX.Y.Z` branch and opens the release PR.
2. **Merge the release PR** — that review is the release authorization.
3. **tag-release** fires on the merge, tags the merged commit `vX.Y.Z`,
   and dispatches the build; the GitHub Release publishes automatically.

The cut also works locally on a branch:

```sh
./scripts/release.sh --no-tag           # version from the commit log
./scripts/release.sh --no-tag 1.4.0     # or pin it explicitly
```

then open the PR yourself — tagging still happens on merge.

The script bumps `Cargo.toml` (+ `Cargo.lock`), regenerates `CHANGELOG.md`,
commits `chore(release): vX.Y.Z`, and creates the `vX.Y.Z` tag. Review, then:

```sh
git push origin main --follow-tags
```

Pushing the tag triggers `.github/workflows/release.yml`: it builds linux
(musl, static), macOS, and Windows, then publishes `dense-<os>` +
`manifest-<os>.json` as a GitHub Release whose notes come from git-cliff.
The `cli.<zone>` host 302-redirects `dense self update` / install downloads to
those assets.
