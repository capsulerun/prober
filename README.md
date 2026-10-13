<div align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset=".github/images/logo-dark-prober.svg" />
    <source media="(prefers-color-scheme: light)" srcset=".github/images/logo-light-prober.svg" />
    <img alt="prober logo and name" src=".github/images/logo-light-prober.svg" width="25%"  />
  </picture>
</div>

<div align="center">
  <h3>Run only the tests your change can affect</h3>
</div>

<div align="center">

[![CI](https://github.com/capsulerun/prober/actions/workflows/ci.yml/badge.svg)](https://github.com/capsulerun/prober/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/capsulerun/prober?include_prereleases&sort=semver)](https://github.com/capsulerun/prober/releases)

</div>


Most pipelines run the whole test suite on every change, even when it touches a few files, so CI time and cost grow with every change rather than with what it changes.

Prober outputs the tests a change can affect, each with the reason it was selected. It never runs your tests and keeps no state: the same change always gives the same selection. When it cannot be sure, it selects more, never less, and says why.


> [!NOTE]
> Works with TypeScript/JavaScript (Vitest, Jest, Mocha, `node:test`, Playwright, Cypress, Vite), Python (pytest, Django) and Rust (Cargo workspaces), in one run when a repository mixes them.

## Getting started

### In GitHub Actions

```yaml
- uses: actions/checkout@v7
- uses: capsulerun/prober@v0
  id: prober
- run: xargs -r npx vitest run < "$TESTS"
  env:
    TESTS: ${{ steps.prober.outputs.typescript-file }}
```

The selected test files are written to `typescript-file`.

### Run it locally

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/capsulerun/prober/releases/latest/download/prober-installer.sh | sh
```

> <details>
> 
> <summary>Windows install</summary>
> 
> ```powershell
> powershell -ExecutionPolicy Bypass -c "irm https://github.com/capsulerun/prober/releases/latest/download/prober-installer.ps1 | iex"
> ```
> 
> </details>

> [!IMPORTANT]
> To pin a version, replace `latest/download` with `download/v0.1.0`.

Then, from the repository, pass the tests your change can affect against the branch it targets, to your test runner:

```bash
prober --base origin/main | xargs -r npx vitest run
```

## Documentation

### Output and options

Every run ends in one of three modes:
- `selected`: only the listed tests run
- `none`: the change reaches no test, so nothing runs
- `all`: every test runs, and the output says why

By default, Prober prints one test file per line, ready for `xargs`. For Rust, each line is a crate, to run with `cargo test -p <crate>`. When a repository mixes languages, `--format json` keeps the targets of each language apart, with the reasons each test was selected and some statistics. `-0` separates the list with NUL characters instead of newlines.

```bash
prober --base origin/main --format json > selection.json
jq -r '.targets.python // [] | .[]' selection.json | xargs pytest
jq -r '.targets.rust // [] | .[].crate | "-p " + .' selection.json | xargs cargo test
prober --base origin/main -0 | xargs -0 npx vitest run
```

For CI systems that pass files between steps, `--output-dir <directory>` also writes the selection (`selection.json`), a Markdown summary (`summary.md`) and one list per language (`typescript.txt`, `python.txt`, `rust.txt`).

Prober needs the history of the base branch to find where the change started. Outside the GitHub Action, check out with enough history (`fetch-depth: 0` with `actions/checkout`). If the selection fails anyway, every test runs; `--on-error fail` exits with an error instead. To run every test on purpose, for a release branch or during an incident, set `PROBER_SELECTION=all`.

### Understand a selection

`why` explains why a test file (or crate) is selected, hop by hop from the change, or why it is not:

```bash
prober why --base origin/main tests/test_parser.py
```

`scan` shows what Prober understands of the whole repository: the links it inferred, the dynamic imports it could not resolve and how many tests each one reaches, the files no test reaches, and the hubs most tests depend on. Run it once on a new repository to see where a line of configuration would help:

```bash
prober scan
```

### Check it before relying on it

Before skipping tests for real, compare the selection with a full run. `shadow` reads both and reports the failures the selection would have caught, those it would have missed, and the time it would have saved:

```bash
prober --base origin/main --format json > selection.json
npx vitest run --reporter=junit --outputFile=junit.xml
prober shadow --selection selection.json --report junit.xml
```

In GitHub Actions, select first, run the whole suite, then hand the report to the Action, which adds the comparison to the job summary:

```yaml
- uses: capsulerun/prober@v0
- run: npx vitest run --reporter=junit --outputFile=junit.xml
- uses: capsulerun/prober@v0
  with:
    shadow-report: junit.xml
```

Once many runs are compared, `prober shadow --aggregate <files>` reads their results together and recommends values for the [aggressive options](#aggressive-defer-and-protected).

### GitHub Action

`capsulerun/prober@v0` installs the Prober binary of its own version and verifies its checksum and build provenance attestation. It then fetches only the history it needs, even from a shallow checkout, selects the tests and writes a job summary.

| Input | Default | |
|---|---|---|
| `base` | the pull request's base, the previous commit for a push | Revision to compare with |
| `working-directory` | `.` | Directory of the repository |
| `on-error` | `all` | `all` runs every test when the selection fails, `fail` fails the step |
| `args` | | Extra arguments for `prober` |
| `version` | the Action's version | Prober version to install |
| `verify-attestation` | `true` | `false` checks the checksum only, for runners without `gh` |
| `shadow-report` | | JUnit reports of a full run, one per line: compares them with the latest selection made earlier in the job |

| Output | |
|---|---|
| `mode` | `selected`, `all` or `none` |
| `mode-reasons` | Why every test or no test runs, as a JSON array |
| `typescript-file`, `python-file`, `rust-file` | The tests (or crates) to run, one per line |
| `json-file` | The selection with the reasons of each test |
| `shadow-file` | With `shadow-report`, the comparison as JSON |

### Configuration (optional)

Most repositories need no configuration. When the code alone cannot tell which tests a change affects, or when inference selects too much, add a `prober.toml` at the root of the repository. A mistake in it fails the run and names the line at fault, and `prober scan` lists each rule with what it matched.

```toml
# Detected from the repository; list presets only to force or restrict them
presets = "auto"

# Changes that cannot be reasoned about: run every test
[always_all]
files = ["ci/**", "scripts/release/**"]

# A smoke set added to every selection
[always_run]
tests = ["test/smoke/config.test.ts"]

# A test reads fixtures through a path the code does not show
[[link]]
from = "fixtures/golden/**"
to = "test/golden.e2e.test.ts"

# Plugins are loaded by name: each folder reaches its own test and the registry's
[[link]]
from = "plugins/{id}/**"
to = ["test/plugins/{id}.test.ts", "test/registry.test.ts"]

# An inferred link that selects too much
[[ignore]]
from = "src/shared/logger.ts"
to = "test/**/*.e2e.test.ts"

# A dynamic import that cannot be resolved, now covered by the links above
[[ignore]]
site = "src/loader.ts:42"

# Every inference is on by default; switch one off if it misbehaves
[inference]
widening = true

# Opt-in: trade a little safety for savings, with a full run on main as safety net
[aggressive]
max_distance = 6
hub_fanout_limit = 50

# Slow suites run only when the change is close
[[defer]]
tests = "test/**/*.integration.test.ts"
max_distance = 1

# Tests no aggressive option drops
[protected]
tests = ["test/session-lifecycle.regression.test.ts"]
```

Paths are relative to the root of the repository. In patterns, `*` matches within one path segment, `**` across segments, and `{name}` captures one segment (or part of one) that the other side of the rule reuses.

#### `presets`

The test runners and frameworks whose conventions apply: test file names, setup files, config files and how their changes are compared. `"auto"`, the default, detects them from the repository; a list forces or restricts them. Known presets: `cargo`, `cypress`, `django`, `jest`, `mocha`, `node-test`, `playwright`, `pytest`, `vite`, `vitest`.

#### `[always_all]`

`files`: patterns of files whose change runs every test, for what cannot be reasoned about from the code, such as CI scripts or custom tooling. Each language already has a few built in (a toolchain file, `.cargo/config.toml`). Manifests, lockfiles and runner configs need none: their changes are compared field by field.

#### `[always_run]`

`tests`: patterns of tests added to every selection that is not empty, such as a small smoke set. It only ever adds tests.

#### `[[link]]`

A dependency the code does not show. When a file matching `from` changes, the tests that depend on the files matching `to` are selected, or the tests matching `to` themselves. `to` is one pattern or a list. A `{name}` captured in `from` narrows `to` to the same value, so `plugins/{id}/**` links each plugin folder to its own test only.

#### `[[ignore]]`

Removes what inference got wrong, in one of two forms:
- `from` and `to`: drops the links where files matching `to` depend on files matching `from`. A warning says when this also drops imports written in the code.
- `site`: sets aside the dynamic imports and file reads that could not be resolved in a file (`"src/loader.ts"`) or on one of its lines (`"src/loader.ts:42"`), so they no longer widen the selection. Pair it with a `[[link]]` that says what the site loads.

#### `[inference]`

Every inference is on by default. Switch one off to measure what it adds, or when it misbehaves on a repository.

| Field | What it infers |
|---|---|
| `dynamic_imports` | Computed imports (``import(`./locales/${lang}.js`)``, `importlib.import_module(f"app.{name}")`) resolved to the modules they can load |
| `plugin_folders` | Folders of plugins loaded by name, each linked to the code that loads it |
| `widening` | Tests that reach an unresolved site are selected when a change could reach that site |
| `test_support` | Helpers, fixtures and setup files used by tests, which select only the tests using them |
| `manifests` | Manifests and lockfiles diffed field by field, instead of running every test |
| `files` | Files read from literal paths, such as fixtures, snapshots and `include_str!` |
| `naming_families` | Tests paired with the modules they are named after |
| `spawned_entrypoints` | Scripts and binaries that tests start as a process |
| `build_mapping` | Build outputs (`dist/…`) mapped back to their sources |
| `symbols` | For TypeScript, links followed through the functions and classes a test uses rather than whole files |

#### `[aggressive]`, `[[defer]]` and `[protected]`

Off by default, these options drop some of the tests a change can reach, for more savings. Use them with a periodic full run on the main branch, which catches what they let through, and with values from `prober shadow --aggregate`.

- `max_distance` in `[aggressive]`: drops the tests more than this many files away from every change. A test importing a changed file is at distance 1.
- `hub_fanout_limit` in `[aggressive]`: past a file that more than this many files depend on, keeps only its direct dependents. A test is dropped only when every path to it goes past such a hub.
- `[[defer]]`: the `tests` it names (one pattern or a list) run only within its `max_distance` of a change, for slow suites such as integration or browser tests. Several `[[defer]]` tables can set different distances.
- `[protected]`: `tests` that these options never drop, such as tests that caught real regressions. Tests that mock a module whose exports changed are protected too.

Dropped tests are listed under `dropped` in the JSON and in the summary, and `prober why` names the option that dropped them.

## Contributing

The toolchain is pinned in `rust-toolchain.toml`, so `rustup` picks it up.

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
| `crates/lang-typescript`, `crates/lang-python`, `crates/lang-rust` | One crate per language: what its files depend on, its tests and its presets |
| `crates/cli` | The `prober` binary |

Each crate has unit tests per rule and tests that write small projects to disk and select on them. A change to inference comes with a test of the shape it handles, and with the shapes it must not change. The CI runs Prober on its own pull requests to select which crates to test.

[`tests/samples`](tests/samples) holds small sample repositories, one per language and one mixing them, that check Prober end to end with real test runners. Prober picks them for its own CI through its

Issues and pull requests are welcome. 

## License

Released under the [MIT License](LICENSE).
