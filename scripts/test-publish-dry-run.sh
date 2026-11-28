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
    {name: "taskfast-codegen", version: "9.0.0"},
    {name: "taskfast-client", version: "9.0.0"},
    {name: "taskfast-chains", version: "9.0.0"},
    {name: "taskfast-agent", version: "9.0.0"},
    {name: "taskfast-cli", version: "9.0.0"}]}'
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
grep -q 'publish --workspace --exclude xtask --locked --dry-run' "$work/cargo.log"

{
  echo '{"vers":"9.0.0"}'
  head -c 300000 /dev/zero | tr '\0' 'x'
  echo
} > "$work/index/ta/sk/taskfast-client"
check "a published version matched early in a large response fails" 1 "http://127.0.0.1:${port}"
grep -q 'taskfast-client 9.0.0 is already on crates.io' "$work/out.log"
[ ! -s "$work/cargo.log" ]
rm "$work/index/ta/sk/taskfast-client"

check "a failed index fetch fails" 1 "http://127.0.0.1:1"
grep -q 'index lookup for taskfast-codegen failed' "$work/out.log"
[ ! -s "$work/cargo.log" ]
