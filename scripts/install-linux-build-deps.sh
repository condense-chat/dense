#!/usr/bin/env bash
set -euo pipefail

if [[ "$(dpkg --print-architecture)" == amd64 ]]; then
  for source in /etc/apt/sources.list /etc/apt/sources.list.d/ubuntu.sources; do
    [[ -f "$source" ]] || continue
    # Use the official archive directly instead of the runner's Azure mirror list.
    sudo sed -i \
      -e 's|mirror+file:/etc/apt/apt-mirrors\.txt|https://archive.ubuntu.com/ubuntu/|g' \
      -e 's|http://azure\.archive\.ubuntu\.com/ubuntu/|https://archive.ubuntu.com/ubuntu/|g' \
      "$source"
  done
fi

apt_options=(-o Acquire::Retries=3 -o Acquire::http::Timeout=30 -o Acquire::https::Timeout=30)
sudo apt-get "${apt_options[@]}" -o APT::Update::Error-Mode=any update
sudo apt-get "${apt_options[@]}" install -y musl-tools file
