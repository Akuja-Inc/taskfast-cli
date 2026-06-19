#!/usr/bin/env bash
# Guard that spec/openapi.yaml has not drifted from its recorded provenance.
#
#   scripts/check-spec-drift.sh             # offline: hash the vendored
#                                           #   spec/openapi.yaml against the
#                                           #   sha256 in provenance.toml.
#                                           #   Catches in-repo hand-edits.
#   scripts/check-spec-drift.sh --upstream  # online: fetch the live server
#                                           #   spec and compare. Catches the
#                                           #   server moving ahead of the
#                                           #   vendored copy.
#
# Exit 0 = in sync, 1 = drift (with remediation hint), 2 = usage/setup error.
set -euo pipefail

mode="local"
[[ "${1:-}" == "--upstream" ]] && mode="upstream"

repo_root="$(git rev-parse --show-toplevel)"
spec_file="$repo_root/spec/openapi.yaml"
prov_file="$repo_root/spec/openapi.provenance.toml"

[[ -f "$prov_file" ]] || {
  echo "::error::missing $prov_file — run scripts/vendor-spec.sh"
  exit 2
}

read_toml() { sed -n "s/^$1[[:space:]]*=[[:space:]]*\"\(.*\)\"\$/\1/p" "$prov_file"; }
recorded_sha="$(read_toml sha256)"
spec_url="$(read_toml source)"
[[ -n "$recorded_sha" ]] || { echo "::error::no sha256 in $prov_file"; exit 2; }
[[ -n "$spec_url" ]] || { echo "::error::no source in $prov_file"; exit 2; }

if [[ "$mode" == "upstream" ]]; then
  tmp="$(mktemp)"; trap 'rm -f "$tmp"' EXIT
  echo "fetching $spec_url ..."
  curl -fsSL --max-time 60 -o "$tmp" "$spec_url"
  actual_sha="$(sha256sum "$tmp" | cut -d' ' -f1)"
  subject="server spec at $spec_url"
else
  actual_sha="$(sha256sum "$spec_file" | cut -d' ' -f1)"
  subject="vendored spec/openapi.yaml"
fi

if [[ "$actual_sha" == "$recorded_sha" ]]; then
  echo "ok: $subject matches recorded provenance (${recorded_sha:0:16}…)"
  exit 0
fi

echo "::error::SPEC DRIFT — $subject (${actual_sha:0:16}…) != provenance (${recorded_sha:0:16}…)"
if [[ "$mode" == "upstream" ]]; then
  echo "The server's canonical spec has moved ahead of the vendored copy."
  echo "Re-vendor: scripts/vendor-spec.sh && cargo xtask sync-spec && cargo build --workspace"
else
  echo "spec/openapi.yaml was edited without updating its provenance."
  echo "Re-vendor instead of hand-editing: scripts/vendor-spec.sh"
fi
exit 1
