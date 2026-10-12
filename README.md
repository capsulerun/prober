<div align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset=".github/images/logo-dark-prober.svg" />
    <source media="(prefers-color-scheme: light)" srcset=".github/images/logo-light-prober.svg" />
    <img alt="prober logo and name" src=".github/images/logo-light-prober.svg" width="50%"  />
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

Then, from the repository, pass the tests your change can affect, against the branch it targets, to your test runner:

```bash
prober --base origin/main | xargs -r npx vitest run
```

## Documentation

### Output and options

A selection has one of three modes: `selected` (the listed tests), `none` (the change reaches no test) and `all` (every test, with the reason). For Rust, the targets are crates, run with `cargo test -p <crate>`.

`--format json` gives the targets of each language apart, with the reasons and statistics, and `-0` separates the list with NUL characters:

```bash
prober --base origin/main --format json > selection.json
jq -r '.targets.python // [] | .[]' selection.json | xargs pytest
jq -r '.targets.rust // [] | .[].crate | "-p " + .' selection.json | xargs cargo test
prober --base origin/main -0 | xargs -0 npx vitest run
```

If the run fails, every test is selected; `--on-error fail` exits with an error instead. `PROBER_SELECTION=all` forces every test, for a release branch or an incident.

The history of the base branch is needed to find the merge base. Outside the GitHub Action, check out with enough history (`fetch-depth: 0` with `actions/checkout`).

`--output-dir <directory>` also writes the selection as `selection.json`, a Markdown summary as `summary.md`, and the targets of each language as `typescript.txt`, `python.txt` and `rust.txt`, for CI systems that pass files between steps.

### GitHub Action

`capsulerun/prober@v0` installs the binary of its own version, verifies its checksum and build provenance attestation, fetches the history it needs from a shallow checkout, selects, and writes a job summary.

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

To compare with a full run before relying on it, select first, run the whole suite, then compare:

```yaml
- uses: capsulerun/prober@v0
- run: npx vitest run --reporter=junit --outputFile=junit.xml
- uses: capsulerun/prober@v0
  with:
    shadow-report: junit.xml
```

### Understand a selection

```bash
prober why --base origin/main tests/test_parser.py
```

`why` explains why a test file (or crate) is selected for a change, hop by hop, or why it is not.

```bash
prober scan
```

`scan` reports what is inferred from the repository: the links it found, the dynamic imports it could not resolve and how many tests each one reaches, the files no test reaches, and the hubs most tests depend on. Run it once on a new repository to see where a line of configuration would help.

### Check it before relying on it

```bash
prober --base origin/main --format json > selection.json
npx vitest run --reporter=junit --outputFile=junit.xml
prober shadow --selection selection.json --report junit.xml
```

`shadow` compares a selection with the results of a full run: the failures it would have caught, those it would have missed, and what it would have saved. In GitHub Actions it also writes a job summary.

### Configuration (optional)

No configuration is needed. An optional `prober.toml` at the root of the repository covers what the code cannot tell, and corrects inference where it selects too much. A mistake in it fails the run with the line at fault; `prober scan` lists each rule with what it matched.

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
```

Paths are relative to the root of the repository. In patterns, `*` matches within one path segment, `**` across segments, and `{name}` captures one segment (or part of one) that the other side of the rule reuses.

#### `presets`

The test runners and frameworks whose conventions apply: test file names, setup files, config files and how their changes are diffed. `"auto"` (the default) detects them from the repository; a list forces or restricts them. Known presets: `cargo`, `cypress`, `django`, `jest`, `mocha`, `node-test`, `playwright`, `pytest`, `vite`, `vitest`.

#### `[always_all]`

`files`: patterns of files whose change runs every test. Use it for what cannot be reasoned about from the code, such as CI scripts or custom tooling. Each language already has a few (a toolchain file, `.cargo/config.toml`), while manifests, lockfiles and runner configs are diffed field by field instead.

#### `[always_run]`

`tests`: patterns of tests added to every selection that is not empty, such as a small smoke set. It only adds tests.

#### `[[link]]`

A dependency the code does not show: a change to a file matching `from` selects the tests that depend on the files matching `to`, or the tests matching `to` themselves. `to` is one pattern or a list. A `{name}` captured in `from` narrows `to` to the same value, so `plugins/{id}/**` links each plugin folder to its own test only.

#### `[[ignore]]`

Removes what inference got wrong. Two forms:
- `from` and `to`: drops the links where files matching `to` depend on files matching `from`. A warning says when this also drops imports written in the code.
- `site`: sets aside the dynamic imports and file reads of a file (`"src/loader.ts"`) or of one of its lines (`"src/loader.ts:42"`) that could not be resolved, so they no longer widen the selection. Pair it with a `[[link]]` that says what the site loads.

#### `[inference]`

Each inference can be switched off, to measure what it adds or when it misbehaves on a repository. All are on by default.

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
| `crates/lang-typescript`, `crates/lang-python`, `crates/lang-rust` | One crate per language: what its files depend on, its tests and its presets |
| `crates/cli` | The `prober` binary |

Each crate has unit tests per rule and tests that write small projects to disk and select on them. A change to inference comes with a test of the shape it handles, and with the shapes it must not change. The CI runs Prober on its own pull requests to select which crates to test.

Issues and pull requests are welcome. 

## License

Released under the [MIT License](LICENSE).
