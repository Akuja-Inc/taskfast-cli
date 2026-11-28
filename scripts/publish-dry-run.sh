#!/usr/bin/env bash
set -euo pipefail

. "$(dirname "$0")/registry-lookup.sh"

CRATES="$(release_crates)"

failed=0
for crate in ${CRATES}; do
  ver="$(cargo metadata --no-deps --format-version 1 | jq -er --arg n "$crate" '.packages[] | select(.name == $n) | .version')"
  case "$(published_state "$crate" "$ver")" in
    new) ;;
    published)
      echo "::error::${crate} ${ver} is already on crates.io; the release would skip it and ship the registry copy. Run the dry-run after the version bump."
      failed=1
      ;;
    *)
      echo "::error::crates.io index lookup for ${crate} failed; cannot tell whether ${ver} is published."
      failed=1
      ;;
  esac
done
[ "${failed}" = "0" ] || exit 1

packages=()
for crate in ${CRATES}; do packages+=(-p "$crate"); done
cargo publish "${packages[@]}" --locked --dry-run
