//! TypeScript and JavaScript frontend: test files, imports, and their resolution to repo files.

mod conventions;
mod imports;
mod linker;
mod resolver;

pub use conventions::{ALWAYS_ALL_DEFAULTS, LANGUAGE, is_source_file, is_test_file};
pub use linker::TypeScriptLinker;
