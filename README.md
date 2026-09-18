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
`dense setup`, which asks which of the supported tools (`claude`, `opencode`,
`codex`) to route through dense — so you keep typing `claude` and it just goes
through condense. Change the choice later with `dense persist <tool>` /
`dense unpersist <tool>`.

Both install scripts are vendored in [`install/`](install/) — real, runnable
scripts pointing at prod, byte-identical to what `https://cli.condense.chat`
serves. (Internal environments get the same script with the endpoints
rewritten to their own zone.)

## Commands

```
dense login                  authenticate this machine
dense claude <args>          run Claude Code through the proxy (args pass through)
dense codex <args>           run Codex through the proxy
dense opencode <args>        run OpenCode through the proxy
dense persist [tools...]     shim the named tools (no args: all) so the bare
                             `claude` routes through dense; non-destructive
dense unpersist [tools...]   remove the shims
dense info [--bar|--matrix|--json] [SESSION]
                             account + lifetime savings; with a session id
                             (or inside a dense-launched tool) also that
                             session's context sizes and spend.
                             --matrix is the coloured glyph grid (terminal
                             default), --bar the emoji bars (piped default)
dense usage [claude] [--json] [--attributed-only]
                             Claude subscription limits used, with condense
                             vs what the same traffic would have used without
                             Includes untagged Claude-harness usage as a labeled
                             estimate; --attributed-only excludes those requests.
dense status                 current login + endpoint
dense doctor                 verify the install is wired correctly
dense setup                  first-run wizard (the installer hands off to this)
dense self update            update the binary in place
dense self uninstall         remove dense, its shims, and PATH wiring
```

Inside a `dense`-launched tool, `/dense:info` runs `dense info` for the
current session and renders the result in place. It is scoped to that
process: Claude Code loads it from a plugin dir under dense's data dir,
OpenCode gets it inline in its config, and Codex gets a plugin dense stages
in its cache and enables for that one run. Codex releases old enough to still
read `$CODEX_HOME/prompts/` also get `/prompts:dense` from a file dense
writes there only when nothing else owns it. Everything dense wrote outside
its own data dir goes on `dense self uninstall`; your own commands, skills,
and plugins are never touched.

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
