#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
cargo build --release --locked

if [[ "$(uname -s)" == "Darwin" ]]; then
    platform=darwin
else
    platform=linux
fi

dest_dir="${PUB_DATA_DIR}/etc/bin/container/${platform}"
mkdir -p "${dest_dir}"
cp target/release/gh_wrapper "${dest_dir}/ghw"
echo "Deployed to ${dest_dir}/ghw"
