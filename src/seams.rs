//! Test observation seams for the bounded walks: counters the walks bump,
//! and a hook that cancels a walk's token where a test asks. Test builds
//! only, and per thread: a test reads what its own calls did.

use std::cell::Cell;
use std::thread::LocalKey;

use crate::CancelToken;

/// A step of `repo_stats_bounded` a test can cancel its token at, just
/// before the token is checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// Each commit walked.
    Commit,
    /// Each signed remote.
    Remote,
}

thread_local! {
    /// Entries `list_repos_bounded` has read from the storage directory.
    pub(crate) static ENTRIES: Cell<usize> = const { Cell::new(0) };
    /// Repositories `list_repos_bounded` has opened.
    pub(crate) static OPENED: Cell<usize> = const { Cell::new(0) };
    /// Commits `repo_stats_bounded` has read on from, past its token check.
    pub(crate) static COMMITS: Cell<usize> = const { Cell::new(0) };
    /// Signed remotes `repo_stats_bounded` has read on from, past its token
    /// check.
    pub(crate) static REMOTES: Cell<usize> = const { Cell::new(0) };
    /// The step `repo_stats_bounded` cancels its token at, if any.
    static CANCEL_AT: Cell<Option<Step>> = const { Cell::new(None) };
}

pub(crate) fn bump(counter: &'static LocalKey<Cell<usize>>) {
    counter.with(|count| count.set(count.get() + 1));
}

/// Cancels `token` if this is the step a test cancels at.
pub(crate) fn at(step: Step, token: &CancelToken) {
    if CANCEL_AT.with(Cell::get) == Some(step) {
        token.cancel();
    }
}

fn read(counter: &'static LocalKey<Cell<usize>>) -> usize {
    counter.with(|count| count.replace(0))
}

mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{Bounds, Embedded, Error, HostKey, Options, RepoId};

    struct Fixture {
        dir: PathBuf,
        node: Embedded,
        /// The repositories imported, and each one's commit.
        repos: Vec<(RepoId, String)>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A node that dials nobody, with `repos` imported repositories of one
    /// commit each.
    fn fixture(name: &str, repos: usize) -> Fixture {
        // Short: a COB ref's path in storage, under it, is long enough to
        // pass Windows' 260 characters under a long name.
        let dir = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("config.json"),
            r#"{"preferredSeeds":[],"node":{"alias":"seams","network":"test","connect":[],"peers":{"type":"static"}}}"#,
        )
        .unwrap();
        let mut bytes = vec![7u8; 32];
        let key = HostKey::from_bytes(&mut bytes).expect("32 bytes");
        let mut node = Embedded::start_with_key(
            Options {
                home: home.clone(),
                alias: "seams".into(),
                listen: vec![],
                control_socket: false,
            },
            key,
        )
        .expect("start");
        let repos = (0..repos)
            .map(|i| {
                let work = dir.join(format!("work{i}"));
                let repo = git2::Repository::init(&work).unwrap();
                repo.set_head("refs/heads/main").unwrap();
                std::fs::write(work.join("README"), format!("repo {i}\n")).unwrap();
                let mut index = repo.index().unwrap();
                index.add_path("README".as_ref()).unwrap();
                let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
                let signature = git2::Signature::now("Test", "test@example.com").unwrap();
                let commit = repo
                    .commit(
                        Some("refs/heads/main"),
                        &signature,
                        &signature,
                        "first",
                        &tree,
                        &[],
                    )
                    .unwrap();
                let rid = node
                    .import_repo(&work, &format!("repo{i}"), "", "main")
                    .expect("import");
                (rid, commit.to_string())
            })
            .collect();
        Fixture { dir, node, repos }
    }

    fn budget(budget: u64) -> Bounds {
        Bounds::new(CancelToken::new(), budget)
    }

    // Review of libradicle #8, round 2: with no room left for a repository,
    // the listing opens none.
    #[test]
    fn a_listing_with_no_room_left_opens_no_repository() {
        let f = fixture("rsopened", 1);
        read(&OPENED);
        assert!(matches!(
            f.node.list_repos_bounded(&budget(2)),
            Err(Error::TooLarge { size: 64, limit: 2 })
        ));
        assert_eq!(read(&OPENED), 0);
        // The seam counts: with room, the repository opens.
        assert_eq!(
            f.node.list_repos_bounded(&budget(1 << 20)).unwrap().len(),
            1
        );
        assert_eq!(read(&OPENED), 1);
    }

    // Review of libradicle #8, round 2: the listing reads storage one entry at
    // a time, and none past the repository that doesn't fit.
    #[test]
    fn a_listing_reads_no_entry_past_the_repository_that_doesnt_fit() {
        let f = fixture("rsentries", 2);
        let storage = f.node.profile.storage.path().to_owned();
        for i in 0..100 {
            std::fs::create_dir(storage.join(format!(".hidden-{i}"))).unwrap();
        }
        let names = std::fs::read_dir(&storage)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        // The first repository, whichever it is, comes before the second.
        let first = 1 + names
            .iter()
            .position(|name| !name.to_string_lossy().starts_with('.'))
            .unwrap();
        assert!(first < names.len());
        read(&ENTRIES);
        assert!(matches!(
            f.node.list_repos_bounded(&budget(2)),
            Err(Error::TooLarge { .. })
        ));
        assert_eq!(read(&ENTRIES), first);
        assert_eq!(
            f.node.list_repos_bounded(&budget(1 << 20)).unwrap().len(),
            2
        );
        assert_eq!(read(&ENTRIES), names.len());
    }

    // Review of libradicle #8, round 2: cancelled at a commit, or at a
    // remote, `repo_stats` reads nothing of it.
    #[test]
    fn a_stats_walk_cancelled_at_a_step_reads_none_of_it() {
        let f = fixture("rsstats", 1);
        let (rid, commit) = &f.repos[0];
        for (step, counter) in [(Step::Commit, &COMMITS), (Step::Remote, &REMOTES)] {
            read(counter);
            CANCEL_AT.with(|at| at.set(Some(step)));
            let cancelled = f.node.repo_stats_bounded(*rid, commit, &budget(1 << 20));
            CANCEL_AT.with(|at| at.set(None));
            assert!(
                matches!(cancelled, Err(Error::Cancelled)),
                "{step:?}: {cancelled:?}"
            );
            assert_eq!(read(counter), 0, "{step:?}");
        }
        // The seams count: uncancelled, the one commit, and the one remote,
        // the node's, are read.
        read(&COMMITS);
        read(&REMOTES);
        let stats = f
            .node
            .repo_stats_bounded(*rid, commit, &budget(1 << 20))
            .unwrap();
        assert_eq!((stats.commits, stats.branches), (1, 1));
        assert_eq!((read(&COMMITS), read(&REMOTES)), (1, 1));
    }
}
