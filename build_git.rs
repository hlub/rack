//! Git commands for the build script that cannot reach the repository the
//! build was started from.

use std::process::Command;

/// The variables that name a repository, its index or its objects. Cargo
/// runs build scripts from inside a git hook's environment, where these point
/// at the outer repository.
pub const REPOSITORY_VARIABLES: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_PREFIX",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
];

/// Removes every repository variable from the command's environment.
pub fn without_repository_variables(mut command: Command) -> Command {
    for variable in REPOSITORY_VARIABLES {
        command.env_remove(variable);
    }
    command
}

/// A `git` command that works on the repository of its working directory.
pub fn git() -> Command {
    without_repository_variables(Command::new("git"))
}
