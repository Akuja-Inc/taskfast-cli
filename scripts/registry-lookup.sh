#!/usr/bin/env bash
INDEX_URL="${CRATES_INDEX_URL:-https://index.crates.io}"
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

