#!/usr/bin/env bash
set -euo pipefail

missing_commit=0000000000000000000000000000000000000000
deepening_steps=(10 100 1000)
force_all=false

fetch_commit() {
  git cat-file -e "$1^{commit}" 2> /dev/null && return 0
  git fetch --quiet --no-tags --depth=1 origin "$1" 2> /dev/null || return 1
  [ "$(git rev-parse FETCH_HEAD)" = "$1" ] || git cat-file -e "$1^{commit}" 2> /dev/null
}

fetch_branch() {
  git fetch --quiet --no-tags --depth=1 origin "+refs/heads/$1:refs/remotes/origin/$1"
  git rev-parse "refs/remotes/origin/$1"
}

if [ -n "${INPUT_BASE:-}" ]; then
  requested=$INPUT_BASE
else
  case "${EVENT_NAME:-}" in
    pull_request | pull_request_target) requested=$PULL_REQUEST_BASE ;;
    merge_group) requested=$MERGE_GROUP_BASE ;;
    push)
      if [ -n "${PUSH_BEFORE:-}" ] && [ "$PUSH_BEFORE" != "$missing_commit" ]; then
        requested=$PUSH_BEFORE
      elif [ "${REF_NAME:-}" != "${DEFAULT_BRANCH:-}" ]; then
        requested=$DEFAULT_BRANCH
      else
        requested=""
        force_all=true
      fi
      ;;
    *) requested=${DEFAULT_BRANCH:-} ;;
  esac
fi

if [ "$force_all" = true ]; then
  echo "::notice::The default branch was pushed without a previous commit: every test runs"
  base=$(git rev-parse HEAD)
elif [[ "$requested" =~ ^[0-9a-f]{40}$ ]]; then
  if fetch_commit "$requested"; then
    base=$requested
  elif [ -n "${DEFAULT_BRANCH:-}" ]; then
    echo "::notice::The previous commit $requested is gone (force-push): comparing with $DEFAULT_BRANCH"
    base=$(fetch_branch "$DEFAULT_BRANCH")
  else
    echo "::error::cannot fetch the base $requested"
    exit 1
  fi
elif git rev-parse --verify --quiet "$requested^{commit}" > /dev/null \
  && ! git ls-remote --exit-code --heads origin "$requested" > /dev/null 2>&1; then
  base=$(git rev-parse "$requested^{commit}")
else
  base=$(fetch_branch "$requested")
fi

head=$(git rev-parse HEAD)

if [ "$force_all" != true ]; then
  for depth in "${deepening_steps[@]}"; do
    git merge-base "$base" "$head" > /dev/null 2>&1 && break
    [ "$(git rev-parse --is-shallow-repository)" = true ] || break
    git fetch --quiet --no-tags --deepen="$depth" origin "$base" "$head"
  done

  if ! git merge-base "$base" "$head" > /dev/null 2>&1 \
    && [ "$(git rev-parse --is-shallow-repository)" = true ]; then
    git fetch --quiet --no-tags --unshallow origin
  fi
fi

echo "base=$base" >> "$GITHUB_OUTPUT"
echo "force-all=$force_all" >> "$GITHUB_OUTPUT"
echo "Base: $base"
