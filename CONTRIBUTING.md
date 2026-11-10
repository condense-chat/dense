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

## Terminal output

Use `src/ui.rs` for human-facing output. Start with a title and the result,
then show labeled details and a useful next command. Use `OK` for completed
actions, `INFO` for neutral states, `CHECK` for diagnostic findings, and
`WARN` or `ERROR` when something needs attention. An already-completed action
should say that there is nothing to change.

Keep wording concrete: “log in,” “automatic routing,” and “background service.”
Explain engine, shim, and PATH details only where they help the user act.
Never claim routing is active when login or organization settings prevent it.

The shared renderer wraps prose, preserves commands inside backticks, and uses
color only on the destination terminal. `NO_COLOR` and `TERM=dumb` disable
color. Warnings, errors, login, and launch notices go to stderr; JSON and
child-process stdout stay free of decoration. Interactive prompts keep their
keyboard controls, with explicit defaults when there is no interactive terminal.

Use `ui::Report` for buffered summaries with the same headings, fields, and
wrapping. Default views should answer the user's question in plain language.
Keep account IDs and file paths behind `--details` when they are not needed for
the next action. `info` includes compact token split bars by default in both
terminals and pipes; `--details` adds exact counts and IDs. Emoji charts remain
available with `--bar` or `--matrix`. Use `ui::Report::bars` for proportional
charts that keep small categories visible and stack on narrow terminals. Missing usage
must say unavailable, and session totals awaiting pricing must say they are partial.

Use `ui::progress` around slow work and drop its guard before printing results.
The small animated bar clears on success or error, shows no invented percentage,
and stays hidden for pipes, JSON, and `TERM=dumb`.

To preview the CLI without Cargo's build messages, run from this repository:

```sh
cargo run --quiet -- claude-desktop status
```

Installed clients run `dense` directly and never see Cargo's build messages.

## Releasing

Releases are cut from `main` with [git-cliff](https://git-cliff.org),
through a PR (main is branch-protected):

1. Actions → **begin-release** → Run workflow (leave `version` empty to
   compute it from the commit log). It runs `scripts/release.sh --no-tag`
   on a `release/vX.Y.Z` branch and opens the release PR as the releaser
   app, so checks run on it normally.
2. **Merge the release PR** — that review is the release authorization.
3. **tag-release** fires on the merge and tags the merged commit `vX.Y.Z`
   (with the app token, so the tag event runs the release build); the
   GitHub Release publishes automatically.

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
