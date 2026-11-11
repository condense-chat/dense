#!/usr/bin/env bash
set -euo pipefail

if [[ "$(dpkg --print-architecture)" == amd64 ]]; then
  ubuntu_archive=https://archive.ubuntu.com/ubuntu/
  if ! timeout 10 getent ahosts archive.ubuntu.com >/dev/null; then
    ubuntu_archive=https://mirrors.edge.kernel.org/ubuntu/
  fi
  for source in /etc/apt/sources.list /etc/apt/sources.list.d/ubuntu.sources; do
    [[ -f "$source" ]] || continue
    # Both archives retain Ubuntu's signed metadata and the runner's keyring.
    sudo sed -E -i \
      -e "s|mirror\+file:/etc/apt/apt-mirrors\.txt|${ubuntu_archive}|g" \
      -e "s#https?://(azure\.archive|archive|security)\.ubuntu\.com/ubuntu/?#${ubuntu_archive}#g" \
      "$source"
  done
fi

apt_options=(-o Acquire::Retries=3 -o Acquire::http::Timeout=30 -o Acquire::https::Timeout=30)
sudo apt-get "${apt_options[@]}" -o APT::Update::Error-Mode=any update
sudo apt-get "${apt_options[@]}" install -y musl-tools file
