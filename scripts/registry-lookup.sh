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
  if ! code="$(curl -sS --max-time 20 -o "$body" -w '%{http_code}' "${INDEX_URL}/$(index_path "$name")")"; then
    rm -f "$body"
    echo "error"
    return 0
  fi
  case "$code" in
    200)
      if ! jq -es --arg n "$name" 'length > 0 and all(.[]; type == "object" and .name == $n and (.vers | type == "string" and length > 0))' "$body" > /dev/null 2>&1; then
        echo "error"
      elif jq -es --arg v "$ver" 'any(.[]; .vers == $v)' "$body" > /dev/null 2>&1; then
        echo "published"
      else
        echo "new"
      fi
      ;;
    404) echo "new" ;;
    *) echo "error" ;;
  esac
  rm -f "$body"
}


release_crates() {
  cargo metadata --no-deps --format-version 1 | jq -er '
    [.packages[] | select(.publish != [])] as $p
    | ($p | map(.name)) as $names
    | ($p | map({name, deps: ([.dependencies[] | select(.path != null and .kind != "dev") | .name] | map(select(. as $d | $names | index($d))))})) as $g
    | {done: [], rest: $g}
    | until(.rest | length == 0;
        . as $s
        | ($s.rest | map(select(all(.deps[]; . as $d | $s.done | index($d)))) | map(.name) | sort) as $ready
        | if ($ready | length) == 0 then error("dependency cycle among release crates") else
          {done: ($s.done + $ready), rest: ($s.rest | map(select(.name as $n | $ready | index($n) | not)))} end)
    | .done[]'
}

pkg_version() {
  cargo metadata --no-deps --format-version 1 | jq -er --arg n "$1" '.packages[] | select(.name == $n) | .version'
}

pending_crates() {
  local recover="${1:-false}" crate ver failed=0
  for crate in $(release_crates); do
    ver="$(pkg_version "$crate")"
    case "$(published_state "$crate" "$ver")" in
      new) echo "$crate" ;;
      published)
        if [ "$recover" = "true" ]; then
          echo "::notice::${crate} ${ver} already on crates.io; recovery mode skips it" >&2
        else
          echo "::error::${crate} ${ver} is already on crates.io; the release would skip it and ship the registry copy. Run the release after the version bump, or use recovery mode to finish a partial release." >&2
          failed=1
        fi
        ;;
      *)
        echo "::error::crates.io index lookup for ${crate} failed; cannot tell whether ${ver} is published." >&2
        failed=1
        ;;
    esac
  done
  return "$failed"
}
