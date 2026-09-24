#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
cargo build --release --locked
cp target/release/gh_wrapper /pub_data/etc/bin/container/ghw
echo "Deployed to /pub_data/etc/bin/container/ghw"
