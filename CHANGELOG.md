# Changelog

All notable changes to dense. Format follows Conventional Commits; versions
are computed by git-cliff.

## [0.6.0] - 2026-06-29

### Features
- Route OpenCode through condense via `dense opencode`- Dynamic model resolution + client-side Gemini signature replay- Implement MultiTool trait for OpenCode and enhance run function

### Refactor
- Ride the shared harness via MultiTool- Install thought_signature plugin globally, write-once- Express providers via Dialect types (MultiDialect)- Address review nits — purer apply, no silent drops- Tighten vestigial pub(crate) to private- Collapse Tool/MultiTool + Dialect/MultiDialect into one

## [0.5.0] - 2026-06-23

### Features
- Pin codex's Responses transport (WS default, CONDENSE_CODEX_WEBSOCKET=0 for HTTP)

## [0.4.0] - 2026-06-22

### Bug Fixes
- BYO upstream auth — let codex attach its own credential- Use collision-proof provider id `condense_cli`

### Features
- Route Codex through condense

## [0.3.3] - 2026-06-18

### Bug Fixes
- Pin auto-compact window to the full 1M- Force-enable Tool Search behind the proxy

## [0.3.2] - 2026-06-17

### Bug Fixes
- Assert first-party base url to keep 1M context window

## [0.3.1] - 2026-06-12

### Bug Fixes
- Keep the base URL path when building request URLs

## [0.3.0] - 2026-06-12

### Features
- XDG-style config and data dirs on macOS

## [0.2.0] - 2026-06-11

### Bug Fixes
- Keep every wizard path inside the cliclack frame- Don't suggest a shell restart when nothing was wired

### CI
- Conventional-commit check on PRs; dependabot prefixes; dist retention

### Features
- Fetch binaries and manifests from GitHub releases- Open-source read-the-code note at the top of setup- Cliclack-framed login and register flows

### Miscellaneous
- Bump actions/create-github-app-token from 2 to 3

### Performance
- Size-optimized release profile; dedupe the dep tree

## [0.1.1] - 2026-06-11

### CI
- Portable sha256 in the package step- Begin-release with the releaser app

## [0.1.0] - 2026-06-11

### CI
- Route the release cut through a PR- Dispatch required checks for bot-created release PRs

### Features
- Dense — the durable condense CLI

