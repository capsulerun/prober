#!/usr/bin/env bash
set -euo pipefail

temporary_directory=$RUNNER_TEMP

if command -v cygpath > /dev/null; then
  temporary_directory=$(cygpath -m "$RUNNER_TEMP")
fi

last_selection="$temporary_directory/prober/last-selection"
report_arguments=()

[ -f "$last_selection" ] || {
  echo "::error::no selection to compare with: run this Action without shadow-report earlier in the job"
  exit 1
}

while IFS= read -r report; do
  [ -n "$report" ] && report_arguments+=(--report "$report")
done <<< "$SHADOW_REPORT"

selection=$(cat "$last_selection")

prober shadow --selection "$selection" "${report_arguments[@]}" > "$(dirname "$selection")/shadow.json"
