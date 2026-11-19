#!/usr/bin/env bash
# Exercise the real partition, runner and CI shell with a fake Cargo launcher.
set -euo pipefail
unset CDPATH
source_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fixture="$(mktemp -d "${TMPDIR:-/tmp}/leg-test.XXXXXX")"
trap 'rm -rf -- "$fixture"' EXIT
mkdir -p "$fixture/scripts/dev" "$fixture/dev/ci" "$fixture/bin"
cp "$source_root/scripts/dev/leg.sh" "$fixture/scripts/dev/"
cp "$source_root/dev/ci/test-partition.json" "$fixture/dev/ci/"
cat > "$fixture/scripts/dev/cargo-local.sh" <<'CARGO'
#!/usr/bin/env bash
set -euo pipefail
event="$(printf '%q ' "$@")"
event="${event% }"
printf '%s\n' "$event" >> "$TEST_LEG_TRACE"
# A child must not consume the extra-run loop's next record.
cat >/dev/null
if [[ "$event" == "${TEST_LEG_FAIL_EVENT:-}" ]]; then exit 7; fi
CARGO
chmod +x "$fixture/scripts/dev/cargo-local.sh"
cp "$fixture/scripts/dev/cargo-local.sh" "$fixture/bin/cargo"
export PATH="$fixture/bin:$PATH" TEST_LEG_TRACE="$fixture/trace"
export LEG_RUNNER=test TEST_LEG_FAIL_EVENT=
cd "$fixture"

run() {
  : > "$TEST_LEG_TRACE"
  status=0
  "$@" > "$fixture/output" 2>&1 || status=$?
}

check() {
  local expected_status="$1"
  shift
  if [[ "$status" -ne "$expected_status" ]]; then
    cat "$fixture/output" >&2
    echo "expected exit $expected_status, got $status" >&2
    exit 1
  fi
  : > "$fixture/expected"
  if [[ $# -gt 0 ]]; then printf '%s\n' "$@" > "$fixture/expected"; fi
  diff -u "$fixture/expected" "$TEST_LEG_TRACE"
}

runtime='-p reiclaw-runtime -p reiclaw-eval --features reiclaw-runtime/heavy-tests\,reiclaw-runtime/memory-core'
landlock='-p reiclaw-runtime --features reiclaw-runtime/heavy-tests\,reiclaw-runtime/memory-core\,reiclaw-runtime/sandbox-landlock -- landlock'
for LEG_RUNNER in test nextest; do
  prefix=test
  [[ "$LEG_RUNNER" == test ]] || prefix='nextest run'
  main="$prefix --locked $runtime"
  extra="$prefix --locked $landlock"

  run bash scripts/dev/leg.sh runtime
  check 0 "$main" "$extra"
  run bash scripts/dev/leg.sh --no-run runtime
  check 0 "$prefix --locked --no-run $runtime" "$prefix --locked --no-run $landlock"
  # Preserve old post-leg arguments: only the explicit pre-leg mode covers all phases.
  run bash scripts/dev/leg.sh runtime --no-run
  check 0 "$main --no-run" "$extra"
  run bash scripts/dev/leg.sh runtime -- 'name with spaces'
  check 0 "$main -- name\ with\ spaces" "$extra"
  run bash scripts/dev/leg.sh --no-run --dry-run runtime
  check 0
  TEST_LEG_FAIL_EVENT="$main" run bash scripts/dev/leg.sh runtime
  check 7 "$main"
  TEST_LEG_FAIL_EVENT="$extra" run bash scripts/dev/leg.sh runtime
  check 7 "$main" "$extra"
  # A selected phase runs alone, keeps pre-leg modes, and forwards post-leg
  # cargo args to the main run only.
  run bash scripts/dev/leg.sh --phase main runtime
  check 0 "$main"
  run bash scripts/dev/leg.sh --phase landlock runtime
  check 0 "$extra"
  run bash scripts/dev/leg.sh --no-run --phase landlock runtime
  check 0 "$prefix --locked --no-run $landlock"
  run bash scripts/dev/leg.sh --phase main runtime -- 'name with spaces'
  check 0 "$main -- name\ with\ spaces"
  run bash scripts/dev/leg.sh --phase landlock runtime -- 'name with spaces'
  check 0 "$extra"
  TEST_LEG_FAIL_EVENT="$extra" run bash scripts/dev/leg.sh --phase landlock runtime
  check 7 "$extra"
done

LEG_RUNNER=test
# An unknown phase, or a phase of another leg, fails before any Cargo work.
run bash scripts/dev/leg.sh --phase nope runtime
check 1
run bash scripts/dev/leg.sh --phase voice-surface runtime
check 1
run bash scripts/dev/leg.sh --phase
check 1

# A leg without extra runs emits only its main phase.
run bash scripts/dev/leg.sh app
check 0 'test --locked -p reiclaw -p reiclaw-config --features reiclaw/nodes'
run bash scripts/dev/leg.sh channels
check 0 \
  'test --locked -p reiclaw-gateway --features reiclaw-gateway/nodes\,reiclaw-gateway/memory-core' \
  'test --locked -p reiclaw-gateway --features reiclaw-gateway/nodes\,reiclaw-gateway/memory-core\,reiclaw-gateway/voice-surface -- ws::tests::'

# Run the actual CI step's shell (its env is set as in the workflow), not a copy
# of its command construction. No YAML dependency is needed for this block.
awk '
  /^      - name: Run tests$/ { step = 1; next }
  step && /^        run: \|$/ { body = 1; next }
  body && /^          / { sub(/^          /, ""); print; next }
  body && /^$/ { print; next }
  body { exit }
' "$source_root/.github/workflows/ci.yml" > "$fixture/ci-run.sh"
[[ -s "$fixture/ci-run.sh" ]] || { echo 'missing CI test step' >&2; exit 1; }
export LEG=runtime LEG_RUNNER=nextest CARGO_PROFILE_DEV_DEBUG=line-tables-only
main="nextest run --locked --no-fail-fast $runtime"
extra="nextest run --locked --no-fail-fast $landlock"
# Each CI matrix entry runs exactly its own phase.
PHASE=main run bash -e -o pipefail "$fixture/ci-run.sh"
check 0 "$main"
PHASE=landlock run bash -e -o pipefail "$fixture/ci-run.sh"
check 0 "$extra"
TEST_LEG_FAIL_EVENT="$main" PHASE=main run bash -e -o pipefail "$fixture/ci-run.sh"
check 7 "$main"
TEST_LEG_FAIL_EVENT="$extra" PHASE=landlock run bash -e -o pipefail "$fixture/ci-run.sh"
check 7 "$extra"

# The CI matrix is exactly every leg's main phase plus every extra run, so the
# union of matrix jobs runs what a full local leg run does.
run bash scripts/dev/leg.sh --matrix
check 0
expected_matrix="$(jq -c '. as $p | [.legs | keys_unsorted[] as $leg
  | {leg: $leg, phase: "main"}, ($p.extra_runs[$leg] // [] | .[] | {leg: $leg, phase: .name})]' \
  dev/ci/test-partition.json)"
actual_matrix="$(jq -c '[.[] | {leg, phase}]' "$fixture/output")"
[[ "$actual_matrix" == "$expected_matrix" ]] || {
  echo "matrix mismatch: $actual_matrix != $expected_matrix" >&2
  exit 1
}
jq -e 'map(.name) | (unique | length) == length' "$fixture/output" >/dev/null \
  || { echo 'matrix job names are not unique' >&2; exit 1; }
for entry in $(jq -r '.[] | "\(.leg):\(.phase)"' "$fixture/output"); do
  run bash scripts/dev/leg.sh --dry-run --phase "${entry#*:}" "${entry%%:*}"
  check 0
done

# Phase names must stay addressable: matrix generation rejects unknown legs
# and duplicate or reserved extra-run names.
cp dev/ci/test-partition.json "$fixture/partition.orig"
for broken in \
  '.features.ghost = ["x"]' \
  '.extra_runs.ghost = []' \
  '.extra_runs.runtime += [{"name":"main","features":["x"],"filter":"x"}]' \
  '.extra_runs.runtime += [{"name":"landlock","features":["x"],"filter":"x"}]' \
  '.extra_runs.runtime[0].packages = []' \
  '.extra_runs.runtime[0].packages = ["reiclaw-api"]' \
  '.extra_runs.runtime[0].packages = "reiclaw-runtime"'; do
  jq "$broken" "$fixture/partition.orig" > dev/ci/test-partition.json
  run bash scripts/dev/leg.sh --matrix
  if [[ "$status" -eq 0 ]]; then
    cat "$fixture/output" >&2
    echo "matrix accepted broken partition: $broken" >&2
    exit 1
  fi
done
cp "$fixture/partition.orig" dev/ci/test-partition.json

# Exercise multiple extra phases and prove a failed extra prevents later work.
jq '.extra_runs.runtime += [{"name":"following","features":["following"],"filter":"following"}]' \
  dev/ci/test-partition.json > "$fixture/partition.json"
mv "$fixture/partition.json" dev/ci/test-partition.json
main="nextest run --locked $runtime"
extra="nextest run --locked $landlock"
following='nextest run --locked -p reiclaw-runtime -p reiclaw-eval --features reiclaw-runtime/heavy-tests\,reiclaw-runtime/memory-core\,following -- following'
run bash scripts/dev/leg.sh runtime
check 0 "$main" "$extra" "$following"
TEST_LEG_FAIL_EVENT="$extra" run bash scripts/dev/leg.sh runtime
check 7 "$main" "$extra"
# With several extra runs, a selected phase still runs only itself.
run bash scripts/dev/leg.sh --phase landlock runtime
check 0 "$extra"
run bash scripts/dev/leg.sh --phase following runtime
check 0 "$following"
run bash scripts/dev/leg.sh --matrix
check 0
jq -e '[.[] | select(.leg == "runtime") | .phase] == ["main", "landlock", "following"]' \
  "$fixture/output" >/dev/null || { cat "$fixture/output" >&2; exit 1; }

echo 'PASS: extra phases, selected phases, CI matrix, compile-only phases, CI parity, legacy arguments and failure propagation'
