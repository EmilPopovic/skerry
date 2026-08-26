#!/usr/bin/env bash
# Every arm and every pair, one at a time. Drags share a single pointer, so they
# cannot overlap — running two at once produces a matrix of coincidences.
set -u
cd "$(dirname "$0")"
mkdir -p runs
{
  echo "### $(date -Is)"
  timeout 240 ./target/release/spike-d-platform wayland
  echo
  timeout 240 ./target/release/spike-d-platform x11
  echo
  for pair in self nautilus-wayland konsole-wayland firefox-wayland designer-wayland \
              nautilus-x11 konsole-x11 firefox-x11 designer-x11 \
              nautilus-in-wayland nautilus-in-x11; do
    echo "########## $pair ##########"
    timeout 150 ./peers.sh "$pair"
    echo
  done
} 2>&1
