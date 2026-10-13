#!/usr/bin/env bash
# Prints the samples to run as a JSON array: those reached by a selection written with --output-dir, or every sample without one.
set -euo pipefail

samples=$(cd "$(dirname "$0")" && pwd)
prefix=$(git -C "$samples" rev-parse --show-prefix)
selection=${1:-}
reached=()

reaches() {
  local sample=$1 manifest=$samples/$1/Cargo.toml crates

  case "$(jq -r .mode "$selection/selection.json")" in
    all) return 0 ;;
    none) return 1 ;;
  esac

  if grep -q "^$prefix$sample/" "$selection/typescript.txt" "$selection/python.txt"; then
    return 0
  fi

  [ -f "$manifest" ] || return 1
  crates=$(cargo metadata --no-deps --format-version 1 --manifest-path "$manifest" | jq -r '.packages[].name')
  grep -Fxq -f "$selection/rust.txt" <<< "$crates"
}

for directory in "$samples"/*/; do
  sample=$(basename "$directory")

  if [ -z "$selection" ] || reaches "$sample"; then
    reached+=("$sample")
  fi
done

if [ ${#reached[@]} -eq 0 ]; then
  echo '[]'
else
  printf '%s\n' "${reached[@]}" | jq -Rsc 'split("\n") | map(select(. != ""))'
fi
