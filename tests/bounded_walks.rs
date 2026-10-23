//! The `_bounded` reads: each stops with `Cancelled` once its token is
//! cancelled, and with `TooLarge` when its result's JSON, as the `json`
//! module writes it, would be over its budget. A result whose JSON is
//! exactly the budget fits.

use std::path::{Path, PathBuf};

use libradicle::{json, Bounds, CancelToken, Embedded, Error, HostKey, Options};

fn key() -> HostKey {
    let mut bytes = vec![5u8; 32];
    HostKey::from_bytes(&mut bytes).expect("32 bytes")
}

/// A scratch directory under TMPDIR, unless that is too deep for the node's
/// Unix socket path.
fn scratch(name: &str) -> PathBuf {
    let name = format!("{name}-{}", std::process::id());
    let mut dir = std::env::temp_dir().join(&name);
    if dir.join("home/node/control.sock").as_os_str().len() > 100 {
        dir = Path::new("/tmp").join(&name);
    }
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Fixture {
    dir: PathBuf,
    node: Embedded,
    rid: radicle::identity::RepoId,
    /// The commits on `main`, oldest first.
    commits: Vec<String>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn commit(repo: &git2::Repository, message: &str) -> git2::Oid {
    let mut index = repo.index().unwrap();
    index
        .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let signature = git2::Signature::now("Test", "test@example.com").unwrap();
    let parent = repo.head().ok().map(|head| head.peel_to_commit().unwrap());
    let parents = parent.iter().collect::<Vec<_>>();
    repo.commit(
        Some("refs/heads/main"),
        &signature,
        &signature,
        message,
        &tree,
        &parents,
    )
    .unwrap()
}

/// A node that dials nobody, with one imported repository of three commits:
/// two files; one of them changed; a file of 2000 short lines and a 5000
/// byte binary file added. And two issues.
fn fixture(name: &str) -> Fixture {
    let dir = scratch(name);
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.json"),
        r#"{"preferredSeeds":[],"node":{"alias":"walks","network":"test","connect":[],"peers":{"type":"static"}}}"#,
    )
    .unwrap();
    let work = dir.join("work");
    let repo = git2::Repository::init(&work).unwrap();
    repo.set_head("refs/heads/main").unwrap();
    std::fs::write(work.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    std::fs::create_dir_all(work.join("dir")).unwrap();
    std::fs::write(work.join("dir/b.txt"), "bee\n").unwrap();
    let first = commit(&repo, "first");
    std::fs::write(work.join("a.txt"), "one\n2\nthree\n").unwrap();
    let second = commit(&repo, "second\n\nChanges a line.");
    std::fs::write(work.join("lines.txt"), "x\n".repeat(2000)).unwrap();
    std::fs::write(work.join("blob.bin"), vec![0u8; 5000]).unwrap();
    let third = commit(&repo, "third");

    let mut node = Embedded::start_with_key(
        Options {
            home: home.to_str().expect("utf-8 temp dir").into(),
            alias: "walks".into(),
            listen: vec![],
        },
        key(),
    )
    .expect("start");
    let rid = node
        .import_repo(&work, "walks", "bounded walks", "main")
        .expect("import");
    for title in ["First issue", "Second issue"] {
        node.create_issue(rid, title, "Its description.", vec![])
            .expect("issue");
    }
    Fixture {
        dir,
        node,
        rid,
        commits: [first, second, third].map(|oid| oid.to_string()).into(),
    }
}

fn budget(budget: u64) -> Bounds {
    Bounds::new(CancelToken::new(), budget)
}

fn cancelled() -> Bounds {
    let token = CancelToken::new();
    token.cancel();
    Bounds::new(token, u64::MAX - 1)
}

/// `read` at a budget of exactly `len` bytes fits; at one byte less it is
/// `TooLarge`, asking for `len`; cancelled, it is `Cancelled`.
fn exact<T: std::fmt::Debug>(len: u64, read: impl Fn(&Bounds) -> Result<T, Error>) {
    read(&budget(len)).expect("a result at exactly its budget");
    match read(&budget(len - 1)) {
        Err(Error::TooLarge { size, limit }) => {
            assert_eq!(limit, len - 1);
            assert!(size >= len, "{size} < {len}");
        }
        other => panic!("expected TooLarge, got {other:?}"),
    }
    assert!(matches!(read(&cancelled()), Err(Error::Cancelled)));
}

fn len(write: impl FnOnce(&mut Vec<u8>) -> json::Written) -> u64 {
    json::to_string(write).len() as u64
}

#[test]
fn bounded_walks_stop_at_their_budget_or_their_token() {
    let f = fixture("radwalks");
    let (node, rid) = (&f.node, f.rid);

    let files = node.list_files(rid).expect("files");
    assert_eq!(files.len(), 4);
    exact(len(|w| json::files(w, &files)), |b| {
        node.list_files_bounded(rid, b)
    });

    let head = f.commits[2].as_str();
    let commits = node.commits(rid, head, 0, 10).expect("commits");
    assert_eq!(commits.len(), 3);
    exact(len(|w| json::commits(w, &commits)), |b| {
        node.commits_bounded(rid, head, 0, 10, b)
    });
    // A later page is the same commits as the unbounded read's.
    let page = node
        .commits_bounded(rid, head, 1, 1, &budget(1 << 20))
        .expect("page");
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].id, node.commits(rid, head, 1, 1).unwrap()[0].id);

    let second = f.commits[1].as_str();
    let detail = node.commit(rid, second).expect("commit");
    exact(len(|w| json::commit(w, &detail)), |b| {
        node.commit_bounded(rid, second, b)
    });

    let issues = node.issues(rid).expect("issues");
    assert_eq!(issues.len(), 2);
    exact(len(|w| json::issues(w, &issues)), |b| {
        node.issues_bounded(rid, b)
    });

    let patches = node.patches(rid).expect("patches");
    assert!(patches.is_empty());
    exact(len(|w| json::patches(w, &patches)), |b| {
        node.patches_bounded(rid, b)
    });

    let repos = node.list_repos().expect("repos");
    assert_eq!(repos.len(), 1);
    exact(len(|w| json::repos(w, &repos)), |b| {
        node.list_repos_bounded(b)
    });

    // While it counts it holds one contributor, `Test <test@example.com>`
    // (20 bytes), and one branch, `main` (4), each at 64 bytes more; then its
    // JSON.
    let stats = node.repo_stats(rid, head).expect("stats");
    assert_eq!(
        (stats.commits, stats.branches, stats.contributors),
        (3, 1, 1)
    );
    let json = len(|w| json::repo_stats(w, &stats));
    exact(20 + 4 + 2 * 64 + json, |b| {
        node.repo_stats_bounded(rid, head, b)
    });
    // Review of libradicle #8, round 1: its JSON alone doesn't fit one byte
    // short of itself.
    assert!(matches!(
        node.repo_stats_bounded(rid, head, &budget(json - 1)),
        Err(Error::TooLarge { .. })
    ));
}

#[test]
fn a_diff_that_cannot_fit_is_refused_before_it_is_built() {
    let f = fixture("radwalkdiff");
    let (node, rid) = (&f.node, f.rid);
    let third = f.commits[2].as_str();
    let detail = node.commit(rid, third).expect("commit");
    let whole = len(|w| json::commit(w, &detail));

    // Lines first: 2000 additions of `x\n` take at least 2000 * 42 bytes,
    // and are refused as soon as their count passes the budget, short of
    // the whole detail's JSON.
    let limit = 50_000;
    assert!(whole > limit);
    match node.commit_bounded(rid, third, &budget(limit)) {
        Err(Error::TooLarge { size, limit: left }) => {
            assert_eq!(left, limit);
            assert!(size > limit && size < whole, "{size} of {whole}");
        }
        other => panic!("expected TooLarge from the line count, got {other:?}"),
    }

    // A file larger than the budget is refused before it is loaded, by its
    // header's size, the binary file's included.
    match node.commit_bounded(rid, third, &budget(4999)) {
        Err(Error::TooLarge { size, limit }) => assert_eq!((size, limit), (5000, 4999)),
        other => panic!("expected TooLarge from the binary file's size, got {other:?}"),
    }
    // And the files it would load are sized together: 5000 and 4000 bytes
    // don't fit 8999, though each would.
    match node.commit_bounded(rid, third, &budget(8999)) {
        Err(Error::TooLarge { size, limit }) => assert_eq!((size, limit), (9000, 8999)),
        other => panic!("expected TooLarge from the files' sizes, got {other:?}"),
    }
}

/// Imports into `f`'s node a repository whose commits each add or move many
/// small files: 200 text files; 512 empty files; 300 small binary files;
/// then the 200 text files moved. Its RID, and its commits oldest first.
fn many_files(f: &mut Fixture) -> (radicle::identity::RepoId, Vec<String>) {
    let work = f.dir.join("many");
    let repo = git2::Repository::init(&work).unwrap();
    repo.set_head("refs/heads/main").unwrap();
    std::fs::create_dir_all(work.join("r")).unwrap();
    for i in 0..200 {
        std::fs::write(work.join(format!("r/{i}.txt")), format!("text file {i}\n")).unwrap();
    }
    let texts = commit(&repo, "texts");
    std::fs::create_dir_all(work.join("e")).unwrap();
    for i in 0..512 {
        std::fs::write(work.join(format!("e/{i}")), b"").unwrap();
    }
    let empties = commit(&repo, "empties");
    std::fs::create_dir_all(work.join("b")).unwrap();
    for i in 0..300u32 {
        let mut bytes = vec![0u8; 100];
        bytes[..4].copy_from_slice(&i.to_le_bytes());
        std::fs::write(work.join(format!("b/{i}.bin")), bytes).unwrap();
    }
    let binaries = commit(&repo, "binaries");
    std::fs::rename(work.join("r"), work.join("s")).unwrap();
    let mut index = repo.index().unwrap();
    index.update_all(["*"], None).unwrap();
    index.write().unwrap();
    let moved = commit(&repo, "moved");
    let rid = f
        .node
        .import_repo(&work, "many", "many files", "main")
        .expect("import");
    (
        rid,
        [texts, empties, binaries, moved]
            .map(|oid| oid.to_string())
            .into(),
    )
}

// Review of libradicle #8, round 1: a diff of many files, whose lines are few
// or none, is refused before it is built: each is refused asking for less
// than the JSON the whole detail takes.
#[test]
fn a_diff_of_many_files_is_refused_before_it_is_built() {
    let mut f = fixture("radwalkmany");
    let (rid, commits) = many_files(&mut f);
    let node = &f.node;
    for (what, commit) in [
        ("512 empty files", &commits[1]),
        ("300 small binary files", &commits[2]),
        ("200 moved files", &commits[3]),
    ] {
        let detail = node.commit(rid, commit).expect("commit");
        let whole = len(|w| json::commit(w, &detail));
        match node.commit_bounded(rid, commit, &budget(1024)) {
            Err(Error::TooLarge { size, limit }) => {
                assert_eq!(limit, 1024);
                assert!(size > limit && size < whole, "{what}: {size} of {whole}");
            }
            other => panic!("{what}: expected TooLarge, got {other:?}"),
        }
        // Just short of the whole: refused too, the result never over budget.
        assert!(
            matches!(
                node.commit_bounded(rid, commit, &budget(whole - 1)),
                Err(Error::TooLarge { .. })
            ),
            "{what}"
        );
        node.commit_bounded(rid, commit, &budget(whole))
            .unwrap_or_else(|e| panic!("{what} at exactly its budget: {e}"));
    }
}

// Moves are found before their records are counted: a budget the moves fit,
// though the deletions and additions they join wouldn't, isn't refused, and
// one a little short of their records is refused before the diff is built.
#[test]
fn moved_files_are_counted_as_moves() {
    let mut f = fixture("radwalkmoved");
    let (rid, commits) = many_files(&mut f);
    let moved = &commits[3];
    let detail = f.node.commit(rid, moved).expect("commit");
    let whole = len(|w| json::commit(w, &detail));
    // 200 moves fit a budget 400 records, deletions and additions, wouldn't.
    assert!(whole < 400 * 119, "{whole}");
    f.node
        .commit_bounded(rid, moved, &budget(whole))
        .expect("the moves at exactly their budget");
    match f.node.commit_bounded(rid, moved, &budget(whole - 2000)) {
        Err(Error::TooLarge { size, .. }) => assert!(size < whole, "{size} of {whole}"),
        other => panic!("expected TooLarge, got {other:?}"),
    }
}

// Review of libradicle #8, round 1: listing repositories stops at the first
// one that doesn't fit, with many in storage.
#[test]
fn listing_many_repositories_stops_at_the_first_that_doesnt_fit() {
    let mut f = fixture("radwalkrepos");
    for i in 0..6 {
        let work = f.dir.join(format!("repo{i}"));
        let repo = git2::Repository::init(&work).unwrap();
        repo.set_head("refs/heads/main").unwrap();
        std::fs::write(work.join("README"), format!("repo {i}\n")).unwrap();
        commit(&repo, "first");
        f.node
            .import_repo(&work, &format!("repo{i}"), "", "main")
            .expect("import");
    }
    let repos = f.node.list_repos().expect("repos");
    assert_eq!(repos.len(), 7);
    // Room for the brackets and the first repository, but not the second.
    let first = len(|w| json::repo(w, &repos[0]));
    match f.node.list_repos_bounded(&budget(2 + first)) {
        Err(Error::TooLarge { limit, .. }) => assert_eq!(limit, 2 + first),
        other => panic!("expected TooLarge, got {other:?}"),
    }
    assert!(matches!(
        f.node.list_repos_bounded(&cancelled()),
        Err(Error::Cancelled)
    ));
}
