# Install the `dense` CLI, then hand off to its first-run setup.
#
# Usage:
#   irm https://cli.condense.chat/nt | iex
#
# Honours CONDENSE_URL (override the proxy/api base the install targets).
# Runs via `iex` in the caller's session, so early exits are `return`, not
# `exit` (which would close their console).

$ErrorActionPreference = "Stop"

# Environment endpoints — the serving cli host rewrites these three
# assignments (and only these) for non-prod zones.
$CliUrl = "https://cli.condense.chat"
$ApiUrl = "https://api.condense.chat"
$AuthRequired = "1"

function Write-Arrow($msg) {
    Write-Host ">>> " -ForegroundColor Green -NoNewline
    Write-Host $msg
}

Write-Host "Welcome to condense.chat" -ForegroundColor Cyan
Write-Host "Claude Code through the condense proxy - install once, no key swap." -ForegroundColor DarkGray
Write-Host ""

$binDir = Join-Path $env:LOCALAPPDATA "dense\bin"
New-Item -ItemType Directory -Force -Path $binDir | Out-Null

# Version we'd install (from the manifest), and what's already installed.
$targetVersion = ""
try {
    $targetVersion = (Invoke-RestMethod -Uri "$CliUrl/windows-x86_64/dense/manifest.json").version
} catch {}

$updating = $false
$existing = (Get-Command dense -ErrorAction SilentlyContinue).Source
if ($existing) {
    $installedVersion = ""
    try { $installedVersion = ((& $existing --version) -split ' ')[-1] } catch {}
    if ($targetVersion -and ($installedVersion -eq $targetVersion)) {
        Write-Arrow "dense $targetVersion is already installed. Run ``dense -h`` for more info."
        return
    }
    $avail = ""
    if ($targetVersion) { $avail = "; $targetVersion available" }
    Write-Arrow "dense $installedVersion is installed$avail."
    $ans = "y"
    try { $ans = Read-Host "update? [Y/n]" } catch {}
    if ($ans -match '^[Nn]') {
        Write-Host "keeping dense $installedVersion."
        return
    }
    $updating = $true
}

# An update swaps the binary wherever it already lives; a fresh install
# lands in our bin dir.
$dest = if ($existing) { $existing } else { Join-Path $binDir "dense.exe" }
$url = "$CliUrl/windows-x86_64/dense/stable"
$tmp = "$dest.download"
Write-Arrow "downloading dense from $url"
try {
    Invoke-WebRequest -Uri $url -OutFile $tmp
    if (Test-Path $dest) {
        # A running dense.exe can't be overwritten, but it can be renamed.
        $old = "$dest.old"
        Remove-Item $old -ErrorAction SilentlyContinue
        Move-Item $dest $old
    }
    Move-Item $tmp $dest
} finally {
    Remove-Item $tmp -ErrorAction SilentlyContinue
}
Write-Arrow "installed dense to $dest"
Write-Host ""

# An update only swaps the binary — the existing PATH + shims stay as they are.
if ($updating) {
    $v = if ($targetVersion) { $targetVersion } else { "latest" }
    Write-Arrow "updated dense to $v. Run ``dense -h`` for more info."
    return
}

$env:Path = "$binDir;$env:Path"
if (-not $env:CONDENSE_URL) { $env:CONDENSE_URL = $ApiUrl }
$env:CONDENSE_AUTH_REQUIRED = $AuthRequired
& $dest setup
