#!/usr/bin/env bash
# Fails unless CI passed on a commit: the `CI` and `CodeQL` workflows have a
# successful push run for it, and the latest check-run of every name on it
# finished with success (or skipped: a job whose `if` is false, such as `dco`,
# which runs only on pull requests). Check-runs of the `Release` and `Tag release`
# workflows are not CI and are left out. Names in packaging/ci-exceptions.txt
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

# The check-runs: the suites of the Release and Tag release workflows are left out.
verdicts="$(jq -r --argjson runs "$runs" --arg exempt "$exempt" '
  ($runs | map(select(.name == "Release" or .name == "Tag release") | .check_suite_id)) as $skip
  | ($exempt | split("\n") | map(select(length > 0))) as $ex
  | map(select((.check_suite.id | IN($skip[])) | not))
  | group_by(.name + "|" + (.app.slug // ""))
  | map(max_by(.started_at // ""))
  | sort_by(.name)
  | .[]
  | (if .status != "completed" then "running"
     elif .conclusion == "success" or .conclusion == "skipped" then "ok"
     elif (.name | IN($ex[])) then "exempt"
     else "bad" end) as $v
  | [$v, .name, (.app.slug // "?"), .status, (.conclusion // "-")] | @tsv' <<< "$checks")"

if [ -z "$verdicts" ]; then
  echo "::error::No check-runs found on $sha" >&2
  problems=1
else
  printf '%s\n' "$verdicts" | while IFS=$'\t' read -r v name app status conclusion; do
    printf '  %-7s %-28s %-24s %s/%s\n' "$v" "$name" "$app" "$status" "$conclusion"
  done
  if printf '%s\n' "$verdicts" | grep -q '^\(bad\|running\)'; then
    printf '%s\n' "$verdicts" | grep '^\(bad\|running\)' | while IFS=$'\t' read -r v name app status conclusion; do
      echo "::error::Check-run '$name' ($app) is $status/$conclusion on $sha" >&2
    done
    problems=1
  fi
fi

if [ "$problems" -ne 0 ]; then
  echo "::error::CI did not pass on $sha, so it is not tagged. Nothing has been created." >&2
  exit 1
fi
echo "CI passed on $sha."
