# Project Rules — dense

This is the standalone `dense` CLI repo (a Rust binary). It is vendored into the
condense monorepo as a git submodule, but versions and ships on its own.

- **No co-author trailers.** NEVER add `Co-Authored-By:` (or any co-author
  trailer) to commits. No exceptions, regardless of tooling.
- **Conventional Commits are required.** Every commit is
  `<type>(<scope>): <subject>` (`feat`/`fix`/`docs`/`chore`/`refactor`/`perf`/
  `test`/`ci`; `!` or a `BREAKING CHANGE:` footer for breaking). git-cliff parses
  these to generate `CHANGELOG.md` and compute the next `vX.Y.Z` — a wrong type
  produces a wrong release. See CONTRIBUTING.md.
- **No panics.** `unsafe` is forbidden; `unwrap`/`expect`/`panic`/
  `unwrap_in_result` are denied crate-wide. Fallible code returns `crate::Result`
  — the thiserror `Error` in `error.rs`, with the `.ctx(..)` extension for human
  context (the lib's `wrap_err`); `main` renders it via color-eyre.
  `cargo clippy --all-targets -- -D warnings` must pass.
- **No `mod.rs`.** Module files are self-named (`foo.rs` + `foo/`), never
  `mod.rs` (`mod_module_files` is denied).
- **Item ordering — declarations, then definitions, then data, then behaviour.**
  Within a file, items go in kind order: `mod` declarations → `use` → `const` →
  `type` → `trait` → `enum`/`struct` → `impl` → `fn`. clippy enforces this
  (`arbitrary_source_item_ordering` + `clippy.toml`). Within each kind group,
  order by visibility then name: `pub`, then `pub(crate)`, then private, each
  alphabetical — public interface on top, easy to scan. (Visibility ordering and
  the alpha sort aren't machine-checkable here, so they're convention.) An
  `impl` ideally trails its type. Inline modules (`#[cfg(test)] mod tests { … }`)
  stay at the bottom — clippy only orders outline `mod foo;` declarations.
- **Comment discipline.** Comments only when critical: an inline note, a real
  TODO, or a short clarification for non-obvious code. No file-header doc blocks,
  no narrative paragraphs, no "previously X, now Y" — the diff and commit message
  hold that.
- **Before committing:** `cargo fmt --check`, `cargo clippy --all-targets -D
  warnings`, `cargo test` all green.
- **Releasing.** The cut-release GitHub Action (or `./scripts/release.sh`
  locally — same script) computes the next `vX.Y.Z` from the commit log
  (git-cliff), bumps `Cargo.toml`, regenerates `CHANGELOG.md`, commits, and
  tags; the tag (pushed, or dispatched by the action) builds the matrix and
  publishes the GitHub Release. Full procedure in CONTRIBUTING.md — not in
  the user-facing README.

## Layout

Single binary crate; `src/main.rs` just parses clap and dispatches. The work is
grouped by concern:

- `api/` — the condense HTTP client. `Api` (in `api.rs`) is the one place
  requests are built: shared pool, base URL, default `x-condense-*` headers,
  error context. Submodules: `auth` (device-flow/register), `session`
  (heartbeat), `profile` (fetch a descriptor), `dialect` (the proxy wire
  formats; `Anthropic` is the first).
- `harness.rs` (+ `harness/claude.rs`) — the interceptor: a generic
  `launch(cfg, tool, dialect, args)` over `Tool<D: Dialect>`; `Claude` is the
  first tool. Config is the single reader of condense env vars; the one
  exception is a tool's own passthrough env (e.g. ANTHROPIC_CUSTOM_HEADERS),
  which its `Tool` impl owns.
- `config`/`profile`/`hosts` — resolved paths + the environment-profile model +
  sibling-host resolution.
- `persist`/`env_file`/`tool` — non-destructive PATH shims and the env wiring.
- `error` — the `Error`/`Result`/`Context` types.
- `cli`/`doctor`/`setup`/`ui`/`selfupdate` — the user-facing surface.

Profiles are an internal mechanism, deliberately left out of the user-facing
README — `prod` is baked in; other environments are fetched from a cli host's
`/profile` endpoint and cached under `~/.config/dense/<name>/`, with the
`target` pointer selecting the active one. The end-to-end story lives in the
condense monorepo; here, read the source.
