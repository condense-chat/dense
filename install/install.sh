#!/usr/bin/env bash
# Install the `dense` CLI, then hand off to its first-run setup.
#
# Usage:
#   curl -fsSL {{ cli_url }}/unix | sh
#
# Honours CONDENSE_URL (override the proxy/api base the install targets).

set -euo pipefail

CLI_URL="{{ cli_url }}"
API_URL="{{ api_url }}"

# Colours, only when stderr is a terminal.
if [ -t 2 ]; then
  B=$'\033[1m'; DIM=$'\033[2m'; CYAN=$'\033[1;36m'; GREEN=$'\033[32m'; R=$'\033[0m'
else
  B=; DIM=; CYAN=; GREEN=; R=
fi
arrow="${GREEN}>>>${R}"

printf '%s\n\n' "${CYAN}Welcome to condense.chat${R}" >&2
printf '%s\n'   "${DIM}Claude Code through the condense proxy — install once, no key swap.${R}" >&2
printf '\n' >&2

case "$(uname -s)" in
  Linux)  os=linux ;;
  Darwin) os=macos ;;
  *) echo "unsupported OS: $(uname -s)" >&2; exit 1 ;;
esac

case "$(uname -m)" in
  x86_64|amd64)  arch=x86_64 ;;
  aarch64|arm64) arch=aarch64 ;;
  *)
    echo "dense ships ${os} binaries for x86_64 and aarch64; detected $(uname -m)." >&2
    echo "build from source: ${CLI_URL}" >&2
    exit 1
    ;;
esac
platform="${os}-${arch}"

bindir="${XDG_BIN_HOME:-$HOME/.local/bin}"
mkdir -p "$bindir"

# Version we'd install (from the manifest), and what's already installed.
target_version=""
if manifest="$(curl -fsSL "${CLI_URL}/${platform}/dense/manifest.json" 2>/dev/null)"; then
  target_version="$(printf '%s' "$manifest" | sed -n 's/.*"version":"\([^"]*\)".*/\1/p')"
fi

updating=
existing="$(command -v dense 2>/dev/null || true)"
if [ -n "$existing" ]; then
  installed_version="$("$existing" --version 2>/dev/null | awk '{print $NF}')"
  if [ -n "$target_version" ] && [ "$installed_version" = "$target_version" ]; then
    printf '%s %s\n' "$arrow" "${GREEN}dense ${target_version} is already installed.${R} Run ${B}dense -h${R} for more info." >&2
    exit 0
  fi
  avail=""
  [ -n "$target_version" ] && avail="; ${target_version} available"
  printf '%s %s\n' "$arrow" "dense ${installed_version} is installed${avail}." >&2
  ans=y
  if [ -r /dev/tty ]; then
    printf 'update? [Y/n] ' >&2
    read -r ans < /dev/tty || ans=y
  fi
  case "${ans:-y}" in
    [Nn]*) printf '%s\n' "keeping dense ${installed_version}." >&2; exit 0 ;;
  esac
  updating=1
fi

url="${CLI_URL}/${platform}/dense/stable"
tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT
printf '%s %s\n' "$arrow" "downloading dense from ${DIM}${url}${R}" >&2
curl -fsSL "$url" -o "$tmp"
chmod +x "$tmp"
dest="${existing:-${bindir}/dense}"
mv "$tmp" "$dest"
printf '%s %s\n\n' "$arrow" "installed dense to ${B}${dest}${R}" >&2

# An update only swaps the binary — the existing PATH + shims stay as they are.
if [ -n "$updating" ]; then
  printf '%s %s\n' "$arrow" "updated dense to ${B}${target_version:-latest}${R}. Run ${B}dense -h${R} for more info." >&2
  exit 0
fi

export PATH="${bindir}:${PATH}"
export CONDENSE_URL="${CONDENSE_URL:-$API_URL}"
export CONDENSE_AUTH_REQUIRED="{{ 1 if auth_required else 0 }}"

# `curl … | sh` leaves stdin attached to the pipe, not the terminal, so the
# setup wizard can't prompt. Reconnect stdin to the controlling tty when we
# have one (this is what rustup does); without a tty, setup uses defaults.
if [ -r /dev/tty ]; then
  exec "${bindir}/dense" setup < /dev/tty
else
  exec "${bindir}/dense" setup
fi
