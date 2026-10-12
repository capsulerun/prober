#!/usr/bin/env bash
set -euo pipefail

action_directory=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
failures=0

file_url() {
  if command -v cygpath > /dev/null; then
    echo "file:///$(cygpath -m "$1")"
  else
    echo "file://$1"
  fi
}

commit() {
  git -c user.name=test -c user.email=test@example.com commit --quiet "$@"
}

git init --quiet --initial-branch=main "$work/origin"
cd "$work/origin"

for number in $(seq 1 30); do
  echo "$number" > main.txt
  git add main.txt
  commit --message "main $number"
done

git switch --quiet --create feature

for number in 1 2 3; do
  echo "$number" > feature.txt
  git add feature.txt
  commit --message "feature $number"
done

git switch --quiet main
echo moved > moved.txt
git add moved.txt
commit --message "main moves on"

main=$(git rev-parse main)
fork_point=$(git merge-base main feature)
before=$(git rev-parse feature~2)

check() {
  local name=$1 expected_base=$2 expected_force_all=$3
  shift 3

  rm -rf "$work/clone"
  git clone --quiet --depth 1 --branch feature "$(file_url "$work/origin")" "$work/clone"
  : > "$work/output"
  (cd "$work/clone" && env GITHUB_OUTPUT="$work/output" "$@" bash "$action_directory/base.sh" > /dev/null)

  local base force_all merge_base
  base=$(sed -n 's/^base=//p' "$work/output")
  force_all=$(sed -n 's/^force-all=//p' "$work/output")
  merge_base=$(git -C "$work/clone" merge-base "$base" HEAD 2> /dev/null || true)

  if [ "$base" != "$expected_base" ] || [ "$force_all" != "$expected_force_all" ] \
    || { [ "$force_all" = false ] && [ -z "$merge_base" ]; }; then
    echo "FAIL $name: base $base (expected $expected_base), force-all $force_all, merge base '$merge_base'"
    failures=$((failures + 1))
  else
    echo "ok   $name: base ${base:0:8}, merge base ${merge_base:0:8}, $(git -C "$work/clone" rev-list --count HEAD) commits fetched"
  fi
}

check "pull request" "$main" false EVENT_NAME=pull_request PULL_REQUEST_BASE="$main"
check "push" "$before" false EVENT_NAME=push PUSH_BEFORE="$before" REF_NAME=feature DEFAULT_BRANCH=main
check "new branch" "$main" false EVENT_NAME=push PUSH_BEFORE=0000000000000000000000000000000000000000 REF_NAME=feature DEFAULT_BRANCH=main
check "force-push" "$main" false EVENT_NAME=push PUSH_BEFORE=1234567890123456789012345678901234567890 REF_NAME=feature DEFAULT_BRANCH=main
check "base input" "$main" false INPUT_BASE=main
check "default branch first push" "$(git -C "$work/origin" rev-parse feature)" true EVENT_NAME=push PUSH_BEFORE=0000000000000000000000000000000000000000 REF_NAME=main DEFAULT_BRANCH=main

echo "fork point ${fork_point:0:8}"
rm -rf "$work"
[ "$failures" -eq 0 ]
