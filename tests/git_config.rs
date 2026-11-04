//! `isolate_git_config`: once it has run, no git configuration outside a
//! repository applies, however the process's environment points libgit2 at
//! one. A test binary of its own, since libgit2's options are process-wide.

use std::path::PathBuf;

/// A `HOME` with a global git configuration that sets `user.name`, and an
/// XDG one that sets `user.email`.
fn configured_home() -> PathBuf {
    let home = std::env::temp_dir().join(format!("lr-git-config-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join(".config/git")).unwrap();
    std::fs::write(home.join(".gitconfig"), "[user]\n\tname = Global\n").unwrap();
    std::fs::write(
        home.join(".config/git/config"),
        "[user]\n\temail = xdg@example.invalid\n",
    )
    .unwrap();
    home
}

#[test]
fn no_configuration_outside_a_repository_applies() {
    let home = configured_home();
    // Before any libgit2 call, as the function's contract asks: one test, on
    // the test's own thread.
    std::env::set_var("HOME", &home);
    std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
    libradicle::isolate_git_config().expect("isolated");

    let config = git2::Config::open_default().unwrap();
    assert!(config.get_string("user.name").is_err());
    assert!(config.get_string("user.email").is_err());

    // A repository's own configuration still applies.
    let work = home.join("work");
    let repo = git2::Repository::init(&work).unwrap();
    repo.config()
        .unwrap()
        .set_str("user.name", "Local")
        .unwrap();
    let config = git2::Repository::open(&work).unwrap().config().unwrap();
    assert_eq!(config.get_string("user.name").unwrap(), "Local");
    assert!(config.get_string("user.email").is_err());

    let _ = std::fs::remove_dir_all(&home);
}
