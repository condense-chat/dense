#!/usr/bin/env bash
# Cut a release: bump Cargo.toml, regenerate CHANGELOG.md, commit, and tag.
# Version comes from the argument, else git-cliff computes it from the commit
# log. Pushing the resulting tag triggers .github/workflows/release.yml.
#
#   ./scripts/release.sh            # next version from Conventional Commits
#   ./scripts/release.sh 1.4.0      # pin it explicitly
#   ./scripts/release.sh --no-tag   # commit only — CI tags after the release
#                                   # PR merges (branch-protection flow)
set -euo pipefail

no_tag=
if [ "${1:-}" = "--no-tag" ]; then
  no_tag=1
  shift
fi

cd "$(dirname "$0")/.."

if ! command -v git-cliff >/dev/null 2>&1; then
  echo "git-cliff is required (https://git-cliff.org)" >&2
  exit 1
fi
if [ -n "$(git status --porcelain)" ]; then
  echo "working tree is dirty — commit or stash first" >&2
  exit 1
fi

raw="${1:-$(git-cliff --bumped-version)}"
version="${raw#v}"
tag="v${version}"

if git rev-parse "$tag" >/dev/null 2>&1; then
  echo "tag ${tag} already exists" >&2
  exit 1
fi

echo ">> releasing ${tag}"
sed -i.bak -E "0,/^version = \".*\"/s//version = \"${version}\"/" Cargo.toml
rm -f Cargo.toml.bak
cargo update -p dense >/dev/null 2>&1 || true
git-cliff --tag "$tag" -o CHANGELOG.md

git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -m "chore(release): ${tag}"

if [ -n "$no_tag" ]; then
  echo ">> done (no tag). merge the release PR; CI tags the merged commit."
  exit 0
fi

git tag -a "$tag" -m "${tag}"

echo ">> done. review, then: git push origin main --follow-tags"
