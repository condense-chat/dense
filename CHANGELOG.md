# Changelog

All notable changes to dense. Format follows Conventional Commits; versions
are computed by git-cliff.

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

