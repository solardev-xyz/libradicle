//! `isolate_git_config` and `remove_git_config_env`: libgit2's default
//! configuration search paths are empty, and the environment variables an
//! environment-driven open reads are gone. A repository's own configuration
//! still applies, includes and all.
//!
//! A test binary of its own, with one test: both change process-wide state,
//! and their contract is that no other thread uses libgit2 meanwhile.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use git2::{Repository, RepositoryOpenFlags};

/// A `HOME` whose global git configuration sets `user.name` and whose XDG
/// one sets `user.email`, and, outside it, the files the environment and an
/// include point at, each setting a key of its own.
fn configured_home() -> PathBuf {
    let home = std::env::temp_dir().join(format!("lr-git-config-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join(".config/git")).unwrap();
    std::fs::create_dir_all(home.join("outside")).unwrap();
    for (path, body) in [
        (".gitconfig", "[user]\n\tname = Global\n"),
        (
            ".config/git/config",
            "[user]\n\temail = xdg@example.invalid\n",
        ),
        ("outside/global", "[outside]\n\tglobal = set\n"),
        ("outside/system", "[outside]\n\tsystem = set\n"),
        ("outside/include", "[outside]\n\tinclude = set\n"),
    ] {
        std::fs::write(home.join(path), body).unwrap();
    }
    home
}

/// Points `GIT_CONFIG_GLOBAL` and `GIT_CONFIG_SYSTEM` at the outside files.
fn set_config_env(home: &Path) {
    std::env::set_var("GIT_CONFIG_GLOBAL", home.join("outside/global"));
    std::env::set_var("GIT_CONFIG_SYSTEM", home.join("outside/system"));
}

fn get(config: &git2::Config, key: &str) -> Option<String> {
    config.get_string(key).ok()
}

#[test]
fn default_search_paths_are_empty_and_the_config_environment_removed() {
    let home = configured_home();
    // Before any libgit2 call, on the test's one thread, as both functions'
    // contract asks.
    std::env::set_var("HOME", &home);
    std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
    set_config_env(&home);
    std::env::set_var("GIT_CONFIG_NOSYSTEM", "0");
    // SAFETY: this binary's one test, on its own thread, before any libgit2
    // call: no other thread uses libgit2 or reads the environment.
    unsafe { libradicle::remove_git_config_env() };
    for name in libradicle::GIT_CONFIG_ENV {
        assert_eq!(std::env::var_os(name), None, "{name} removed");
    }
    // SAFETY: as above.
    let _isolated: libradicle::GitConfigIsolated =
        unsafe { libradicle::isolate_git_config() }.expect("isolated");
    // A later call returns the first one's result.
    // SAFETY: as above.
    unsafe { libradicle::isolate_git_config() }.expect("isolated again");

    // The default search paths are empty: neither the global nor the XDG
    // configuration applies.
    let config = git2::Config::open_default().unwrap();
    assert_eq!(get(&config, "user.name"), None);
    assert_eq!(get(&config, "user.email"), None);

    // A repository's own configuration still applies.
    let work = home.join("work");
    let repo = Repository::init(&work).unwrap();
    repo.config()
        .unwrap()
        .set_str("user.name", "Local")
        .unwrap();
    let config = Repository::open(&work).unwrap().config().unwrap();
    assert_eq!(get(&config, "user.name").as_deref(), Some("Local"));
    assert_eq!(get(&config, "user.email"), None);

    // With the variables removed, an environment-driven open finds no
    // configuration through them either.
    std::env::set_var("GIT_DIR", work.join(".git"));
    let config = Repository::open_from_env().unwrap().config().unwrap();
    assert_eq!(get(&config, "outside.global"), None);
    assert_eq!(get(&config, "outside.system"), None);

    // Set again, they don't reach an open by path, as libradicle's and
    // heartwood storage's are, the storage's flags included...
    set_config_env(&home);
    let storage = home.join("storage.git");
    Repository::init_bare(&storage).unwrap();
    let by_path = [
        Repository::open(&work).unwrap(),
        Repository::open_ext(
            &storage,
            RepositoryOpenFlags::BARE
                | RepositoryOpenFlags::NO_DOTGIT
                | RepositoryOpenFlags::NO_SEARCH,
            &[] as &[&OsStr],
        )
        .unwrap(),
    ];
    for opened in by_path {
        let config = opened.config().unwrap();
        assert_eq!(get(&config, "outside.global"), None, "{:?}", opened.path());
        assert_eq!(get(&config, "outside.system"), None, "{:?}", opened.path());
    }
    // ...but an environment-driven open reads them, whatever the search
    // paths: why a host that might open one removes or refuses them.
    let config = Repository::open_from_env().unwrap().config().unwrap();
    assert_eq!(get(&config, "outside.global").as_deref(), Some("set"));
    assert_eq!(get(&config, "outside.system").as_deref(), Some("set"));
    for name in ["GIT_DIR", "GIT_CONFIG_GLOBAL", "GIT_CONFIG_SYSTEM"] {
        std::env::remove_var(name);
    }

    // An include in a repository's own configuration applies, wherever it
    // points: that configuration is its local owner's to write, and a
    // peer's fetch never writes it (`isolate_git_config`'s docs).
    repo.config()
        .unwrap()
        .set_str(
            "include.path",
            home.join("outside/include").to_str().unwrap(),
        )
        .unwrap();
    let config = Repository::open(&work).unwrap().config().unwrap();
    assert_eq!(get(&config, "outside.include").as_deref(), Some("set"));

    let _ = std::fs::remove_dir_all(&home);
}
