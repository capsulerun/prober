use prober_core::junit::looks_like_test_file;

const ESCAPE: char = '\u{1b}';

pub fn failed_test_file_paths(job_log: &str) -> Vec<String> {
    job_log
        .lines()
        .filter_map(|log_line| failed_test_file_path(&without_ansi_codes(log_line)))
        .collect()
}

fn failed_test_file_path(log_line: &str) -> Option<String> {
    let mut words = log_line.split_whitespace();

    words.by_ref().find(|word| *word == "FAIL")?;

    words
        .find(|word| looks_like_test_file(word))
        .map(str::to_owned)
}

fn without_ansi_codes(text: &str) -> String {
    let mut plain_text = String::with_capacity(text.len());
    let mut characters = text.chars();

    while let Some(character) = characters.next() {
        if character == ESCAPE {
            characters.by_ref().find(char::is_ascii_alphabetic);

            continue;
        }

        plain_text.push(character);
    }

    plain_text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vitest_and_jest_failure_lines_give_their_test_file() {
        let job_log = [
            "2026-10-08T09:04:19.9093150Z [shard:unit-2] \u{1b}[41m\u{1b}[1m FAIL \u{1b}[22m\u{1b}[49m \u{1b}[30m\u{1b}[45m gateway (gateway-parallel) \u{1b}[49m\u{1b}[39m src/gateway/probe.test.ts > probe > answers",
            "2026-10-08T09:04:19.9100000Z AssertionError: expected 1 to be 2",
            "2026-10-08T09:04:20.0000000Z FAIL src/math.test.js (5.2 s)",
            "2026-10-08T09:04:21.0000000Z  FAIL  src/broken.test.ts [ src/broken.test.ts ]",
        ]
        .join("\n");

        assert_eq!(
            failed_test_file_paths(&job_log),
            [
                "src/gateway/probe.test.ts",
                "src/math.test.js",
                "src/broken.test.ts"
            ]
        );
    }

    #[test]
    fn lookalike_lines_are_ignored() {
        let job_log = [
            "2026-10-08T09:01:54.5299102Z   CONTINUE_ON_FAILURE: 1",
            "2026-10-08T09:01:55.0000000Z FAIL_FAST=1 src/env.test.ts",
            "2026-10-08T09:01:56.0000000Z FAIL scripts/build.sh",
        ]
        .join("\n");

        assert_eq!(failed_test_file_paths(&job_log), Vec::<String>::new());
    }
}
