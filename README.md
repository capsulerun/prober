<div align="center">

<h1>Prober</h1>

Run only the tests your change can affect.

[![CI](https://github.com/capsulerun/prober/actions/workflows/ci.yml/badge.svg)](https://github.com/capsulerun/prober/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/capsulerun/prober?include_prereleases&sort=semver)](https://github.com/capsulerun/prober/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

</div>



Most pipelines run the whole test suite on every change, even when it touches a few files, so CI time and cost grow with every change rather than with what it changes. Prober reads the code of both revisions and builds the graph of what depends on what: imports, modules loaded by name, files read from disk, fixtures, manifests, lockfiles and test runner configs. It outputs the tests a change can affect, each with the reason it was selected.

It never runs your tests and keeps no state: the same change always gives the same selection. When it cannot be sure, it selects more, never less, and says why.

Prober supports TypeScript/JavaScript (Vitest, Jest, Mocha, `node:test`, Playwright, Cypress, Vite), Python (pytest, Django) and Rust (Cargo workspaces), in one run when a repository mixes them.

## Getting started

### GitHub Actions

```yaml
- uses: actions/checkout@v7
- uses: capsulerun/prober@v1
  id: prober
- if: steps.prober.outputs.mode != 'none'
  run: |
    if [ "${{ steps.prober.outputs.mode }}" = "all" ]; then npx vitest run
    else xargs -a "${{ steps.prober.outputs.typescript-file }}" npx vitest run; fi
```

### Run it locally

#### Install

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/capsulerun/prober/releases/latest/download/prober-installer.sh | sh
```

> <details>
> 
> <summary>On Windows</summary>
> 
> ```powershell
> powershell -ExecutionPolicy Bypass -c "irm https://github.com/capsulerun/prober/releases/latest/download/prober-installer.ps1 | iex"
> ```
> 
> </details>

To pin a version, replace `latest/download` with `download/v0.1.0`.

#### Usage

From the repository, against the branch your change targets:

```bash
prober --base origin/main
```

Prober prints the selected test files, one per line, and a summary:

```
prober: selected 12/840 test files (1.4%)
```

Pass them to your test runner:

```bash
prober --base origin/main | xargs npx vitest run
```

## Documentation

### Selections

A selection has one of three modes: `selected` (the listed tests), `none` (the change reaches no test) and `all` (every test, with the reason). For Rust, the targets are crates, run with `cargo test -p <crate>`.

`--format json` gives the targets of each language apart, with the reasons and statistics, and `-0` separates the list with NUL characters:

```bash
prober --base origin/main --format json > selection.json
jq -r '.targets.python // [] | .[]' selection.json | xargs pytest
jq -r '.targets.rust // [] | .[].crate | "-p " + .' selection.json | xargs cargo test
prober --base origin/main -0 | xargs -0 npx vitest run
```

If Prober fails, it selects every test; `--on-error fail` makes it exit with an error instead. `PROBER_SELECTION=all` forces every test, for a release branch or an incident.

Prober needs the history of the base branch to find the merge base. Outside the GitHub Action, check out with enough history (`fetch-depth: 0` with `actions/checkout`).

### Understand a selection

```bash
prober why --base origin/main tests/test_parser.py
```

`why` explains why a test file (or crate) is selected for a change, hop by hop, or why it is not.

```bash
prober scan
```

`scan` reports what Prober infers from the repository: the links it found, the dynamic imports it could not resolve and how many tests each one reaches, the files no test reaches, and the hubs most tests depend on. Run it once on a new repository to see where a line of configuration would help.

### Check it before relying on it

```bash
prober --base origin/main --format json > selection.json
npx vitest run --reporter=junit --outputFile=junit.xml
prober shadow --selection selection.json --report junit.xml
```

`shadow` compares a selection with the results of a full run: the failures it would have caught, those it would have missed, and what it would have saved. In GitHub Actions it also writes a job summary.

### Configuration (optional)

Prober needs no configuration. An optional `prober.toml` at the root of the repository covers what the code cannot tell, and corrects inference where it selects too much. A mistake in it fails the run with the line at fault; `prober scan` lists each rule with what it matched.

```toml
presets = "auto"

[always_all]
files = ["ci/**", "scripts/release/**"]

[always_run]
tests = ["test/smoke/config.test.ts"]

[[link]]
from = "fixtures/golden/**"
to = "test/golden.e2e.test.ts"

[[link]]
from = "plugins/{id}/**"
to = ["test/plugins/{id}.test.ts", "test/registry.test.ts"]

[[ignore]]
from = "src/shared/logger.ts"
to = "test/**/*.e2e.test.ts"

[[ignore]]
site = "src/loader.ts:42"

[inference]
widening = true
```

Paths are relative to the root of the repository. In patterns, `*` matches within one path segment, `**` across segments, and `{name}` captures one segment (or part of one) that the other side of the rule reuses.

#### `presets`

The test runners and frameworks whose conventions Prober applies: test file names, setup files, config files and how their changes are diffed. `"auto"` (the default) detects them from the repository; a list forces or restricts them. Known presets: `cargo`, `cypress`, `django`, `jest`, `mocha`, `node-test`, `playwright`, `pytest`, `vite`, `vitest`.

#### `[always_all]`

`files`: patterns of files whose change runs every test. Use it for what Prober cannot reason about, such as CI scripts or custom tooling. Each language already has a few (a toolchain file, `.cargo/config.toml`), while manifests, lockfiles and runner configs are diffed field by field instead.

#### `[always_run]`

`tests`: patterns of tests added to every selection that is not empty, such as a small smoke set. It only adds tests.

#### `[[link]]`

A dependency the code does not show: a change to a file matching `from` selects the tests that depend on the files matching `to`, or the tests matching `to` themselves. `to` is one pattern or a list. A `{name}` captured in `from` narrows `to` to the same value, so `plugins/{id}/**` links each plugin folder to its own test only.

#### `[[ignore]]`

Removes what inference got wrong. Two forms:
- `from` and `to`: drops the links where files matching `to` depend on files matching `from`. Prober warns when this also drops imports written in the code.
- `site`: sets aside the dynamic imports and file reads of a file (`"src/loader.ts"`) or of one of its lines (`"src/loader.ts:42"`) that Prober could not resolve, so they no longer widen the selection. Pair it with a `[[link]]` that says what the site loads.

#### `[inference]`

Each inference can be switched off, to measure what it adds or when it misbehaves on a repository. All are on by default.

| Field | What it infers |
|---|---|
| `dynamic_imports` | Computed imports (``import(`./locales/${lang}.js`)``, `importlib.import_module(f"app.{name}")`) resolved to the modules they can load |
| `plugin_folders` | Folders of plugins loaded by name, each linked to the code that loads it |
| `widening` | Tests that reach a site Prober could not resolve are selected when a change could reach that site |
| `test_support` | Helpers, fixtures and setup files used by tests, which select only the tests using them |
| `manifests` | Manifests and lockfiles diffed field by field, instead of running every test |
| `files` | Files read from literal paths, such as fixtures, snapshots and `include_str!` |
| `naming_families` | Tests paired with the modules they are named after |
| `spawned_entrypoints` | Scripts and binaries that tests start as a process |
| `build_mapping` | Build outputs (`dist/…`) mapped back to their sources |
| `symbols` | For TypeScript, links followed through the functions and classes a test uses rather than whole files |

## Contributing

Prober is written in Rust. The toolchain is pinned in `rust-toolchain.toml`, so `rustup` picks it up.

```bash
cargo build
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

The workspace has one crate per part:

| Crate | Role |
|---|---|
| `crates/core` | The graph, the selection, the config, the reports and `shadow` |
| `crates/vcs` | Reading both revisions and the change from git |
| `crates/lang-typescript`, `crates/lang-python`, `crates/lang-rust` | One frontend per language: what its files depend on, its tests and its presets |
| `crates/cli` | The `prober` binary |

Each frontend has unit tests per rule and tests that write small projects to disk and select on them. A change to inference comes with a test of the shape it handles, and with the shapes it must not change. Prober's CI runs Prober on its own pull requests to select which crates to test.

Issues and pull requests are welcome. Prober is released under the [MIT License](LICENSE).
