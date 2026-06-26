#!/usr/bin/env bash
# Validate the bundled taskfast-agent skill before publishing.
#
# Checks:
#   1. SKILL.md exists at the expected path.
#   2. YAML frontmatter carries `name:` and `description:`.
#   3. Every `reference/*.md` link in SKILL.md resolves to a real file.
#   4. The npm `skills` CLI discovers exactly one skill named `taskfast-agent`
#      from the local working tree (proves the skills.sh installer will find
#      it the same way when users run `npx skills add Akuja-Inc/taskfast-cli`).
#
# Usage: scripts/validate-skill.sh

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SKILL_DIR="${ROOT}/skills/taskfast-agent"
SKILL_MD="${SKILL_DIR}/SKILL.md"
EXPECTED_NAME="taskfast-agent"

fail() { printf 'skill-validate: %s\n' "$*" >&2; exit 1; }
info() { printf 'skill-validate: %s\n' "$*"; }

[[ -f "${SKILL_MD}" ]] || fail "missing ${SKILL_MD}"

# Extract frontmatter (between leading '---' markers).
frontmatter="$(awk 'NR==1 && $0=="---"{flag=1; next} flag && $0=="---"{exit} flag' "${SKILL_MD}")"
[[ -n "${frontmatter}" ]] || fail "SKILL.md has no YAML frontmatter"

name="$(printf '%s\n' "${frontmatter}" | awk -F': *' '/^name:/ {print $2; exit}')"
[[ "${name}" == "${EXPECTED_NAME}" ]] \
  || fail "frontmatter name='${name}' (expected '${EXPECTED_NAME}')"

# `description` may be a plain scalar or a YAML folded/literal block (e.g.
# `description: >-`). Only require that the key is present with non-empty
# payload — the skills CLI smoke test below does the authoritative parse.
printf '%s\n' "${frontmatter}" | awk '
  /^description:[[:space:]]*\|/ || /^description:[[:space:]]*>/ { block=1; next }
  /^description:[[:space:]]*\S/ { found=1; exit }
  block && /^[[:space:]]+\S/ { found=1; exit }
  END { exit (found ? 0 : 1) }
' || fail "frontmatter description is empty or malformed"

# Validate every reference/*.md link actually exists.
missing=0
while IFS= read -r ref; do
  target="${SKILL_DIR}/${ref}"
  if [[ ! -f "${target}" ]]; then
    printf 'skill-validate: broken link: %s -> %s\n' "${ref}" "${target}" >&2
    missing=1
  fi
done < <(grep -oE '\(reference/[^)#]+\.md' "${SKILL_MD}" | sed 's/^(//' | sort -u)
[[ "${missing}" -eq 0 ]] || fail "one or more reference/*.md links are broken"

# Smoke-test through the authoritative installer.
if ! command -v npx >/dev/null 2>&1; then
  fail "npx not found; install Node.js to run the installer smoke test"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "${tmp}"' EXIT

info "running 'npx skills add ${ROOT} -l' in ${tmp}"
if ! (cd "${tmp}" && npx -y -p skills add-skill add "${ROOT}" -l 2>&1 | tee out.log) \
    | grep -q "${EXPECTED_NAME}"; then
  fail "skills installer did not surface '${EXPECTED_NAME}'"
fi

# Parity check: the bundled copy shipped inside the published taskfast-cli crate
# (crates/taskfast-cli/skills/taskfast-agent) must be byte-identical to this
# workspace source of truth. The CLI embeds these files via include_str! at
# build time; if the bundled copy drifts, `cargo install taskfast-cli` ships a
# stale skill. Keep them in sync by copying the workspace tree over the bundled
# one whenever the skill is edited.
bundled_dir="${ROOT}/crates/taskfast-cli/skills/taskfast-agent"
if [[ -d "${bundled_dir}" ]]; then
  while IFS= read -r f; do
    rel="${f#${SKILL_DIR}/}"
    if ! diff -q "${f}" "${bundled_dir}/${rel}" >/dev/null 2>&1; then
      printf 'skill-validate: bundled skill drift: %s\n' "${rel}" >&2
      drift=1
    fi
  done < <(find "${SKILL_DIR}" -type f | sort)
  # Also catch files present in the bundle but absent from the source of truth.
  extra="$(cd "${bundled_dir}" && find . -type f | sort)"
  src_list="$(cd "${SKILL_DIR}" && find . -type f | sort)"
  [[ "${extra}" == "${src_list}" ]] || { printf 'skill-validate: bundled skill file set differs from source of truth\n' >&2; drift=1; }
  [[ "${drift:-0}" -eq 0 ]] || fail "bundled crates/taskfast-cli/skills/taskfast-agent drifted from skills/taskfast-agent — re-copy the tree"
  info "OK: bundled skill tree matches workspace source of truth"
fi

info "OK: ${EXPECTED_NAME} is publishable"
