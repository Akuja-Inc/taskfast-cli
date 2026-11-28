#!/usr/bin/env bash
set -euo pipefail

. "$(dirname "$0")/registry-lookup.sh"

crates="$(pending_crates)"

packages=()
for crate in ${crates}; do packages+=(-p "$crate"); done
cargo publish "${packages[@]}" --locked --dry-run
