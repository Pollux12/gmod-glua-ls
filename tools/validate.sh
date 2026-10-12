#!/usr/bin/env bash
# Validation tiers (see AGENTS.md):
#   change  before each commit, on the vehicle base              (~2-3 min)
#   step    at the end of a task, on CityRP                       (~6-8 min)
#   phase   at a phase exit: both tiers plus sweeps and suites    (~15 min)
# usage: tools/validate.sh change|step|phase
# Env: GLUALS_ANNOTATIONS, GLUALS_VB (change, phase), GLUALS_CITYRP (step, phase),
#      GLUALS_GAMEMODES (phase: a folder of gamemodes), VALIDATE_REF (optional
#      folder with vb.json / cityrp.json glua_check references to diff against),
#      CHECK_CACHE=1 (adds cache check mode), CHECK_QUERY=1 (adds query check
#      mode), SKIP_BUILD=1, VALIDATE_OUT,
#      VALIDATE_BIN (folder of prebuilt binaries; implies SKIP_BUILD).
# Exits 1 if any step failed; logs go to VALIDATE_OUT (default target/validate/<tier>).
set -u
TIER=${1:?usage: validate.sh change|step|phase}
REPO=$(cd "$(dirname "$0")/.." && pwd)
OUT=${VALIDATE_OUT:-$REPO/target/validate/$TIER}
REL=${VALIDATE_BIN:-$REPO/target/release}
X=""; [ -f "$REL/glua_ls.exe" ] || [ "${OS:-}" = "Windows_NT" ] && X=".exe"
ANN=${GLUALS_ANNOTATIONS:?GLUALS_ANNOTATIONS is required}
mkdir -p "$OUT"
cd "$REPO"
FAILED=0
START=$(date +%s)

step() { # name command...: runs, logs, prints a summary line
  local name=$1; shift
  local t=$(date +%s)
  "$@" > "$OUT/$name.log" 2>&1
  local rc=$?
  [ $rc -ne 0 ] && FAILED=1
  echo "== $name: $([ $rc -eq 0 ] && echo ok || echo FAILED) in $(( $(date +%s) - t ))s"
  grep -E "^\[[a-z_:0-9 ]+(\[.*\])?\] (\+|[0-9]+ difference)|DIFF .*removed|STALE|LOCALITY|panicked|FAILED|differences|IDENTICAL|test result|mismatches|^\[mem\]" \
    "$OUT/$name.log" | grep -v "+0 index, +0 diagnostics\|+0 diagnostics" | head -20
}

det() { # corpus jobs env...
  local corpus=$1 jobs=$2; shift 2
  env GLUALS_CHECK_CONVERGENCE=1 DET_CODEBASE="$corpus" DET_ANNOTATIONS="$ANN" DET_JOBS="$jobs" "$@" "$REL/determinism$X"
}

corpus() { # name dir: glua_check JSON, diffed against VALIDATE_REF/<name>.json when present
  local name=$1 dir=$2
  "$REL/glua_check$X" "$dir" -f json --output "$OUT/$name.json" --gmod-annotations "$ANN" > /dev/null 2>&1
  [ -f "$OUT/$name.json" ] || { echo "glua_check wrote nothing"; return 1; }
  if [ -n "${VALIDATE_REF:-}" ] && [ -f "$VALIDATE_REF/$name.json" ]; then
    node tools/diagnostics_diff.js "$VALIDATE_REF/$name.json" "$OUT/$name.json"
  else
    echo "no reference for $name; output kept at $OUT/$name.json"
  fi
}

cache_check() { # name dir: GLUALS_CHECK_CACHE mismatches must be 0
  local name=$1 dir=$2
  : > "$OUT/$name.cache.json"
  GLUALS_CHECK_CACHE=1 timeout 1800 "$REL/glua_check$X" "$dir" -f json --output "$OUT/$name.cache.json" --gmod-annotations "$ANN" > "$OUT/$name.cache.txt" 2>&1
  [ -s "$OUT/$name.cache.json" ] || { echo "glua_check wrote nothing (timed out or crashed)"; return 1; }
  local n; n=$(grep -cE '^\[cache\] .*mismatch' "$OUT/$name.cache.txt")
  echo "$n mismatches"
  [ "$n" -eq 0 ]
}

query_check() { # name dir: GLUALS_CHECK_QUERY mismatches must be 0
  local name=$1 dir=$2
  : > "$OUT/$name.query.json"
  GLUALS_CHECK_QUERY=1 timeout 1800 "$REL/glua_check$X" "$dir" -f json --output "$OUT/$name.query.json" --gmod-annotations "$ANN" > "$OUT/$name.query.txt" 2>&1
  [ -s "$OUT/$name.query.json" ] || { echo "glua_check wrote nothing (timed out or crashed)"; return 1; }
  local n; n=$(grep -cE '^\[query\] .*mismatch' "$OUT/$name.query.txt")
  echo "$n mismatches"
  [ "$n" -eq 0 ]
}

gate() { # dir file find replace reference: the corpus step's glua_check of the same build
  LSP_SERVER="$REL/glua_ls$X" GLUA_CHECK="$REL/glua_check$X" LSP_CODEBASE="$1" LSP_ANNOTATIONS="$ANN" node tools/lsp_drift.js --file "$2" \
    --edit-find "$3" --edit-replace "$4" --reference "$5"
}

# Target edits: path<TAB>find<TAB>replace, \n escaped.
VB_FILE=lua/glide/sh_lighting_api.lua
VB_FIND='function Lighting.GetAdapter(name)'
VB_REPLACE='function Lighting.GetAdapter(name, extra)'
printf '%s\t%s\t%s\n' "$VB_FILE" "$VB_FIND" "$VB_REPLACE" > "$OUT/vb.edits"
CR_FILE=gamemode/core/sh_data.lua
CR_FIND='function cityrp.data.Set(key, value, bGlobal, bIgnoreMap)'
CR_REPLACE='function cityrp.data.Set(key, value, bGlobal, bIgnoreMap, extra)'
{ printf '%s\t%s\t%s\n' "$CR_FILE" "$CR_FIND" "$CR_REPLACE"
  printf '%s\t%s\t%s\n' gamemode/core/sh_configuration.lua 'cityrp.configuration["Quick AutoRefresh"] = false' \
    'if SERVER then cityrp.realmProbe = 1 end\n\ncityrp.configuration["Quick AutoRefresh"] = false'
} > "$OUT/cityrp.edits"
CR_CORE=gamemode/core/sh_data.lua,gamemode/core/sh_configuration.lua,gamemode/core/sh_util.lua

if [ -z "${SKIP_BUILD:-}" ] && [ -z "${VALIDATE_BIN:-}" ]; then
  echo "== build"
  cargo build --release -p determinism -p glua_check -p glua_ls 2>&1 | grep -E "^error|Finished"
  [ "${PIPESTATUS[0]}" -eq 0 ] || exit 1
fi

TARGET_STAGES=schedule,schedule:deferred,edit,revert,noop,reload,delete,recreate,gate

if [ "$TIER" = change ] || [ "$TIER" = phase ]; then
  VB=${GLUALS_VB:?GLUALS_VB is required}
  if [ "$TIER" = phase ]; then
    # The fingerprint check runs the whole suite; it overlaps the harness runs,
    # which leave most cores idle.
    ( rc=0
      GLUALS_CHECK_FINGERPRINTS=1 cargo test -p glua_code_analysis --lib > "$OUT/suite_fingerprints.log" 2>&1 || rc=1
      cargo test -p glua_ls > "$OUT/suite_ls.log" 2>&1 || rc=1
      exit $rc ) &
    SUITES=$!
  else
    step suite cargo test -p glua_code_analysis --lib
  fi
  step vb_corpus corpus vb "$VB"
  step vb_det det "$VB" 4 DET_EDITS="$OUT/vb.edits" DET_ONLY="$VB_FILE" DET_STAGES=split:2,shift,$TARGET_STAGES
  step vb_gate gate "$VB" "$VB_FILE" "$VB_FIND" "$VB_REPLACE" "$OUT/vb.json"
  [ -n "${CHECK_CACHE:-}" ] && step vb_cache cache_check vb "$VB"
  [ -n "${CHECK_QUERY:-}" ] && step vb_query query_check vb "$VB"
fi

if [ "$TIER" = step ]; then
  CR=${GLUALS_CITYRP:?GLUALS_CITYRP is required}
  step cityrp_corpus corpus cityrp "$CR"
  step cityrp_det det "$CR" 2 DET_EDITS="$OUT/cityrp.edits" DET_ONLY="$CR_FILE" DET_STAGES=$TARGET_STAGES
  step cityrp_gate gate "$CR" "$CR_FILE" "$CR_FIND" "$CR_REPLACE" "$OUT/cityrp.json"
  [ -n "${CHECK_CACHE:-}" ] && step cityrp_cache cache_check cityrp "$CR"
  [ -n "${CHECK_QUERY:-}" ] && step cityrp_query query_check cityrp "$CR"
fi

if [ "$TIER" = phase ]; then
  CR=${GLUALS_CITYRP:?GLUALS_CITYRP is required}
  GM=${GLUALS_GAMEMODES:?GLUALS_GAMEMODES is required}
  step vb_hubs det "$VB" 4 DET_HUBS=6 DET_EACH=1 DET_STAGES=touch,shift,delete,recreate,rename
  step vb_stride det "$VB" 4 DET_STRIDE=10 DET_STAGES=touch,shift
  for g in "$GM"/*/; do
    step "gm_$(basename "$g")" det "$g" 4 DET_STAGES=schedule,schedule:deferred
  done
  # Helix's item registry and config: loops of statements across its item files.
  [ -d "$GM/helix" ] && step gm_helix_sweep det "$GM/helix" 4 DET_HUBS=4 DET_EACH=1 \
    DET_ONLY=gamemode/core/libs/sh_item.lua,gamemode/core/meta/sh_item.lua,gamemode/core/sh_config.lua \
    DET_STAGES=touch,shift,delete,recreate,rename
  # Typed into while cleared loops settle, with other files put ahead between slices.
  [ -d "$GM/helix" ] && step gm_helix_typing det "$GM/helix" 4 DET_TARGETS=gamemode/core/libs/sh_item.lua \
    DET_EDIT_FIND='function ix.item.Get(identifier)' DET_EDIT_REPLACE='function ix.item.Get(identifier, extra)' \
    DET_STAGES=typing,gate
  step cityrp_corpus corpus cityrp "$CR"
  step cityrp_det det "$CR" 2 DET_EDITS="$OUT/cityrp.edits" DET_HUBS=5 DET_ONLY="$CR_CORE" \
    DET_STAGES=$TARGET_STAGES,shift,rename
  step cityrp_gate gate "$CR" "$CR_FILE" "$CR_FIND" "$CR_REPLACE" "$OUT/cityrp.json"
  [ -n "${CHECK_CACHE:-}" ] && step cityrp_cache cache_check cityrp "$CR"
  [ -n "${CHECK_QUERY:-}" ] && step cityrp_query query_check cityrp "$CR"
  wait $SUITES || FAILED=1
  echo "== suites:"; grep -hE "test result|FAILED|panicked" "$OUT/suite_fingerprints.log" "$OUT/suite_ls.log" | head
fi

echo "== $TIER $([ $FAILED -eq 0 ] && echo PASSED || echo FAILED) in $(( $(date +%s) - START ))s (logs: $OUT)"
exit $FAILED
