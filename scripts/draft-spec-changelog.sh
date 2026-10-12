#!/usr/bin/env bash
# Draft a CHANGELOG `## Unreleased` entry for a re-vendored spec, so the
# bot/spec-refresh PR passes the changelog-entry gate (ci.yml) out of the box.
#
#   scripts/draft-spec-changelog.sh [<base-ref>]   # default: HEAD
#
# Compares operationIds in <base-ref>:spec/openapi.yaml against the working
# tree copy. Schema-only changes aren't detected — the PR reviewer rewords the
# entry to name them (see the refresh PR body in ci-scheduled.yml).
set -euo pipefail

base="${1:-HEAD}"
repo_root="$(git rev-parse --show-toplevel)"
changelog="$repo_root/CHANGELOG.md"

ops() { grep -oE 'operationId: *[A-Za-z0-9_]+' | sed 's/.*: *//' | sort -u; }
# shellcheck disable=SC2016  # literal backticks: Markdown code spans
join() { sed 's/.*/`&`/' | paste -sd, - | sed 's/,/, /g'; }

old="$(mktemp)"; new="$(mktemp)"; trap 'rm -f "$old" "$new"' EXIT
git show "$base:spec/openapi.yaml" | ops > "$old"
ops < "$repo_root/spec/openapi.yaml" > "$new"
added="$(comm -13 "$old" "$new" | join)"
removed="$(comm -23 "$old" "$new" | join)"

fetched="$(sed -n 's/^fetched_at[[:space:]]*=[[:space:]]*"\(.*\)"$/\1/p' "$repo_root/spec/openapi.provenance.toml")"
text="**Re-vendored the OpenAPI spec** (fetched $fetched)."
[[ -n "$added" ]] && text+=" Adds operations $added."
[[ -n "$removed" ]] && text+=" Removes operations $removed."
[[ -z "$added$removed" ]] && text+=" No operations added or removed."
entry="$(printf '%s\n' "$text" | fold -s -w 76 | sed 's/ *$//; 1s/^/- /; 2,$s/^/  /')"

grep -qx '## Unreleased' "$changelog" || {
  echo "::error::CHANGELOG.md has no '## Unreleased' heading"
  exit 1
}
ENTRY="$entry" awk '
  { print }
  !done && $0 == "## Unreleased" { print "\n### Changed\n\n" ENVIRON["ENTRY"]; done = 1 }
' "$changelog" > "$changelog.tmp"
mv "$changelog.tmp" "$changelog"
echo "drafted CHANGELOG entry: $text"
