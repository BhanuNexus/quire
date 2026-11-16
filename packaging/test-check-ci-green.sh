#!/usr/bin/env bash
# Tests of packaging/check-ci-green.sh against made-up API answers (CI's
# `packaging` job): no network and no token.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
tool="$here/check-ci-green.sh"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

runs_ok='[
 {"name":"CI","event":"push","status":"completed","conclusion":"success","created_at":"2026-01-01T00:00:00Z","check_suite_id":1},
 {"name":"CodeQL","event":"push","status":"completed","conclusion":"success","created_at":"2026-01-01T00:00:00Z","check_suite_id":2},
 {"name":"Release","event":"workflow_dispatch","status":"completed","conclusion":"failure","created_at":"2026-01-01T00:00:00Z","check_suite_id":3}]'
check() { # name status conclusion started suite app
  printf '{"name":"%s","status":"%s","conclusion":%s,"started_at":"%s","app":{"slug":"%s"},"check_suite":{"id":%s}}' \
    "$1" "$2" "$3" "$4" "${6:-github-actions}" "${5:-1}"
}
good="[$(check lint completed '"success"' 2026-01-01T00:01:00Z), $(check dco completed '"skipped"' 2026-01-01T00:01:00Z), $(check "test (windows-latest)" completed '"success"' 2026-01-01T00:01:00Z)]"

run() { # runs checks
  printf '%s' "$1" > "$work/runs.json"; printf '%s' "$2" > "$work/checks.json"
  CI_RUNS_FILE="$work/runs.json" CI_CHECK_RUNS_FILE="$work/checks.json" CI_ALSO_REQUIRED_FILE=/dev/null \
    "$tool" owner/repo abc123 > "$work/out.txt" 2>&1
}
expect_pass() { if ! run "${@:2}"; then echo "FAIL: $1 was refused"; cat "$work/out.txt"; exit 1; fi; echo "ok: $1"; }
expect_fail() { if run "${@:2}"; then echo "FAIL: $1 was accepted"; cat "$work/out.txt"; exit 1; fi; echo "ok: $1 is refused"; }

expect_pass "green CI, a skipped dco" "$runs_ok" "$good"
expect_fail "a failed check-run" "$runs_ok" "[$(check lint completed '"failure"' 2026-01-01T00:01:00Z)]"
expect_fail "a cancelled check-run" "$runs_ok" "[$(check lint completed '"cancelled"' 2026-01-01T00:01:00Z)]"
expect_fail "a neutral check-run" "$runs_ok" "[$(check CodeQL completed '"neutral"' 2026-01-01T00:01:00Z 1 github-advanced-security)]"
expect_fail "a check-run still running" "$runs_ok" "[$(check lint in_progress null 2026-01-01T00:01:00Z)]"
expect_fail "no check-runs at all" "$runs_ok" "[]"
expect_fail "no CodeQL push run" '[{"name":"CI","event":"push","status":"completed","conclusion":"success","created_at":"2026-01-01T00:00:00Z","check_suite_id":1}]' "$good"
expect_fail "a CI push run that is still going" '[{"name":"CI","event":"push","status":"in_progress","conclusion":null,"created_at":"2026-01-01T00:00:00Z","check_suite_id":1},{"name":"CodeQL","event":"push","status":"completed","conclusion":"success","created_at":"2026-01-01T00:00:00Z","check_suite_id":2}]' "$good"
# A failure followed by a re-run that passed: the latest of each name counts.
expect_pass "a re-run that passed" "$runs_ok" "[$(check lint completed '"failure"' 2026-01-01T00:01:00Z), $(check lint completed '"success"' 2026-01-01T00:09:00Z)]"
expect_fail "a re-run that failed after a pass" "$runs_ok" "[$(check lint completed '"success"' 2026-01-01T00:01:00Z), $(check lint completed '"failure"' 2026-01-01T00:09:00Z)]"
# Check-runs of other suites (Release workflow 3; Dependabot, Audit, GHAS: 9) do not decide.
expect_pass "a failed Release dry run on the same commit" "$runs_ok" "${good%]}, $(check publish completed '"failure"' 2026-01-01T00:05:00Z 3)]"
ghas="${good%]}, $(check CodeQL completed '"failure"' 2026-01-01T00:01:00Z 9 github-advanced-security)]"
expect_pass "the GHAS CodeQL pull-request check failing" "$runs_ok" "$ghas"
expect_pass "a failed Dependabot job" "${runs_ok%]}, {\"name\":\"dependabot\",\"event\":\"dynamic\",\"status\":\"completed\",\"conclusion\":\"success\",\"created_at\":\"2026-01-01T00:00:00Z\",\"check_suite_id\":9}]" "${good%]}, $(check advisories completed '"failure"' 2026-01-01T00:01:00Z 9)]"
expect_pass "a Dependabot job still running" "$runs_ok" "${good%]}, $(check update in_progress null 2026-01-01T00:01:00Z 9)]"
audit_failed='{"name":"Audit","event":"schedule","status":"completed","conclusion":"failure","created_at":"2026-01-01T00:00:00Z","check_suite_id":8}'
expect_pass "a failed weekly Audit (a warning only)" "${runs_ok%]}, $audit_failed]" "$good"
grep -q '::warning::The weekly dependency Audit' "$work/out.txt" || { echo "FAIL: no Audit warning"; cat "$work/out.txt"; exit 1; }
# A queued re-run with no start time is not hidden behind an older success.
expect_fail "a queued re-run after a success" "$runs_ok" "[$(check lint completed '"success"' 2026-01-01T00:01:00Z), $(check lint queued null null 1)]"
# A name outside CI and CodeQL decides only when opted in.
printf '%s' "$runs_ok" > "$work/runs.json"; printf '%s' "${good%]}, $(check advisories completed '"failure"' 2026-01-01T00:01:00Z 9)]" > "$work/checks.json"
printf 'advisories  # test\n' > "$work/also.txt"
if CI_RUNS_FILE="$work/runs.json" CI_CHECK_RUNS_FILE="$work/checks.json" CI_ALSO_REQUIRED_FILE="$work/also.txt" "$tool" owner/repo abc123 > "$work/out.txt" 2>&1; then
  echo "FAIL: an opted-in failing check was accepted"; exit 1
fi
echo "ok: an opted-in check that failed is refused"
bad="[$(check lint completed '"failure"' 2026-01-01T00:01:00Z 1)]"
expect_fail "a failed CI check-run" "$runs_ok" "$bad"
echo "All check-ci-green tests passed."
