# dense

**Save up to 50% on agentic coding usage.** [condense](https://condense.chat)
strips up to 70% of the tokens — prompts, context, and tool output — out of
every request before it reaches the AI provider. Same model, same results, a
fraction of the input tokens.

`dense` is the local CLI that makes it one command: an intercept that routes
your coding agent (Claude Code today) through the condense proxy, plus the auth
that keeps it signed in. Install once, log in once — no key swap, no per-run
`curl`.

## Install

```sh
curl -fsSL https://cli.condense.chat/unix | sh        # macOS / Linux
irm https://cli.condense.chat/nt | iex                # Windows (PowerShell)
```

The installer drops the `dense` binary on your PATH and hands off to
`dense setup`, which offers to route the bare `claude` command through dense —
so you keep typing `claude` and it just goes through condense.

Both install scripts are vendored in [`install/`](install/) so you can read
exactly what `curl | sh` runs; the server fills in the `{{ … }}` endpoint
placeholders when serving them.

## Commands

```
dense login                  authenticate this machine
dense claude <args>          run Claude Code through the proxy (args pass through)
dense persist [tools...]     shim the named tools (no args: all) so the bare
                             `claude` routes through dense; non-destructive
dense unpersist [tools...]   remove the shims
dense status                 current login + endpoint
dense doctor                 verify the install is wired correctly
dense setup                  first-run wizard (the installer hands off to this)
dense self update            update the binary in place
dense self uninstall         remove dense, its shims, and PATH wiring
```

`dense codex` is reserved — it prints a coming-soon notice for now.

## Zero data retention

The proxy is transparent and **ZDR (zero data retention)**: your conversations
are never stored. The database keeps only SHA-256 hashes — never prompt or
completion content — and compressed context lives in a cache that expires after
7 days. condense sees enough to compress a request in flight, and nothing
persists beyond that window.

## Build & verify

```sh
cargo build --release
# static, dependency-free linux binary:
cargo build --release --target x86_64-unknown-linux-musl
```

The crate forbids `unsafe` and denies `unwrap`/`expect`/`panic` via the
`[lints]` table; `cargo clippy -- -D warnings` enforces it. Because releases
are built from this repo's tagged source on GitHub Actions, you can read
exactly what a published binary contains and reproduce it from a tag.

Contributing and the release flow: see [CONTRIBUTING.md](CONTRIBUTING.md).
