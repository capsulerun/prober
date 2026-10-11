//! Python conventions: which files are modules, which are tests and test support, and which changes run every test.

use prober_core::files::RepoPath;

pub const LANGUAGE: &str = "python";

pub const ALWAYS_ALL_DEFAULTS: &[&str] = &[
    "**/.python-version",
    "**/setup.py",
    "**/setup.cfg",
    "**/pytest.ini",
    "**/tox.ini",
    "**/sitecustomize.py",
    "**/usercustomize.py",
];

pub const MANIFEST_DEFAULTS: &[&str] = &[
    "**/pyproject.toml",
    "**/uv.lock",
    "**/poetry.lock",
    "**/pdm.lock",
    "**/Pipfile",
    "**/Pipfile.lock",
    "**/requirements*.txt",
    "**/requirements/*.txt",
    "**/constraints*.txt",
];

pub(crate) const CONFTEST: &str = "conftest.py";

pub(crate) const PACKAGE_INIT: &str = "__init__.py";

pub(crate) const SNAPSHOT_DIRECTORIES: [&str; 2] = ["snapshots", "__snapshots__"];

pub(crate) const FIXTURE_DIRECTORIES: [&str; 2] = ["fixtures", "testdata"];

const SOURCE_EXTENSION: &str = ".py";

const TEST_PREFIX: &str = "test_";

const TEST_SUFFIX: &str = "_test.py";

const TEST_DIRECTORIES: [&str; 3] = ["test", "tests", "testing"];

const TEST_SUPPORT_STEMS: [&str; 3] = ["conftest", "testing", "pytest_plugin"];

pub(crate) const STANDARD_LIBRARY: &[&str] = &[
    "__future__",
    "abc",
    "aifc",
    "antigravity",
    "argparse",
    "array",
    "ast",
    "asynchat",
    "asyncio",
    "asyncore",
    "atexit",
    "audioop",
    "base64",
    "bdb",
    "binascii",
    "bisect",
    "builtins",
    "bz2",
    "cProfile",
    "calendar",
    "cgi",
    "cgitb",
    "chunk",
    "cmath",
    "cmd",
    "code",
    "codecs",
    "codeop",
    "collections",
    "colorsys",
    "compileall",
    "concurrent",
    "configparser",
    "contextlib",
    "contextvars",
    "copy",
    "copyreg",
    "crypt",
    "csv",
    "ctypes",
    "curses",
    "dataclasses",
    "datetime",
    "dbm",
    "decimal",
    "difflib",
    "dis",
    "distutils",
    "doctest",
    "email",
    "encodings",
    "ensurepip",
    "enum",
    "errno",
    "faulthandler",
    "fcntl",
    "filecmp",
    "fileinput",
    "fnmatch",
    "fractions",
    "ftplib",
    "functools",
    "gc",
    "genericpath",
    "getopt",
    "getpass",
    "gettext",
    "glob",
    "graphlib",
    "grp",
    "gzip",
    "hashlib",
    "heapq",
    "hmac",
    "html",
    "http",
    "idlelib",
    "imaplib",
    "imghdr",
    "imp",
    "importlib",
    "inspect",
    "io",
    "ipaddress",
    "itertools",
    "json",
    "keyword",
    "lib2to3",
    "linecache",
    "locale",
    "logging",
    "lzma",
    "mailbox",
    "mailcap",
    "marshal",
    "math",
    "mimetypes",
    "mmap",
    "modulefinder",
    "msilib",
    "msvcrt",
    "multiprocessing",
    "netrc",
    "nis",
    "nntplib",
    "nt",
    "ntpath",
    "nturl2path",
    "numbers",
    "opcode",
    "operator",
    "optparse",
    "os",
    "ossaudiodev",
    "pathlib",
    "pdb",
    "pickle",
    "pickletools",
    "pipes",
    "pkgutil",
    "platform",
    "plistlib",
    "poplib",
    "posix",
    "posixpath",
    "pprint",
    "profile",
    "pstats",
    "pty",
    "pwd",
    "py_compile",
    "pyclbr",
    "pydoc",
    "pydoc_data",
    "pyexpat",
    "queue",
    "quopri",
    "random",
    "re",
    "readline",
    "reprlib",
    "resource",
    "rlcompleter",
    "runpy",
    "sched",
    "secrets",
    "select",
    "selectors",
    "shelve",
    "shlex",
    "shutil",
    "signal",
    "site",
    "smtpd",
    "smtplib",
    "sndhdr",
    "socket",
    "socketserver",
    "spwd",
    "sqlite3",
    "sre_compile",
    "sre_constants",
    "sre_parse",
    "ssl",
    "stat",
    "statistics",
    "string",
    "stringprep",
    "struct",
    "subprocess",
    "sunau",
    "symtable",
    "sys",
    "sysconfig",
    "syslog",
    "tabnanny",
    "tarfile",
    "telnetlib",
    "tempfile",
    "termios",
    "textwrap",
    "this",
    "threading",
    "time",
    "timeit",
    "tkinter",
    "token",
    "tokenize",
    "tomllib",
    "trace",
    "traceback",
    "tracemalloc",
    "tty",
    "turtle",
    "turtledemo",
    "types",
    "typing",
    "unicodedata",
    "unittest",
    "urllib",
    "uu",
    "uuid",
    "venv",
    "warnings",
    "wave",
    "weakref",
    "webbrowser",
    "winreg",
    "winsound",
    "wsgiref",
    "xdrlib",
    "xml",
    "xmlrpc",
    "zipapp",
    "zipfile",
    "zipimport",
    "zlib",
    "zoneinfo",
];

pub fn is_source_file(path: &RepoPath) -> bool {
    path.as_str().ends_with(SOURCE_EXTENSION)
}

pub fn is_test_file(path: &RepoPath) -> bool {
    let name = file_name(path);

    is_source_file(path) && (name.starts_with(TEST_PREFIX) || name.ends_with(TEST_SUFFIX))
}

pub fn is_module_file(path: &RepoPath) -> bool {
    is_source_file(path) && !is_test_file(path)
}

pub fn is_test_support_file(path: &RepoPath) -> bool {
    let (directory, name) = split_path(path.as_str());
    let stem = name.strip_suffix(SOURCE_EXTENSION).unwrap_or(name);
    let in_test_directory = directory
        .split('/')
        .any(|segment| TEST_DIRECTORIES.contains(&segment));
    let in_fixtures = directory
        .split('/')
        .any(|segment| FIXTURE_DIRECTORIES.contains(&segment));

    is_module_file(path)
        && (TEST_SUPPORT_STEMS.contains(&stem) || in_test_directory && !in_fixtures)
}

pub(crate) fn is_standard_library(top_level: &str) -> bool {
    STANDARD_LIBRARY.binary_search(&top_level).is_ok()
}

pub(crate) fn file_name(path: &RepoPath) -> &str {
    split_path(path.as_str()).1
}

pub(crate) fn split_path(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

pub(crate) fn parent_directory(path: &str) -> &str {
    split_path(path).0
}

pub(crate) fn join_directory(directory: &str, name: &str) -> String {
    if directory.is_empty() {
        name.to_owned()
    } else if name.is_empty() {
        directory.to_owned()
    } else {
        format!("{directory}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_path(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    #[test]
    fn standard_library_names_are_sorted_for_binary_search() {
        assert!(STANDARD_LIBRARY.is_sorted());
        assert!(is_standard_library("os"));
        assert!(is_standard_library("__future__"));
        assert!(!is_standard_library("requests"));
    }

    #[test]
    fn test_files_follow_pytest_default_names() {
        for test in ["tests/test_api.py", "pkg/api_test.py", "test_main.py"] {
            assert!(is_test_file(&repo_path(test)), "{test}");
        }

        for other in [
            "tests/conftest.py",
            "pkg/testing.py",
            "tests/test_data.json",
        ] {
            assert!(!is_test_file(&repo_path(other)), "{other}");
        }
    }

    #[test]
    fn test_support_is_named_after_tests() {
        for support in [
            "tests/conftest.py",
            "tests/common.py",
            "pkg/testing.py",
            "src/pkg/pytest_plugin.py",
            "tests/components/light/__init__.py",
        ] {
            assert!(is_test_support_file(&repo_path(support)), "{support}");
        }

        for other in [
            "pkg/api.py",
            "tests/test_api.py",
            "tests/fixtures/sample.py",
            "tests/data.json",
        ] {
            assert!(!is_test_support_file(&repo_path(other)), "{other}");
        }
    }
}
