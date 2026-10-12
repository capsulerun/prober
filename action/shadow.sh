#!/usr/bin/env bash
set -euo pipefail

temporary_directory=$RUNNER_TEMP

if command -v cygpath > /dev/null; then
  temporary_directory=$(cygpath -m "$RUNNER_TEMP")
fi

selection="$temporary_directory/prober/selection.json"
report_arguments=()

[ -f "$selection" ] || {
  echo "::error::no selection to compare with: run this Action without shadow-report earlier in the job"
  exit 1
}

while IFS= read -r report; do
  [ -n "$report" ] && report_arguments+=(--report "$report")
done <<< "$SHADOW_REPORT"

prober shadow --selection "$selection" "${report_arguments[@]}" > "$temporary_directory/prober/shadow.json"
