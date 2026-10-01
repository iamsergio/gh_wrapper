#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
cargo build --release --locked
cp target/release/gh_wrapper ${PUB_DATA_DIR}/etc/bin/container/ghw
echo "Deployed to ${PUB_DATA_DIR}/etc/bin/container/ghw"
