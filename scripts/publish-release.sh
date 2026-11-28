#!/usr/bin/env bash
set -euo pipefail

. "$(dirname "$0")/registry-lookup.sh"

: "${CARGO_REGISTRY_TOKEN:?CARGO_REGISTRY_TOKEN is not set; add the crates.io publish token to the repo secrets}"

cli_version="$(pkg_version taskfast-cli)"
case "${GITHUB_REF:-}" in
  "refs/tags/taskfast-cli-v${cli_version}"|"refs/tags/v${cli_version}") ;;
  *)
    echo "::error::a real upload runs only from the tag taskfast-cli-v${cli_version} (or v${cli_version}), not ${GITHUB_REF:-unset}."
    exit 1
    ;;
esac

crates="$(pending_crates "${RECOVER:-false}")"

for crate in ${crates}; do
  ver="$(pkg_version "$crate")"
  echo "::group::cargo publish -p ${crate}"
  cargo publish -p "$crate" --locked
  echo "::endgroup::"
  visible=false
  for _ in $(seq "${PROPAGATION_TRIES:-10}"); do
    if [ "$(published_state "$crate" "$ver")" = "published" ]; then visible=true; break; fi
    sleep "${PROPAGATION_SLEEP:-3}"
  done
  [ "$visible" = "true" ] || echo "::warning::${crate} ${ver} uploaded but not yet visible on the sparse index; the next crate may fail to resolve it. Re-run in recovery mode if so."
done
