//! Test observation seams for the bounded walks: counters the walks bump,
//! and a hook that cancels a walk's token where a test asks. Test builds
//! only, and per thread: a test reads what its own calls did.

use std::cell::{Cell, RefCell};
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
    /// What a test runs once a signed-refs read has resolved the head it
    /// reads at, before it reads there.
    static RESOLVED: RefCell<Option<Box<dyn FnMut()>>> = const { RefCell::new(None) };
}

thread_local! {
    /// What a test runs once a canonical-head resolution has looked a
    /// delegate's tip up, before it sizes it.
    static TIP_PINNED: RefCell<Option<Box<dyn FnMut()>>> = const { RefCell::new(None) };
}

/// Runs the test's hook, if any, once a delegate's tip is looked up.
pub(crate) fn tip_pinned() {
    TIP_PINNED.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook();
        }
    });
}

/// Runs the test's hook, if any, once a signed-refs read has resolved its
/// head.
pub(crate) fn resolved() {
    RESOLVED.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook();
        }
    });
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
                routing: None,
                isolated: false,
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

    /// The bytes this process's peak resident set grew while `run` ran
    /// (Linux), or `None` elsewhere.
    fn peak_growth(run: impl FnOnce()) -> Option<u64> {
        fn status(field: &str) -> u64 {
            let status = std::fs::read_to_string("/proc/self/status").unwrap();
            let line = status.lines().find(|l| l.starts_with(field)).unwrap();
            line.split_whitespace()
                .nth(1)
                .unwrap()
                .parse::<u64>()
                .unwrap()
                * 1024
        }
        if !cfg!(target_os = "linux") {
            run();
            return None;
        }
        std::fs::write("/proc/self/clear_refs", "5").unwrap();
        let before = status("VmRSS:");
        run();
        Some(status("VmHWM:").saturating_sub(before))
    }

    // Review of libradicle #15, round 3: a signed-refs read is pinned to the
    // head it resolved. A fetch moving the ref in between (here, the moment
    // after it is resolved) to a commit with a 20 MiB `refs` blob changes
    // only the next read. In a child, for its peak memory.
    #[test]
    fn a_signed_refs_read_is_pinned_to_the_head_it_resolved() {
        const NAME: &str = "seams::tests::a_signed_refs_read_is_pinned_to_the_head_it_resolved";
        if std::env::var("LIBRADICLE_CHILD").as_deref() != Ok(NAME) {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args([NAME, "--exact", "--nocapture", "--test-threads=1"])
                .env("LIBRADICLE_CHILD", NAME)
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                out.status.success(),
                "the child died: {:?}\n{stdout}",
                out.status
            );
            assert!(
                stdout.contains("pinned: 1 remote, moved 1 time"),
                "{stdout}"
            );
            assert!(stdout.contains("next: TooLarge"), "{stdout}");
            if cfg!(target_os = "linux") {
                let read = |prefix: &str| -> u64 {
                    stdout
                        .lines()
                        .find_map(|l| l.strip_prefix(prefix))
                        .unwrap_or_else(|| panic!("no {prefix:?} in {stdout}"))
                        .parse()
                        .unwrap()
                };
                let (grew, control) = (read("peak growth: "), read("control growth: "));
                assert!(
                    grew < 8 * 1024 * 1024,
                    "the read grew the peak by {grew} bytes"
                );
                assert!(
                    control >= 16 * 1024 * 1024,
                    "the measure missed a load: {control}"
                );
            }
            return;
        }
        let f = fixture("rspinned", 1);
        let (rid, _) = f.repos[0];
        let nid = f.node.identity().nid;
        let sigrefs = format!("refs/namespaces/{nid}/refs/rad/sigrefs");
        let stored =
            radicle::storage::ReadStorage::repository(&f.node.profile.storage, rid).unwrap();
        let git = &stored.backend;
        let tip = git
            .find_reference(&sigrefs)
            .unwrap()
            .peel_to_commit()
            .unwrap();
        let huge = git.blob(&vec![b'x'; 20 * 1024 * 1024]).unwrap();
        let replacement = {
            let mut tree = git.treebuilder(Some(&tip.tree().unwrap())).unwrap();
            tree.insert("refs", huge, 0o100644).unwrap();
            let tree = git.find_tree(tree.write().unwrap()).unwrap();
            let signature = git2::Signature::now("Test", "test@example.com").unwrap();
            git.commit(None, &signature, &signature, "replacement", &tree, &[&tip])
                .unwrap()
        };
        let moved = std::rc::Rc::new(Cell::new(0));
        let path = git.path().to_owned();
        let name = sigrefs.clone();
        let counter = moved.clone();
        RESOLVED.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                if counter.get() == 0 {
                    let repo = git2::Repository::open_bare(&path).unwrap();
                    repo.reference(&name, replacement, true, "a fetch").unwrap();
                }
                counter.set(counter.get() + 1);
            }));
        });
        let mut result = None;
        let grew = peak_growth(|| {
            result = Some(f.node.remotes_bounded(rid, &Bounds::for_binding()));
        });
        RESOLVED.with(|hook| *hook.borrow_mut() = None);
        match result.unwrap() {
            Ok(remotes) => println!(
                "pinned: {} remote, moved {} time",
                remotes.len(),
                moved.get()
            ),
            Err(e) => println!("pinned: unexpected {e:?}"),
        }
        // The next read resolves the moved ref, and refuses the replacement.
        match f.node.remotes_bounded(rid, &Bounds::for_binding()) {
            Err(e @ Error::TooLarge { .. }) => println!("next: {e:?}"),
            other => println!("next: unexpected {other:?}"),
        }
        let control = peak_growth(|| {
            assert!(git.find_blob(huge).unwrap().content().len() >= 20 * 1024 * 1024);
        });
        if let (Some(grew), Some(control)) = (grew, control) {
            println!("peak growth: {grew}");
            println!("control growth: {control}");
        }
    }

    // Review of libradicle #15, round 5: with no usable local HEAD, the
    // canonical head uses the delegate tips it looked up, sized and charged.
    // A ref moved the moment after its lookup (here, to a 32 MiB blob)
    // changes only the next read. In a child, for its peak memory.
    #[test]
    fn a_canonical_head_uses_the_tips_it_pinned() {
        const NAME: &str = "seams::tests::a_canonical_head_uses_the_tips_it_pinned";
        if std::env::var("LIBRADICLE_CHILD").as_deref() != Ok(NAME) {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args([NAME, "--exact", "--nocapture", "--test-threads=1"])
                .env("LIBRADICLE_CHILD", NAME)
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                out.status.success(),
                "the child died: {:?}\n{stdout}",
                out.status
            );
            assert!(
                stdout.contains("pinned: the charged tip, moved 1 time"),
                "{stdout}"
            );
            assert!(stdout.contains("next: TooLarge"), "{stdout}");
            if cfg!(target_os = "linux") {
                let read = |prefix: &str| -> u64 {
                    stdout
                        .lines()
                        .find_map(|l| l.strip_prefix(prefix))
                        .unwrap_or_else(|| panic!("no {prefix:?} in {stdout}"))
                        .parse()
                        .unwrap()
                };
                let (grew, control) = (read("peak growth: "), read("control growth: "));
                assert!(
                    grew < 8 * 1024 * 1024,
                    "the read grew the peak by {grew} bytes"
                );
                assert!(
                    control >= 24 * 1024 * 1024,
                    "the measure missed a load: {control}"
                );
            }
            return;
        }
        let f = fixture("rstips", 1);
        let (rid, commit) = f.repos[0].clone();
        let nid = f.node.identity().nid;
        let tip_ref = format!("refs/namespaces/{nid}/refs/heads/main");
        let stored =
            radicle::storage::ReadStorage::repository(&f.node.profile.storage, rid).unwrap();
        let git = &stored.backend;
        git.set_head("refs/heads/does-not-exist").unwrap();
        let huge = git.blob(&vec![b'x'; 32 * 1024 * 1024]).unwrap();
        let moved = std::rc::Rc::new(Cell::new(0));
        let path = git.path().to_owned();
        let name = tip_ref.clone();
        let counter = moved.clone();
        TIP_PINNED.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                if counter.get() == 0 {
                    let repo = git2::Repository::open_bare(&path).unwrap();
                    repo.reference(&name, huge, true, "a fetch").unwrap();
                }
                counter.set(counter.get() + 1);
            }));
        });
        let mut result = None;
        let grew = peak_growth(|| {
            result = Some(f.node.repo_info_bounded(rid, &Bounds::for_binding()));
        });
        TIP_PINNED.with(|hook| *hook.borrow_mut() = None);
        match result.unwrap() {
            Ok(info) if info.head == commit => {
                println!("\npinned: the charged tip, moved {} time", moved.get())
            }
            other => println!("\npinned: unexpected {other:?}"),
        }
        // The next read looks the moved ref up, and refuses its target from
        // the header.
        match f.node.repo_info_bounded(rid, &Bounds::for_binding()) {
            Err(e @ Error::TooLarge { .. }) => println!("next: {e:?}"),
            other => println!("next: unexpected {other:?}"),
        }
        let control = peak_growth(|| {
            assert!(git.find_blob(huge).unwrap().content().len() >= 32 * 1024 * 1024);
        });
        if let (Some(grew), Some(control)) = (grew, control) {
            println!("peak growth: {grew}");
            println!("control growth: {control}");
        }
    }
}
