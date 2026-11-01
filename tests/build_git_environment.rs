//! The build script's git commands must not inherit the environment of a git
//! hook that is building this crate. The helper lives in `build_git.rs` so
//! that this test and `build.rs` compile the same source.

#[path = "../build_git.rs"]
mod build_git;

use std::ffi::OsStr;
use std::process::Command;

#[test]
fn every_repository_variable_is_removed_from_the_child() {
    let command = build_git::git();
    for variable in build_git::REPOSITORY_VARIABLES {
        let removed = command
            .get_envs()
            .any(|(name, value)| name == OsStr::new(variable) && value.is_none());
        assert!(removed, "{variable} is not removed from the git command");
    }
}

#[test]
fn the_child_does_not_see_the_index_file_of_the_outer_repository() {
    // The only test in this file that touches the process environment.
    std::env::set_var("GIT_INDEX_FILE", "/nonexistent/outer-index");
    let mut probe = Command::new("sh");
    probe.args(["-c", "printf %s \"${GIT_INDEX_FILE-unset}\""]);
    let output = build_git::without_repository_variables(probe)
        .output()
        .expect("sh runs");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "unset");
}
