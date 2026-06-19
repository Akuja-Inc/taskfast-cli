#!/usr/bin/env bash
# Guard that every tracked Rust source file carries an SPDX license header.
#
#   scripts/check-spdx.sh         # verify: exit 1 if any .rs file lacks the header
#   scripts/check-spdx.sh --fix   # insert the header into any file missing it
#
# The workspace is MIT-licensed (see LICENSE). A per-file SPDX identifier lets
# downstream license scanners (REUSE, ScanCode, FOSSA) auto-detect licensing
# per file instead of inferring it from the repo-level LICENSE.
#
# Exit 0 = all headers present (or fixed), 1 = missing headers, 2 = setup error.
set -euo pipefail

header='// SPDX-License-Identifier: MIT'

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

mode="check"
case "${1:-}" in
  "") ;;
  --fix) mode="fix" ;;
  *) echo "::error::usage: scripts/check-spdx.sh [--fix]"; exit 2 ;;
esac

mapfile -t files < <(git ls-files '*.rs')
[[ ${#files[@]} -gt 0 ]] || { echo "::error::no tracked .rs files found"; exit 2; }

missing=()
for f in "${files[@]}"; do
  # The SPDX identifier must be the first line of the file.
  [[ "$(head -n1 "$f")" == "$header" ]] || missing+=("$f")
done

if [[ ${#missing[@]} -eq 0 ]]; then
  echo "ok: SPDX header present in all ${#files[@]} tracked .rs files"
  exit 0
fi

if [[ "$mode" == "fix" ]]; then
  for f in "${missing[@]}"; do
    tmp="$(mktemp)"
    printf '%s\n' "$header" >"$tmp"
    cat "$f" >>"$tmp"
    mv "$tmp" "$f"
    echo "fixed: $f"
  done
  echo "added SPDX header to ${#missing[@]} file(s)"
  exit 0
fi

echo "::error::missing SPDX header (\"$header\") in ${#missing[@]} of ${#files[@]} tracked .rs files:"
printf '  %s\n' "${missing[@]}"
echo "Run: scripts/check-spdx.sh --fix"
exit 1
