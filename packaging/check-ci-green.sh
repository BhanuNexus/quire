#!/usr/bin/env bash
# Fails unless CI passed on a commit: the `CI` and `CodeQL` workflows have a
# successful push run for it, and the latest check-run of every name in those two
# runs finished with success (or skipped: a job whose `if` is false, such as `dco`,
# which runs only on pull requests). Other check-runs on the commit (Dependabot,
# the weekly Audit, the GitHub Advanced Security "CodeQL" pull-request check, the
# Release workflows) are printed as information and do not decide, unless named in
# packaging/ci-also-required.txt (empty). Names in packaging/ci-exceptions.txt
# (empty by default) are let through.
#   packaging/check-ci-green.sh <owner/repo> <sha>
# Needs `gh` with GH_TOKEN (checks: read, actions: read) and `jq`.
# CI_CHECK_RUNS_FILE and CI_RUNS_FILE (JSON arrays of check-runs and of workflow
# runs) replace the two API reads, for tests.
set -euo pipefail

repo="${1:?usage: check-ci-green.sh <owner/repo> <sha>}"
sha="${2:?usage: check-ci-green.sh <owner/repo> <sha>}"
here="$(cd "$(dirname "$0")" && pwd)"
exceptions_file="${CI_EXCEPTIONS_FILE:-$here/ci-exceptions.txt}"

# The exception list: one check-run name per line, each with a reason.
exempt=""
while IFS= read -r line || [ -n "$line" ]; do
  case "$line" in ''|'#'*) continue ;; esac
  name="${line%%#*}"
  name="$(printf '%s' "$name" | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')"
  [ -n "$name" ] || continue
  case "$line" in
    *'#'*[![:space:]]*) ;;
    *) echo "::error::$exceptions_file: '$name' has no '# reason'" >&2; exit 1 ;;
  esac
  exempt+="$name"$'\n'
  echo "Exception (packaging/ci-exceptions.txt): $name"
done < "$exceptions_file"

# Only the fields used below (the full objects are large).
runs_fields='{name, event, status, conclusion, created_at, check_suite_id}'
checks_fields='{name, status, conclusion, started_at, app: {slug: .app.slug}, check_suite: {id: .check_suite.id}}'
required=""
required_file="${CI_ALSO_REQUIRED_FILE:-$here/ci-also-required.txt}"
if [ -f "$required_file" ]; then
  while IFS= read -r line || [ -n "$line" ]; do
    name="${line%%#*}"
    name="$(printf '%s' "$name" | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')"
    [ -n "$name" ] || continue
    required+="$name"$'\n'
    echo "Also required (packaging/ci-also-required.txt): $name"
  done < "$required_file"
fi

if [ -n "${CI_RUNS_FILE:-}" ]; then
  runs="$(jq "map($runs_fields)" "$CI_RUNS_FILE")"
else
  runs="$(gh api --paginate "repos/$repo/actions/runs?head_sha=$sha&per_page=100" --jq ".workflow_runs[] | $runs_fields" | jq -s .)"
fi
if [ -n "${CI_CHECK_RUNS_FILE:-}" ]; then
  checks="$(jq "map($checks_fields)" "$CI_CHECK_RUNS_FILE")"
else
  checks="$(gh api --paginate "repos/$repo/commits/$sha/check-runs?per_page=100" --jq ".check_runs[] | $checks_fields" | jq -s .)"
fi

problems=0

# The CI and CodeQL workflows each ran on the push of this commit, and passed.
for workflow in CI CodeQL; do
  state="$(jq -r --arg w "$workflow" '
    [.[] | select(.name == $w and .event == "push")] | sort_by(.created_at) | last
    | if . == null then "no run" elif .status != "completed" then "still " + .status else .conclusion end' <<< "$runs")"
  if [ "$state" = success ]; then
    echo "Workflow $workflow: success on the push of $sha"
  else
    echo "::error::Workflow $workflow on the push of $sha: $state (it must have succeeded; wait for it, or fix and merge)" >&2
    problems=1
  fi
done

# The check-runs that decide: those of the push runs of the CI and CodeQL workflows
# on this commit (by check suite), plus any name in packaging/ci-also-required.txt.
# Everything else on the commit (Dependabot jobs, the weekly Audit run, the GitHub
# Advanced Security "CodeQL" pull-request check, the Release workflows) is printed
# as "info" and does not decide.
verdicts="$(jq -r --argjson runs "$runs" --arg exempt "$exempt" --arg required "$required" '
  ($runs | map(select((.name == "CI" or .name == "CodeQL") and .event == "push") | .check_suite_id)) as $suites
  | ($exempt | split("\n") | map(select(length > 0))) as $ex
  | ($required | split("\n") | map(select(length > 0))) as $req
  | group_by(.name + "|" + (.app.slug // "") + "|" + ((.check_suite.id | tostring)))
  | map(max_by(.started_at // "9999"))
  | map(. + {judged: ((.check_suite.id | IN($suites[])) or (.name | IN($req[])))})
  | sort_by(.name)
  | .[]
  | (if .status != "completed" then "running"
     elif .conclusion == "success" or .conclusion == "skipped" then "ok"
     elif (.name | IN($ex[])) then "exempt"
     else "bad" end) as $v
  | [(if .judged then $v else "info" end), .name, (.app.slug // "?"), .status, (.conclusion // "-")] | @tsv' <<< "$checks")"

judged="$(printf '%s\n' "$verdicts" | grep -v '^info' | grep -c . || true)"
if [ "$judged" -eq 0 ]; then
  echo "::error::No check-runs of the CI and CodeQL push runs found on $sha" >&2
  problems=1
fi
printf '%s\n' "$verdicts" | sed '/^$/d' | while IFS=$'\t' read -r v name app status conclusion; do
  printf '  %-7s %-28s %-24s %s/%s\n' "$v" "$name" "$app" "$status" "$conclusion"
done
if printf '%s\n' "$verdicts" | grep -q '^\(bad\|running\)'; then
  printf '%s\n' "$verdicts" | grep '^\(bad\|running\)' | while IFS=$'\t' read -r v name app status conclusion; do
    echo "::error::Check-run '$name' ($app) is $status/$conclusion on $sha" >&2
  done
  problems=1
fi
# Information that does not decide: the weekly advisory audit, shown prominently.
audit="$(jq -r '[.[] | select(.name == "Audit")] | sort_by(.created_at) | last
  | if . == null then "none" elif .status != "completed" then "still " + .status else .conclusion end' <<< "$runs")"
if [ "$audit" = success ]; then
  echo "Weekly Audit run on this commit: success (it does not decide the result)."
elif [ "$audit" = none ]; then
  echo "No weekly Audit run on this commit (it does not decide the result)."
else
  echo "::warning::The weekly dependency Audit on $sha is: $audit. It does not block a release; read it before tagging (cargo deny check advisories)."
fi

if [ "$problems" -ne 0 ]; then
  echo "::error::CI did not pass on $sha, so it is not tagged. Nothing has been created." >&2
  exit 1
fi
echo "CI passed on $sha."
