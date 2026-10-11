use std::env;
use std::process::ExitCode;

use clap::Args;
use prober_core::files::RepoPath;
use prober_core::report::Mode;
use prober_core::why::{Explanation, explain};

use crate::pipeline::{self, OnError, Revisions, Selection};
use crate::{print_warnings, write_to_stdout};

#[derive(Debug, Args)]
pub struct WhyArguments {
    #[arg(help = "Test file, relative to the repository root")]
    test: String,

    #[arg(long, help = "Base revision, e.g. origin/main")]
    base: String,

    #[arg(
        long,
        default_value = "HEAD",
        help = "Head revision, which must be the checked-out commit"
    )]
    head: String,
}

pub fn run(arguments: &WhyArguments) -> ExitCode {
    let revisions = Revisions {
        base: arguments.base.clone(),
        head: arguments.head.clone(),
    };
    let selection = env::current_dir()
        .map_err(anyhow::Error::from)
        .and_then(|current_directory| pipeline::run(&current_directory, &revisions, OnError::Fail));
    let test = arguments.test.trim_start_matches("./");
    let test = selection
        .as_ref()
        .ok()
        .and_then(|selection| named_target(selection, test))
        .map_or_else(|| RepoPath::new(test), Ok);

    match (selection, test) {
        (Ok(selection), Ok(test)) => print_explanation(&selection, &test),
        (Err(error), _) => {
            eprintln!("prober why: {error:#}");

            ExitCode::FAILURE
        }
        (_, Err(error)) => {
            eprintln!("prober why: {error:#}");

            ExitCode::FAILURE
        }
    }
}

fn named_target(selection: &Selection, name: &str) -> Option<RepoPath> {
    let in_report = selection
        .report
        .targets
        .iter()
        .find(|target| {
            target
                .name
                .as_ref()
                .is_some_and(|target_name| target_name.name == name)
        })
        .map(|target| target.path.clone());

    in_report.or_else(|| {
        let graph = &selection.analysis.as_ref()?.graph;

        Some(graph.path(graph.target_named(name)?).clone())
    })
}

fn print_explanation(selection: &Selection, test: &RepoPath) -> ExitCode {
    print_warnings(selection);

    let report = &selection.report;
    let is_known_test = report.targets.iter().any(|target| target.path == *test);
    let text = match (&report.mode, &selection.analysis) {
        (Mode::All { reasons }, _) if is_known_test => {
            let messages: Vec<String> = reasons.iter().map(|reason| reason.message()).collect();

            format!("{test} runs with every test: {}\n", messages.join("; "))
        }
        (_, Some(analysis)) => {
            let explanation = explain(
                &analysis.graph,
                &analysis.changes,
                test,
                analysis.widening,
                &analysis.always_run,
            );

            if explanation == Explanation::NotATest {
                eprintln!("prober why: {test} is not a test file");

                return ExitCode::FAILURE;
            }

            explanation.to_text(test)
        }
        _ => {
            eprintln!("prober why: {test} is not a test file");

            return ExitCode::FAILURE;
        }
    };

    write_to_stdout(&text)
}
