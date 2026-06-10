# Install the `dense` CLI, then hand off to its first-run setup.
#
# Usage:
#   irm {{ cli_url }}/nt | iex

$ErrorActionPreference = "Stop"

$CliUrl = "{{ cli_url }}"
$ApiUrl = "{{ api_url }}"

$binDir = Join-Path $env:LOCALAPPDATA "dense\bin"
New-Item -ItemType Directory -Force -Path $binDir | Out-Null
$exe = Join-Path $binDir "dense.exe"

$url = "$CliUrl/windows-x86_64/dense/stable"
Write-Host ">>> downloading dense from $url"
Invoke-WebRequest -Uri $url -OutFile $exe

$env:Path = "$binDir;$env:Path"
if (-not $env:CONDENSE_URL) { $env:CONDENSE_URL = $ApiUrl }
$env:CONDENSE_AUTH_REQUIRED = "{{ 1 if auth_required else 0 }}"
& $exe setup
