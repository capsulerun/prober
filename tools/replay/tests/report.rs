use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

fn selection(
    mode: &str,
    targets: &[&str],
    distances: &[(&str, u64)],
    selected_ratio: f64,
) -> String {
    let targets: Vec<String> = targets
        .iter()
        .map(|target| format!("\"{target}\""))
        .collect();
    let distances: Vec<String> = distances
        .iter()
        .map(|(file, distance)| format!("\"{file}\":{distance}"))
        .collect();
    let mode_reasons = match mode {
        "all" => {
            r#"[{"kind":"always_all","file":"vitest.config.ts","pattern":"**/vitest.*","message":"vitest.config.ts (modified) matches always_all \"**/vitest.*\""}]"#
        }
        "none" => r#"[{"kind":"no_impact","message":"1 changed file reaches no test file"}]"#,
        _ => "null",
    };

    format!(
        r#"{{"schema_version":1,"revisions":null,"mode":"{mode}","mode_reasons":{mode_reasons},"targets":{{"typescript":[{}]}},"reasons":{{}},"distances":{{{}}},"stats":{{"tests_total":10,"tests_selected":{},"selected_ratio":{selected_ratio}}}}}"#,
        targets.join(","),
        distances.join(","),
        targets.len()
    )
}

fn record(id: &str, seconds: f64, selection: Option<String>, error: Option<&str>) -> String {
    format!(
        r#"{{"id":"{id}","base":"main","head":"{id}","error":{},"seconds":{seconds},"exit_code":0,"selection":{},"stderr":""}}"#,
        error.map_or_else(|| "null".to_owned(), |error| format!("\"{error}\"")),
        selection.unwrap_or_else(|| "null".to_owned())
    )
}

fn write_lines(path: &Path, lines: &[String]) {
    fs::write(path, lines.join("\n") + "\n").unwrap();
}

#[test]
fn report_gives_savings_on_merges_and_misses_on_failed_changes() {
    let directory = TempDir::new().unwrap();
    let merges = directory.path().join("merges.jsonl");
    let failed_changes = directory.path().join("failed-changes.jsonl");
    let failures = directory.path().join("failures.jsonl");

    write_lines(
        &merges,
        &[
            record(
                "m1",
                1.0,
                Some(selection(
                    "selected",
                    &["src/a.test.ts"],
                    &[("src/a.test.ts", 1)],
                    0.1,
                )),
                None,
            ),
            record(
                "m2",
                0.1,
                Some(selection(
                    "all",
                    &["src/a.test.ts", "src/b.test.ts"],
                    &[],
                    1.0,
                )),
                None,
            ),
            record("m3", 0.9, Some(selection("none", &[], &[], 0.0)), None),
            record("m4", 0.0, None, Some("git checkout failed")),
        ],
    );
    write_lines(
        &failed_changes,
        &[
            record(
                "c1",
                1.0,
                Some(selection(
                    "selected",
                    &["src/a.test.ts", "src/b.test.ts", "src/new.test.ts"],
                    &[
                        ("src/a.test.ts", 1),
                        ("src/b.test.ts", 3),
                        ("src/new.test.ts", 0),
                    ],
                    0.2,
                )),
                None,
            ),
            record(
                "c2",
                1.0,
                Some(selection(
                    "selected",
                    &["src/x.test.ts"],
                    &[("src/x.test.ts", 2)],
                    0.1,
                )),
                None,
            ),
            record(
                "c3",
                1.0,
                Some(selection(
                    "all",
                    &["src/a.test.ts", "src/z.test.ts"],
                    &[],
                    1.0,
                )),
                None,
            ),
            record(
                "c4",
                0.0,
                None,
                Some("git fetch --quiet origin c4 failed: not our ref"),
            ),
        ],
    );
    write_lines(
        &failures,
        &[
            r#"{"change":"c1","file":"src/a.test.ts","runs":[1],"pre_existing":false}"#.to_owned(),
            r#"{"change":"c1","file":"src/b.test.ts","runs":[1],"pre_existing":true}"#.to_owned(),
            r#"{"change":"c2","file":"src/x.test.ts","runs":[2],"pre_existing":false}"#.to_owned(),
            r#"{"change":"c2","file":"src/y.test.ts","runs":[2],"pre_existing":false}"#.to_owned(),
            r#"{"change":"c3","file":"src/z.test.ts","runs":[3],"pre_existing":false}"#.to_owned(),
            r#"{"change":"c4","file":"src/q.test.ts","runs":[4],"pre_existing":false}"#.to_owned(),
            r#"{"change":"c2","file":"web/src/elsewhere.test.ts","runs":[2],"pre_existing":false,"in_repository":false}"#.to_owned(),
            r#"{"change":"c1","file":"src/new.test.ts","runs":[1],"pre_existing":false,"in_repository":false}"#.to_owned(),
        ],
    );

    let output = Command::new(env!("CARGO_BIN_EXE_prober-replay"))
        .args(["report", "--title", "acme/app", "--merges"])
        .arg(&merges)
        .arg("--failed-changes")
        .arg(&failed_changes)
        .arg("--failures")
        .arg(&failures)
        .output()
        .unwrap();

    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "prober-replay: report for 4 merged changes and 4 failed changes with 1 misses\n"
    );
    assert_eq!(String::from_utf8(output.stdout).unwrap(), EXPECTED_REPORT);
}

const EXPECTED_REPORT: &str = "# Prober benchmark: acme/app

## Savings on 4 merged changes

| | |
| --- | --- |
| Replayed | 3 (1 failed to replay) |
| Mode | 1 selected, 1 all, 1 none |
| Test files selected | median 10.0%, mean 36.7%, 90th percentile 100.0% |
| Time per change | median 0.9 s, 90th percentile 1.0 s |

### Why every test ran

| Cause | Changes |
| --- | --- |
| `always_all` `**/vitest.*` | 1 |

## Misses on 4 failed pull request changes

| | |
| --- | --- |
| Replayed | 3 (1 could not be replayed) |
| Failed test files | 5 counted, 1 pre-existing on the base branch (not counted), 1 not found in the repository (not counted), 1 on changes that could not be replayed |
| Selected | 4 |
| **Misses** | **1** (0 on tests that failed on 3 or more changes, likely flaky) |
| Test files selected | median 20.0%, mean 43.3% |

### Distance of selected failures

| Distance | Failed test files |
| --- | --- |
| 0 | 1 |
| 1 | 1 |
| 2 | 1 |
| every test ran | 1 |

### Misses

| Change | Test file | Selection | Same test failed on |
| --- | --- | --- | --- |
| `c2` | `src/y.test.ts` | selected, 1 test file | 1 change |

### Changes that could not be replayed

| Change | Error |
| --- | --- |
| `c4` | git fetch --quiet origin c4 failed: not our ref |
";
