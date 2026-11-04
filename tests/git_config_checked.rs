//! `isolate_git_config_single_threaded` and
//! `remove_git_config_env_single_threaded`: while a second thread exists,
//! each refuses and changes nothing; in a single-threaded process, each
//! applies.
//!
//! A test binary without libtest's harness (Cargo.toml), so that `main` is the
//! process's one thread. It checks the refusals with a second thread of its
//! own, then runs itself again, as a child that starts no thread, for the
//! successes: a thread can still be counted for a moment after its join. On
//! Windows the child also has the loader's worker threads, which start in
//! ntdll and which the count leaves out: a strict count found 4 threads there.

use std::process::Command;
use std::sync::mpsc;

use git2::ConfigLevel;
use libradicle::{Error, GIT_CONFIG_ENV};

/// Set in the child's environment.
const CHILD: &str = "LIBRADICLE_TEST_GIT_CONFIG_CHECKED_CHILD";
/// What the child prints once its checks pass.
const CHILD_OK: &str = "git_config_checked child: ok";

const LEVELS: [ConfigLevel; 4] = [
    ConfigLevel::System,
    ConfigLevel::XDG,
    ConfigLevel::Global,
    ConfigLevel::ProgramData,
];

/// libgit2's configuration search path at each level.
fn search_paths() -> Vec<String> {
    LEVELS
        .iter()
        .map(|&level| {
            // SAFETY: reads libgit2's process-wide options. No other thread
            // of this binary uses libgit2: the one the refusals spawn only
            // waits.
            let path = unsafe { git2::opts::get_search_path(level) }.expect("search path");
            path.into_string().expect("UTF-8 search path")
        })
        .collect()
}

fn main() {
    if std::env::var_os(CHILD).is_some() {
        applied_in_a_single_threaded_process();
        println!("{CHILD_OK}");
        return;
    }
    refused_while_a_second_thread_exists();
    applied_in_a_single_threaded_child();
    println!("git_config_checked: ok");
}

fn refused_while_a_second_thread_exists() {
    // Still one thread: set what the refusals must leave alone.
    std::env::set_var("HOME", std::env::temp_dir());
    std::env::set_var("GIT_CONFIG_GLOBAL", std::env::temp_dir().join("global"));
    let before = search_paths();
    assert_ne!(before[2], "", "the global search path, from HOME");

    let (release, wait) = mpsc::channel::<()>();
    let second = std::thread::spawn(move || {
        let _ = wait.recv();
    });

    for refused in [
        libradicle::isolate_git_config_single_threaded().map(|_| ()),
        libradicle::remove_git_config_env_single_threaded(),
    ] {
        match refused {
            Err(Error::NotSingleThreaded {
                threads: Some(threads),
            }) => assert!(threads >= 2, "{threads} threads counted"),
            other => panic!("not refused: {other:?}"),
        }
    }
    assert_eq!(search_paths(), before, "the search paths are unchanged");
    assert!(
        std::env::var_os("GIT_CONFIG_GLOBAL").is_some(),
        "the environment is unchanged"
    );

    release.send(()).expect("release");
    second.join().expect("join");
}

fn applied_in_a_single_threaded_child() {
    let exe = std::env::current_exe().expect("this test binary");
    let out = Command::new(exe)
        .env(CHILD, "1")
        .env("GIT_CONFIG_GLOBAL", std::env::temp_dir().join("global"))
        .output()
        .expect("run the child");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains(CHILD_OK),
        "child: {}\nstdout: {stdout}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
}

/// The child: one thread, `main`, which this is.
fn applied_in_a_single_threaded_process() {
    let _isolated: libradicle::GitConfigIsolated =
        libradicle::isolate_git_config_single_threaded().unwrap_or_else(|e| panic!("{e}"));
    assert!(
        search_paths().iter().all(String::is_empty),
        "every search path is empty: {:?}",
        search_paths()
    );
    // A later call, still single-threaded, returns the first one's result.
    libradicle::isolate_git_config_single_threaded().unwrap_or_else(|e| panic!("again: {e}"));

    assert!(std::env::var_os("GIT_CONFIG_GLOBAL").is_some());
    libradicle::remove_git_config_env_single_threaded().unwrap_or_else(|e| panic!("{e}"));
    for name in GIT_CONFIG_ENV {
        assert_eq!(std::env::var_os(name), None, "{name} removed");
    }
}
