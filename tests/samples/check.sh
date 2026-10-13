#!/usr/bin/env bash
# Runs a sample in a fresh repository: its change.sh selects the targets of its expected.txt, the runners run exactly them, and prober shadow reads their JUnit reports.
set -euo pipefail

sample=$1
samples=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
repository=$work/repository
selection=$work/selection
languages=()

commit() {
  git -c user.name=prober -c user.email=prober@example.com commit --quiet "$@"
}

run_typescript() {
  local report=$1/vitest.xml

  if [ -n "${2:-}" ]; then
    xargs -r npx vitest run --reporter=junit --outputFile="$report" < "$2"
  else
    npx vitest run --reporter=junit --outputFile="$report"
  fi
}

run_python() {
  local report=$1/pytest.xml

  if [ -n "${2:-}" ]; then
    xargs -r python3 -m pytest --junitxml="$report" < "$2"
  else
    python3 -m pytest --junitxml="$report"
  fi
}

run_rust() {
  if [ -n "${2:-}" ]; then
    sed 's/^/-p /' "$2" | xargs -r cargo nextest run --profile ci --no-fail-fast
  else
    cargo nextest run --profile ci --no-fail-fast
  fi

  cp target/nextest/ci/junit.xml "$1/nextest.xml"
}

run_tests() {
  local reports=$1 selected=${2:-}

  mkdir -p "$reports"

  for language in "${languages[@]}"; do
    "run_$language" "$reports" "${selected:+$selection/$language.txt}" || true
  done
}

compare() {
  local reports=$1 condition=$2 report_arguments=() shadow_json

  for report in "$reports"/*.xml; do
    report_arguments+=(--report "$report")
  done

  shadow_json=$(prober shadow --selection "$selection/selection.json" "${report_arguments[@]}")

  if ! jq -e "$condition" <<< "$shadow_json" > /dev/null; then
    echo "$shadow_json"
    echo "$sample: the comparison with $(basename "$reports") does not hold: $condition"
    exit 1
  fi
}

cp -R "$samples/$sample" "$repository"
cd "$repository"
mv change.sh expected.txt "$work/"

if [ -f package.json ]; then
  languages+=(typescript)
fi

if [ -f pyproject.toml ]; then
  languages+=(python)
fi

if [ -f Cargo.toml ]; then
  languages+=(rust)
fi

git init --quiet --initial-branch=main
git add .
commit --message sample
git switch --quiet --create change
bash "$work/change.sh"
commit --all --message change

if [ -f package.json ]; then
  npm install --no-audit --no-fund --silent
fi

prober --base main --output-dir "$selection" > /dev/null

if [ "$(jq -r .mode "$selection/selection.json")" != selected ] \
  || ! diff <(cat "$selection"/*.txt | sort) <(sort "$work/expected.txt"); then
  echo "$sample: the change does not select the expected targets"
  exit 1
fi

run_tests "$work/selected" selected
compare "$work/selected" '.test_files.reported == .test_files.selected
  and .test_files.selected_but_not_reported == 0
  and (.failures | length) > 0 and (.unattributed_failures | length) == 0'

run_tests "$work/everything"
compare "$work/everything" '.test_files.skipped > 0
  and (.failures | length) > 0 and all(.failures[]; .selected)
  and (.misses | length) == 0 and (.unattributed_failures | length) == 0'

echo "$sample: selects $(paste -sd ' ' "$work/expected.txt"), run and compared with the full run"
rm -r "$work"
