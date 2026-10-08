use std::fs;
use std::path::Path;
use std::process::Command;

use prober_core::changes::{ChangeKind, FileChange, ResolvedRevisions};
use prober_core::files::RepoPath;
use prober_vcs::GitRepository;
use tempfile::TempDir;

struct TestRepository {
    directory: TempDir,
}

impl TestRepository {
    fn new() -> Self {
        let test_repository = Self {
            directory: TempDir::new().unwrap(),
        };

        test_repository.git(&["init", "--quiet", "--initial-branch=main"]);

        test_repository
    }

    fn path(&self) -> &Path {
        self.directory.path()
    }

    fn git(&self, arguments: &[&str]) -> String {
        run_git(self.path(), arguments)
    }

    fn write(&self, path: &str, content: &str) {
        let file_path = self.path().join(path);

        fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        fs::write(file_path, content).unwrap();
    }

    fn remove(&self, path: &str) {
        fs::remove_file(self.path().join(path)).unwrap();
    }

    fn commit_staged(&self, message: &str) {
        self.git(&["commit", "--quiet", "--message", message]);
    }

    fn commit_all(&self, message: &str) {
        self.git(&["add", "--all"]);
        self.commit_staged(message);
    }

    fn start_branch(&self, branch: &str) {
        self.git(&["switch", "--quiet", "--create", branch]);
    }

    fn changes_between(&self, base: &str, head: &str) -> Vec<FileChange> {
        GitRepository::discover(self.path())
            .unwrap()
            .changes_between(base, head)
            .unwrap()
    }

    fn error_between(&self, base: &str, head: &str) -> String {
        let error = GitRepository::discover(self.path())
            .unwrap()
            .changes_between(base, head)
            .unwrap_err();

        format!("{error:#}")
    }
}

fn run_git(directory: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Prober Tests")
        .env("GIT_AUTHOR_EMAIL", "tests@prober.invalid")
        .env("GIT_COMMITTER_NAME", "Prober Tests")
        .env("GIT_COMMITTER_EMAIL", "tests@prober.invalid")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8(output.stdout).unwrap()
}

fn repo_path(path: &str) -> RepoPath {
    RepoPath::new(path).unwrap()
}

fn change(path: &str, kind: ChangeKind) -> FileChange {
    FileChange {
        path: repo_path(path),
        kind,
    }
}

fn renamed(from: &str, to: &str) -> FileChange {
    change(
        to,
        ChangeKind::Renamed {
            from: repo_path(from),
        },
    )
}

fn numbered_lines(line_count: usize) -> String {
    (1..=line_count)
        .map(|line_number| format!("export const value{line_number} = {line_number};\n"))
        .collect()
}

#[test]
fn added_modified_and_deleted_files_are_listed_by_path() {
    let repository = TestRepository::new();

    repository.write("src/a.ts", "a");
    repository.write("src/b.ts", "b");
    repository.commit_all("base");
    repository.start_branch("feature");
    repository.write("src/a.ts", "a changed");
    repository.remove("src/b.ts");
    repository.write("src/nested/deep/c.ts", "c");
    repository.commit_all("feature");

    assert_eq!(
        repository.changes_between("main", "feature"),
        [
            change("src/a.ts", ChangeKind::Modified),
            change("src/b.ts", ChangeKind::Deleted),
            change("src/nested/deep/c.ts", ChangeKind::Added),
        ]
    );
}

#[test]
fn moved_file_is_a_rename() {
    let repository = TestRepository::new();

    repository.write("src/client.ts", &numbered_lines(10));
    repository.commit_all("base");
    repository.start_branch("feature");
    repository.git(&["mv", "src/client.ts", "src/http_client.ts"]);
    repository.commit_staged("move");

    assert_eq!(
        repository.changes_between("main", "feature"),
        [renamed("src/client.ts", "src/http_client.ts")]
    );
}

#[test]
fn moved_and_slightly_edited_file_is_still_a_rename() {
    let repository = TestRepository::new();
    let original_content = numbered_lines(10);

    repository.write("src/client.ts", &original_content);
    repository.commit_all("base");
    repository.start_branch("feature");
    repository.remove("src/client.ts");
    repository.write(
        "src/http/client.ts",
        &format!("{original_content}export const extra = 0;\n"),
    );
    repository.commit_all("move and edit");

    assert_eq!(
        repository.changes_between("main", "feature"),
        [renamed("src/client.ts", "src/http/client.ts")]
    );
}

#[test]
fn moved_and_rewritten_file_is_a_deletion_and_an_addition() {
    let repository = TestRepository::new();

    repository.write("src/old.ts", &numbered_lines(10));
    repository.commit_all("base");
    repository.start_branch("feature");
    repository.remove("src/old.ts");
    repository.write("src/new.ts", "export default function rewritten() {}\n");
    repository.commit_all("rewrite");

    assert_eq!(
        repository.changes_between("main", "feature"),
        [
            change("src/new.ts", ChangeKind::Added),
            change("src/old.ts", ChangeKind::Deleted),
        ]
    );
}

#[test]
fn changes_made_on_the_base_after_the_fork_are_ignored() {
    let repository = TestRepository::new();

    repository.write("src/shared.ts", "shared");
    repository.commit_all("base");
    repository.start_branch("feature");
    repository.write("src/feature.ts", "feature");
    repository.commit_all("feature");
    repository.git(&["switch", "--quiet", "main"]);
    repository.write("src/main_only.ts", "main only");
    repository.write("src/shared.ts", "shared changed on main");
    repository.commit_all("main moves on");

    assert_eq!(
        repository.changes_between("main", "feature"),
        [change("src/feature.ts", ChangeKind::Added)]
    );
}

#[test]
fn same_revision_has_no_changes() {
    let repository = TestRepository::new();

    repository.write("src/a.ts", "a");
    repository.commit_all("base");

    assert_eq!(repository.changes_between("main", "main"), []);
}

#[test]
fn executable_bit_change_is_a_modification() {
    let repository = TestRepository::new();

    repository.write("scripts/run.sh", "echo run");
    repository.commit_all("base");
    repository.start_branch("feature");
    repository.git(&["update-index", "--chmod=+x", "scripts/run.sh"]);
    repository.commit_staged("make executable");

    assert_eq!(
        repository.changes_between("main", "feature"),
        [change("scripts/run.sh", ChangeKind::Modified)]
    );
}

#[test]
fn revisions_can_be_commit_ids_or_relative_specs() {
    let repository = TestRepository::new();

    repository.write("src/a.ts", "a");
    repository.commit_all("first");
    repository.write("src/b.ts", "b");
    repository.commit_all("second");

    let first_commit = repository.git(&["rev-parse", "HEAD~1"]);
    let expected_changes = [change("src/b.ts", ChangeKind::Added)];

    assert_eq!(
        repository.changes_between(first_commit.trim(), "HEAD"),
        expected_changes
    );
    assert_eq!(
        repository.changes_between("HEAD~1", "HEAD"),
        expected_changes
    );
}

#[test]
fn repository_is_discovered_from_a_subdirectory() {
    let repository = TestRepository::new();

    repository.write("src/a.ts", "a");
    repository.commit_all("base");

    assert!(GitRepository::discover(&repository.path().join("src")).is_ok());
}

#[test]
fn discovering_outside_a_repository_fails() {
    let directory = TempDir::new().unwrap();
    let error = GitRepository::discover(directory.path()).err().unwrap();

    assert!(error.to_string().starts_with("no git repository found at "));
}

#[test]
fn unknown_revision_is_named_in_the_error() {
    let repository = TestRepository::new();

    repository.write("src/a.ts", "a");
    repository.commit_all("base");

    assert_eq!(
        repository.error_between("origin/main", "HEAD"),
        r#"unknown revision "origin/main""#
    );
}

#[test]
fn unrelated_histories_have_no_merge_base() {
    let repository = TestRepository::new();

    repository.write("src/a.ts", "a");
    repository.commit_all("base");
    repository.git(&["switch", "--quiet", "--orphan", "unrelated"]);
    repository.write("src/b.ts", "b");
    repository.commit_all("unrelated root");

    assert_eq!(
        repository.error_between("main", "unrelated"),
        r#"no merge base between "main" and "unrelated""#
    );
}

#[test]
fn shallow_clone_error_suggests_fetching_the_full_history() {
    let origin = TestRepository::new();
    let clone_directory = TempDir::new().unwrap();

    origin.write("src/a.ts", "a");
    origin.commit_all("first");
    origin.write("src/b.ts", "b");
    origin.commit_all("second");

    let origin_url = format!("file://{}", origin.path().display());
    let clone_path = clone_directory.path().to_str().unwrap();

    run_git(
        origin.path(),
        &["clone", "--quiet", "--depth", "1", &origin_url, clone_path],
    );

    let error = GitRepository::discover(clone_directory.path())
        .unwrap()
        .changes_between("HEAD~1", "HEAD")
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        r#"unknown revision "HEAD~1" (shallow clone: fetch the full history, e.g. actions/checkout with fetch-depth: 0)"#
    );
}

#[test]
fn files_at_lists_the_files_of_the_revision_not_of_the_working_tree() {
    let repository = TestRepository::new();

    repository.write("src/a.ts", "a");
    repository.write("src/nested/b.ts", "b");
    repository.commit_all("first");
    repository.write("src/c.ts", "c");
    repository.commit_all("second");
    repository.write("src/untracked.ts", "untracked");

    let git_repository = GitRepository::discover(repository.path()).unwrap();
    let file_paths = |revision| -> Vec<String> {
        git_repository
            .files_at(revision)
            .unwrap()
            .iter()
            .map(|file| file.as_str().to_owned())
            .collect()
    };

    assert_eq!(file_paths("HEAD~1"), ["src/a.ts", "src/nested/b.ts"]);
    assert_eq!(
        file_paths("HEAD"),
        ["src/a.ts", "src/c.ts", "src/nested/b.ts"]
    );
}

#[test]
fn file_is_read_as_committed_at_the_revision() {
    let repository = TestRepository::new();

    repository.write("package.json", r#"{ "version": "1.0.0" }"#);
    repository.write("src/a.ts", "a");
    repository.commit_all("first");
    repository.write("package.json", r#"{ "version": "1.1.0" }"#);
    repository.commit_all("second");
    repository.write("package.json", "uncommitted");

    let git_repository = GitRepository::discover(repository.path()).unwrap();
    let read_file = |revision, path| {
        git_repository
            .read_file(revision, &repo_path(path))
            .unwrap()
            .map(|bytes| String::from_utf8(bytes).unwrap())
    };

    assert_eq!(
        read_file("HEAD~1", "package.json").as_deref(),
        Some(r#"{ "version": "1.0.0" }"#)
    );
    assert_eq!(
        read_file("HEAD", "package.json").as_deref(),
        Some(r#"{ "version": "1.1.0" }"#)
    );
    assert_eq!(read_file("HEAD", "src/missing.ts"), None);
    assert_eq!(read_file("HEAD", "src"), None);
}

#[test]
fn only_the_checked_out_commit_is_checked_out() {
    let repository = TestRepository::new();

    repository.write("src/a.ts", "a");
    repository.commit_all("base");
    repository.start_branch("feature");
    repository.write("src/b.ts", "b");
    repository.commit_all("feature");

    let git_repository = GitRepository::discover(repository.path()).unwrap();

    assert!(git_repository.is_checked_out("feature").unwrap());
    assert!(git_repository.is_checked_out("HEAD").unwrap());
    assert!(!git_repository.is_checked_out("main").unwrap());
}

#[test]
fn work_dir_is_the_repository_root() {
    let repository = TestRepository::new();

    repository.write("src/a.ts", "a");
    repository.commit_all("base");

    let git_repository = GitRepository::discover(&repository.path().join("src")).unwrap();

    assert_eq!(
        git_repository.work_dir().unwrap().canonicalize().unwrap(),
        repository.path().canonicalize().unwrap()
    );
}

#[test]
fn resolved_revisions_are_the_commit_ids_and_their_merge_base() {
    let repository = TestRepository::new();

    repository.write("src/a.ts", "a");
    repository.commit_all("base");
    repository.start_branch("feature");
    repository.write("src/b.ts", "b");
    repository.commit_all("feature");
    repository.git(&["switch", "--quiet", "main"]);
    repository.write("src/c.ts", "c");
    repository.commit_all("main moves on");

    let commit_id = |revision: &str| repository.git(&["rev-parse", revision]).trim().to_owned();
    let resolved_revisions = GitRepository::discover(repository.path())
        .unwrap()
        .resolve_revisions("main", "feature")
        .unwrap();

    assert_eq!(
        resolved_revisions,
        ResolvedRevisions {
            base: commit_id("main"),
            head: commit_id("feature"),
            merge_base: commit_id("feature~1"),
        }
    );
}
