#!/usr/bin/env bash
set -euo pipefail

INDEX_URL="${CRATES_INDEX_URL:-https://index.crates.io}"
CRATES="taskfast-codegen taskfast-client taskfast-chains taskfast-agent taskfast-cli"

index_path() {
  local name="$1"
  case ${#name} in
    1) echo "1/${name}" ;;
    2) echo "2/${name}" ;;
    3) echo "3/${name}" ;;
    *) echo "${name:0:2}/${name:2:2}/${name}" ;;
  esac
}

published_state() {
  local name="$1" ver="$2" body code
  body="$(mktemp)"
  if ! code="$(curl -sS -o "$body" -w '%{http_code}' "${INDEX_URL}/$(index_path "$name")")"; then
    rm -f "$body"
    echo "error"
    return 0
  fi
  case "$code" in
    200) if grep -q "\"vers\":\"${ver}\"" "$body"; then echo "published"; else echo "new"; fi ;;
    404) echo "new" ;;
    *) echo "error" ;;
  esac
  rm -f "$body"
}

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

cargo publish --workspace --exclude xtask --locked --dry-run
