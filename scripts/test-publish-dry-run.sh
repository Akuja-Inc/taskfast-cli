#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d)"
server=""
trap '[ -z "$server" ] || kill "$server" 2>/dev/null || true; rm -rf "$work"' EXIT

mkdir -p "$work/bin" "$work/index/ta/sk"
cat > "$work/bin/cargo" <<'CARGO'
#!/usr/bin/env bash
if [ "$1" = "metadata" ]; then
  jq -n '{packages: [
    {name: "taskfast-cli", version: "9.0.0", publish: null, dependencies: [
      {name: "taskfast-agent", path: "/a", kind: null}, {name: "taskfast-client", path: "/c", kind: null}]},
    {name: "taskfast-agent", version: "9.0.0", publish: null, dependencies: [
      {name: "taskfast-client", path: "/c", kind: null}, {name: "taskfast-cli", path: "/x", kind: "dev"}]},
    {name: "taskfast-client", version: "9.0.0", publish: null, dependencies: [
      {name: "taskfast-codegen", path: "/g", kind: "build"}]},
    {name: "taskfast-chains", version: "9.0.0", publish: null, dependencies: []},
    {name: "taskfast-codegen", version: "9.0.0", publish: null, dependencies: []},
    {name: "xtask", version: "9.0.0", publish: [], dependencies: [{name: "taskfast-codegen", path: "/g", kind: null}]}]}'
else
  echo "cargo $*" >> "$FAKE_CARGO_LOG"
fi
CARGO
chmod +x "$work/bin/cargo"

(cd "$work/index" && exec python3 -u -m http.server 0 --bind 127.0.0.1 > "$work/server.log" 2>&1) &
server=$!
for _ in $(seq 50); do
  port="$(grep -o 'port [0-9]*' "$work/server.log" 2>/dev/null | head -1 | cut -d' ' -f2 || true)"
  [ -n "$port" ] && break
  sleep 0.1
done
[ -n "${port:-}" ] || { echo "FAIL: test server did not start"; exit 1; }

run() {
  : > "$work/cargo.log"
  PATH="$work/bin:$PATH" FAKE_CARGO_LOG="$work/cargo.log" CRATES_INDEX_URL="$1" \
    "$root/scripts/publish-dry-run.sh" > "$work/out.log" 2>&1
}

check() {
  local label="$1" want="$2" url="$3" rc=0
  run "$url" || rc=$?
  if [ "$rc" != "$want" ]; then
    echo "FAIL ${label}: exit ${rc}, want ${want}"
    cat "$work/out.log"
    exit 1
  fi
  echo "ok ${label}"
}

check "unpublished versions run the workspace dry-run" 0 "http://127.0.0.1:${port}"
grep -qx 'cargo publish -p taskfast-chains -p taskfast-codegen -p taskfast-client -p taskfast-agent -p taskfast-cli --locked --dry-run' "$work/cargo.log"

record() {
  printf '{"name":"taskfast-client","vers":"%s","cksum":"%064d","yanked":false}\n' "$1" 0
}

{
  record 9.0.0
  for i in $(seq 4000); do record "1.0.$i"; done
} > "$work/index/ta/sk/taskfast-client"
check "a published version matched early in a large response fails" 1 "http://127.0.0.1:${port}"
grep -q 'taskfast-client 9.0.0 is already on crates.io' "$work/out.log"
[ ! -s "$work/cargo.log" ]

{
  for i in $(seq 4000); do record "1.0.$i"; done
  record 9.0.0
} > "$work/index/ta/sk/taskfast-client"
check "a published version matched late in a large response fails" 1 "http://127.0.0.1:${port}"

for body in "" "not json" '{"name":"taskfast-client"}' '{"name":"taskfast-client","vers":null}' '{"name":"taskfast-codegen","vers":"9.0.0"}'; do
  printf '%s' "$body" > "$work/index/ta/sk/taskfast-client"
  check "a malformed 200 response (${body:-empty}) fails" 1 "http://127.0.0.1:${port}"
  grep -q 'index lookup for taskfast-client failed' "$work/out.log"
  [ ! -s "$work/cargo.log" ]
done
rm "$work/index/ta/sk/taskfast-client"

check "a failed index fetch fails" 1 "http://127.0.0.1:1"
grep -q 'index lookup for taskfast-codegen failed' "$work/out.log"
[ ! -s "$work/cargo.log" ]

release() {
  local label="$1" want="$2" ref="$3" recover="$4" rc=0
  : > "$work/cargo.log"
  PATH="$work/bin:$PATH" FAKE_CARGO_LOG="$work/cargo.log" CRATES_INDEX_URL="http://127.0.0.1:${port}" \
    CARGO_REGISTRY_TOKEN=token GITHUB_REF="$ref" RECOVER="$recover" PROPAGATION_TRIES=1 PROPAGATION_SLEEP=0 \
    "$root/scripts/publish-release.sh" > "$work/out.log" 2>&1 || rc=$?
  if [ "$rc" != "$want" ]; then
    echo "FAIL ${label}: exit ${rc}, want ${want}"
    cat "$work/out.log"
    exit 1
  fi
  echo "ok ${label}"
}

tag=refs/tags/taskfast-cli-v9.0.0

release "a fresh release uploads every crate in dependency order" 0 "$tag" false
[ "$(grep -c '^cargo publish' "$work/cargo.log")" = 5 ]
[ "$(grep '^cargo publish' "$work/cargo.log" | tr '\n' ' ')" = "cargo publish -p taskfast-chains --locked cargo publish -p taskfast-codegen --locked cargo publish -p taskfast-client --locked cargo publish -p taskfast-agent --locked cargo publish -p taskfast-cli --locked " ]

release "a tag for another version uploads nothing" 1 refs/tags/taskfast-cli-v9.0.1 false
[ ! -s "$work/cargo.log" ]

{ record 9.0.0; } > "$work/index/ta/sk/taskfast-client"
release "a stale sibling version prevents any upload" 1 "$tag" false
grep -q 'taskfast-client 9.0.0 is already on crates.io' "$work/out.log"
[ ! -s "$work/cargo.log" ]

release "recovery mode skips the published sibling and uploads the rest" 0 "$tag" true
[ "$(grep -c '^cargo publish' "$work/cargo.log")" = 4 ]
! grep -q 'taskfast-client' "$work/cargo.log"
rm "$work/index/ta/sk/taskfast-client"
