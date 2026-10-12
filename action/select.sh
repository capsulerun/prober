#!/usr/bin/env bash
set -euo pipefail

temporary_directory=$RUNNER_TEMP

if command -v cygpath > /dev/null; then
  temporary_directory=$(cygpath -m "$RUNNER_TEMP")
fi

mkdir -p "$temporary_directory/prober"
output_directory=$(mktemp -d "$temporary_directory/prober/selection-XXXXXX")
read -r -a extra_arguments <<< "${EXTRA_ARGS:-}"

if [ "${FORCE_ALL:-false}" = true ]; then
  export PROBER_SELECTION=all
fi

prober --base "$BASE" --format list --on-error "$ON_ERROR" --output-dir "$output_directory" \
  ${extra_arguments[@]+"${extra_arguments[@]}"} > /dev/null

echo "$output_directory/selection.json" > "$temporary_directory/prober/last-selection"

{
  echo "mode=$(jq -r '.mode' "$output_directory/selection.json")"
  echo "mode-reasons=$(jq -c '[.mode_reasons // [] | .[].message]' "$output_directory/selection.json")"
  echo "json-file=$output_directory/selection.json"
  echo "typescript-file=$output_directory/typescript.txt"
  echo "python-file=$output_directory/python.txt"
  echo "rust-file=$output_directory/rust.txt"
} >> "$GITHUB_OUTPUT"

if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  cat "$output_directory/summary.md" >> "$GITHUB_STEP_SUMMARY"
fi
