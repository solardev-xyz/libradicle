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
            control_socket: true,
            routing: None,
            isolated: false,
            repositories: None,
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

// Review of libradicle #8, round 2: a diff's hunks are counted before it is
// built. The first commit's two files, three lines and two hunks don't fit
// 500 bytes, and are refused for their count, short of the whole detail's
// JSON; without the hunks, the count would fit, and only the whole refused.
#[test]
fn a_diff_counts_its_hunks_before_it_is_built() {
    let f = fixture("radwalkhunks");
    let first = f.commits[0].as_str();
    let whole = len(|w| json::commit(w, &f.node.commit(f.rid, first).unwrap()));
    assert!(whole > 1000, "{whole}");
    match f.node.commit_bounded(f.rid, first, &budget(500)) {
        Err(Error::TooLarge { size, limit }) => {
            assert_eq!(limit, 500);
            assert!(size > limit && size < whole, "{size} of {whole}");
        }
        other => panic!("expected TooLarge from the hunk count, got {other:?}"),
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
    // Review of libradicle #8, round 2: before moves are found, a diff is
    // counted at its floor, half its deltas at the shortest file record (119
    // bytes) each, as moves may pair them. The 512 empty files are refused
    // there, at 256 records, before their records are walked.
    match node.commit_bounded(rid, &commits[1], &budget(1024)) {
        Err(Error::TooLarge { size, limit }) => assert_eq!((size, limit), (256 * 119, 1024)),
        other => panic!("expected TooLarge from the floor, got {other:?}"),
    }
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

/// heartwood's storage of `f`'s node, as the node's own key opens it.
fn storage(f: &Fixture) -> radicle::storage::git::Storage {
    let identity = f.node.identity();
    radicle::storage::git::Storage::open(
        f.dir.join("home/storage"),
        radicle::git::UserInfo {
            alias: identity.alias.parse().unwrap(),
            key: identity.nid.parse().unwrap(),
        },
    )
    .unwrap()
}

// Review of libradicle #8, round 2: the listing reads each repository's local
// signed refs as heartwood's `Storage::repositories` does. Absent, or needing
// migration, they are listed; unreadable, they fail the listing with the
// same error. Round 3: downgraded, they are listed too.
#[test]
fn a_listing_reads_signed_refs_as_heartwood_does() {
    use radicle::storage::{ReadStorage, SignedRefsInfo};

    let f = fixture("radwalksigrefs");
    let compare = |state: &str| -> Result<SignedRefsInfo, String> {
        let upstream = storage(&f).repositories().map_err(|e| e.to_string());
        let bounded = f
            .node
            .list_repos_bounded(&budget(1 << 20))
            .map_err(|e| e.to_string());
        assert_eq!(
            upstream
                .as_ref()
                .map(|repos| repos.iter().map(|repo| repo.rid).collect::<Vec<_>>()),
            bounded
                .as_ref()
                .map(|repos| repos.iter().map(|repo| repo.rid).collect::<Vec<_>>()),
            "{state}"
        );
        upstream.map(|mut repos| repos.remove(0).refs)
    };

    let stored = storage(&f).repository(f.rid).unwrap();
    let sigrefs = format!("refs/namespaces/{}/refs/rad/sigrefs", f.node.identity().nid);
    let tip = stored.backend.refname_to_id(&sigrefs).unwrap();
    assert!(matches!(compare("as signed"), Ok(SignedRefsInfo::Some(_))));

    // At the root of their history, they need migrating.
    let mut root = stored.backend.find_commit(tip).unwrap();
    while let Ok(parent) = root.parent(0) {
        root = parent;
    }
    assert_ne!(root.id(), tip);
    stored
        .backend
        .reference(&sigrefs, root.id(), true, "root")
        .unwrap();
    assert!(matches!(
        compare("at their root"),
        Ok(SignedRefsInfo::NeedsMigration)
    ));

    // Downgraded: a new commit on their tip, with their root's signed refs,
    // still validly signed, but of an older feature level than the history
    // under them. heartwood refuses to load them, and takes them as needing
    // migration. No re-signing: the root's signature still holds.
    let signature = radicle::git::raw::Signature::now("Test", "test@example.com").unwrap();
    let downgraded = stored
        .backend
        .commit(
            None,
            &signature,
            &signature,
            "Downgrade",
            &root.tree().unwrap(),
            &[&stored.backend.find_commit(tip).unwrap()],
        )
        .unwrap();
    stored
        .backend
        .reference(&sigrefs, downgraded, true, "downgrade")
        .unwrap();
    let nid = f.node.identity().nid.parse().unwrap();
    assert!(matches!(
        radicle::storage::refs::SignedRefs::load(nid, &stored),
        Err(radicle::storage::refs::sigrefs::read::error::Read::Downgrade { .. })
    ));
    assert!(matches!(
        compare("downgraded"),
        Ok(SignedRefsInfo::NeedsMigration)
    ));

    // Absent, the repository is seeded but not forked.
    stored
        .backend
        .find_reference(&sigrefs)
        .unwrap()
        .delete()
        .unwrap();
    assert!(matches!(compare("absent"), Ok(SignedRefsInfo::None)));

    // Not a commit, they are unreadable.
    let blob = stored.backend.blob(b"not signed refs").unwrap();
    stored
        .backend
        .reference(&sigrefs, blob, true, "blob")
        .unwrap();
    assert!(compare("a blob").is_err());
    // With no room left, the listing refuses before it opens the repository,
    // and so before it reads them.
    assert!(matches!(
        f.node.list_repos_bounded(&budget(2)),
        Err(Error::TooLarge { .. })
    ));
}

// ---------------------------------------------------------------------------
// Review of libradicle #15: what each binding read loads before its budget
// applies, and what the remaining reads return.

use radicle::storage::ReadStorage as _;

/// The test `name` run again in a child process of this binary, with
/// `LIBRADICLE_CHILD=name`: its stdout. A child that dies (a stack overflow
/// aborts the process, which no `catch_unwind` turns into an error) fails
/// the test with how it died, instead of taking the test run down.
fn in_child(name: &str) -> String {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([name, "--exact", "--nocapture", "--test-threads=1"])
        .env("LIBRADICLE_CHILD", name)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "the child running {name} died: {:?}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn is_child(name: &str) -> bool {
    std::env::var("LIBRADICLE_CHILD").as_deref() == Ok(name)
}

/// The commit `tree` makes on top of `f`'s head in storage, `message` its
/// message; `head` moves `refs/heads/main` (the head every read resolves)
/// to it.
fn store_commit(f: &Fixture, tree: Option<git2::Oid>, message: &str, head: bool) -> git2::Oid {
    let stored = storage(f).repository(f.rid).unwrap();
    let git = &stored.backend;
    let main = git.find_reference("refs/heads/main").unwrap();
    let parent = main.peel_to_commit().unwrap();
    let tree = match tree {
        Some(tree) => git.find_tree(tree).unwrap(),
        None => parent.tree().unwrap(),
    };
    let signature =
        git2::Signature::new("Test", "test@example.com", &git2::Time::new(1, 0)).unwrap();
    let oid = git
        .commit(None, &signature, &signature, message, &tree, &[&parent])
        .unwrap();
    if head {
        git.reference("refs/heads/main", oid, true, "test").unwrap();
    }
    oid
}

/// The bytes this process's peak resident set grew while `run` ran (Linux:
/// `/proc/self/clear_refs` resets the peak), or `None` elsewhere.
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

const MIB: u64 = 1024 * 1024;

/// LR15-M1: a chain of 30,000 nested trees, as the reviewer built it. The
/// bounded walk refuses it cleanly; the unbounded one lists its one file.
/// Neither may overflow the stack, so both run in a child.
#[test]
fn a_deep_tree_is_refused_never_overflowed() {
    const NAME: &str = "a_deep_tree_is_refused_never_overflowed";
    if !is_child(NAME) {
        let out = in_child(NAME);
        assert!(out.contains("bounded: TooDeep { limit: 4096 }"), "{out}");
        assert!(out.contains("unbounded: 1 file, 60001 bytes"), "{out}");
        return;
    }
    let f = fixture("raddeep");
    let stored = storage(&f).repository(f.rid).unwrap();
    let git = &stored.backend;
    let blob = git.blob(b"x").unwrap();
    let mut builder = git.treebuilder(None).unwrap();
    builder.insert("f", blob, 0o100644).unwrap();
    let mut tree = builder.write().unwrap();
    for _ in 0..30_000 {
        let mut builder = git.treebuilder(None).unwrap();
        builder.insert("d", tree, 0o040000).unwrap();
        tree = builder.write().unwrap();
    }
    store_commit(&f, Some(tree), "deep", true);

    match f.node.list_files_bounded(f.rid, &Bounds::for_binding()) {
        Err(e @ Error::TooDeep { .. }) => println!("bounded: {e:?}"),
        other => println!("bounded: unexpected {other:?}"),
    }
    let files = f.node.list_files(f.rid).expect("an unbounded walk");
    println!(
        "unbounded: {} file, {} bytes",
        files.len(),
        files.iter().map(String::len).sum::<usize>()
    );
}

/// The trees a bounded walk loads count against its loads budget: the
/// listing fits at exactly what it loads, and not one byte less.
#[test]
fn a_tree_walk_counts_what_it_loads() {
    let f = fixture("radloads");
    let stored = storage(&f).repository(f.rid).unwrap();
    let git = &stored.backend;
    let odb = git.odb().unwrap();
    let head = git
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    let mut loads = odb.read_header(head.id()).unwrap().0 as u64;
    let root = head.tree().unwrap();
    loads += odb.read_header(root.id()).unwrap().0 as u64;
    for entry in root.iter() {
        if entry.kind() == Some(git2::ObjectType::Tree) {
            loads += odb.read_header(entry.id()).unwrap().0 as u64;
        }
    }
    let at = |loads: u64| budget(1 << 20).with_loads(loads);
    assert_eq!(
        f.node.list_files_bounded(f.rid, &at(loads)).unwrap().len(),
        4
    );
    assert!(matches!(
        f.node.list_files_bounded(f.rid, &at(loads - 1)),
        Err(Error::TooLarge { .. })
    ));
}

/// LR15-M2: the reads that were unbounded now stop at their budget, and a
/// result exactly at its budget fits.
#[test]
fn every_binding_read_has_a_budget() {
    let f = fixture("radreads");
    let (node, rid) = (&f.node, f.rid);

    let tree = node.tree(rid, "").expect("tree");
    assert_eq!(tree.len(), 4);
    exact(len(|w| json::tree(w, &tree)), |b| {
        node.tree_bounded(rid, "", b)
    });
    let dir = node.tree(rid, "dir").expect("a subtree");
    exact(len(|w| json::tree(w, &dir)), |b| {
        node.tree_bounded(rid, "dir", b)
    });

    let head = f.commits[2].as_str();
    let (entries, commit) = node.tree_at(rid, head, "").expect("tree_at");
    exact(len(|w| json::tree_at(w, &entries, &commit)), |b| {
        node.tree_at_bounded(rid, head, "", b)
    });
    assert!(matches!(
        node.tree_at_bounded(rid, head, "a.txt", &budget(1 << 20)),
        Err(Error::NotATree(_))
    ));
    let missing = node
        .tree_at_bounded(rid, head, "nope/deeper", &budget(1 << 20))
        .unwrap_err();
    assert!(missing.to_string().contains("does not exist"), "{missing}");

    let issues = node.issues(rid).expect("issues");
    let (id, issue) = &issues[0];
    exact(len(|w| json::issue(w, id, issue)), |b| {
        node.issue_bounded(rid, &id.to_string(), b)
    });
    // A prefix names it too; one that names nothing is not found, as before.
    let prefix = &id.to_string()[..12];
    assert_eq!(node.issue(rid, prefix).unwrap().0, *id);
    let unknown = node.issue(rid, "0000000000").unwrap_err().to_string();
    assert!(unknown.contains("not found"), "{unknown}");

    let remotes = node.remotes(rid).expect("remotes");
    assert_eq!(remotes.len(), 1);
    exact(len(|w| json::remotes(w, &remotes)), |b| {
        node.remotes_bounded(rid, b)
    });

    let seeded = node.list_seeded_repos().expect("seeded");
    assert_eq!(seeded.len(), 1);
    exact(len(|w| json::seeded_repos(w, &seeded)), |b| {
        node.list_seeded_repos_bounded(b)
    });

    let info = node.repo_info(rid).expect("repo info");
    exact(len(|w| json::repo_info(w, &info)), |b| {
        node.repo_info_bounded(rid, b)
    });
}

/// LR15-M2: a tree with 70,000 long names, as the reviewer built it, is
/// refused at the binding budget, never returned.
#[test]
fn a_wide_tree_is_refused_at_the_binding_budget() {
    let f = fixture("radwide");
    let stored = storage(&f).repository(f.rid).unwrap();
    let git = &stored.backend;
    let blob = git.blob(b"x").unwrap();
    let mut builder = git.treebuilder(None).unwrap();
    for i in 0..70_000 {
        builder
            .insert(format!("{i:0>200}"), blob, 0o100644)
            .unwrap();
    }
    let tree = builder.write().unwrap();
    let oid = store_commit(&f, Some(tree), "wide", true).to_string();

    for result in [
        f.node
            .tree_bounded(f.rid, "", &Bounds::for_binding())
            .map(|_| ()),
        f.node
            .tree_at_bounded(f.rid, &oid, "", &Bounds::for_binding())
            .map(|_| ()),
    ] {
        assert!(matches!(result, Err(Error::TooLarge { .. })), "{result:?}");
    }
}

/// LR15-M3: a commit with a 64 MiB message, away from the head, read by
/// its id: every read refuses it from its header, without loading it. In a
/// child, so the peak memory is this read's alone.
#[test]
fn a_huge_commit_is_refused_before_it_is_loaded() {
    const NAME: &str = "a_huge_commit_is_refused_before_it_is_loaded";
    if !is_child(NAME) {
        let out = in_child(NAME);
        assert!(out.contains("refused: 6 of 6"), "{out}");
        if cfg!(target_os = "linux") {
            let grew: u64 = out
                .lines()
                .find_map(|l| l.strip_prefix("peak growth: "))
                .expect("a peak")
                .parse()
                .unwrap();
            assert!(grew < 24 * MIB, "the reads grew the peak by {grew} bytes");
            let control: u64 = out
                .lines()
                .find_map(|l| l.strip_prefix("control growth: "))
                .expect("a control")
                .parse()
                .unwrap();
            assert!(
                control >= 24 * MIB,
                "the measure missed a load: {control} bytes"
            );
        }
        return;
    }
    let f = fixture("radhuge");
    let oid = {
        let message = "x".repeat(64 * MIB as usize);
        store_commit(&f, None, &message, false).to_string()
    };
    let (node, rid) = (&f.node, f.rid);
    let mut refused = 0;
    let grew = peak_growth(|| {
        let b = Bounds::for_binding();
        for result in [
            node.commit_bounded(rid, &oid, &b).map(|_| ()),
            node.commits_bounded(rid, &oid, 0, 1, &b).map(|_| ()),
            node.commits_bounded(rid, &oid, u32::MAX as usize, 1, &b)
                .map(|_| ()),
            node.tree_at_bounded(rid, &oid, "", &b).map(|_| ()),
            node.repo_stats_bounded(rid, &oid, &b).map(|_| ()),
            node.read_blob_at_capped(rid, &oid, "a.txt", libradicle::BINDING_READ_BUDGET)
                .map(|_| ()),
        ] {
            if matches!(result, Err(Error::TooLarge { .. })) {
                refused += 1;
            } else {
                println!("not refused: {result:?}");
            }
        }
    });
    println!("refused: {refused} of 6");
    if let Some(grew) = grew {
        println!("peak growth: {grew}");
    }
    // The control: loading the commit, as the reads did before, shows on
    // the same measure.
    let stored = storage(&f).repository(rid).unwrap();
    let loaded = peak_growth(|| {
        let commit = stored
            .backend
            .find_commit(git2::Oid::from_str(&oid).unwrap())
            .unwrap();
        assert!(commit.message_bytes().len() as u64 >= 64 * MIB);
    });
    if let Some(loaded) = loaded {
        println!("control growth: {loaded}");
    }
}

/// LR15-M3: skipped commits count against the loads budget too: a page
/// past the end walks every commit, and fits only in what they take.
#[test]
fn skipped_commits_count_against_the_loads_budget() {
    let f = fixture("radskip");
    let stored = storage(&f).repository(f.rid).unwrap();
    let odb = stored.backend.odb().unwrap();
    let loads: u64 = f
        .commits
        .iter()
        .map(|c| odb.read_header(git2::Oid::from_str(c).unwrap()).unwrap().0 as u64)
        .sum();
    let head = f.commits[2].as_str();
    let at = |loads: u64| budget(1 << 20).with_loads(loads);
    let past_the_end = u32::MAX as usize;
    assert!(f
        .node
        .commits_bounded(f.rid, head, past_the_end, 1, &at(loads))
        .unwrap()
        .is_empty());
    assert!(matches!(
        f.node
            .commits_bounded(f.rid, head, past_the_end, 1, &at(loads - 1)),
        Err(Error::TooLarge { .. })
    ));
}

/// LR15-M3: an issue of 32 MiB is refused from its cached size, before it
/// is deserialized, listed or read alone. In a child, for its peak memory.
#[test]
fn a_huge_issue_is_refused_before_it_is_loaded() {
    const NAME: &str = "a_huge_issue_is_refused_before_it_is_loaded";
    if !is_child(NAME) {
        let out = in_child(NAME);
        assert!(out.contains("refused: 2 of 2"), "{out}");
        if cfg!(target_os = "linux") {
            let grew: u64 = out
                .lines()
                .find_map(|l| l.strip_prefix("peak growth: "))
                .expect("a peak")
                .parse()
                .unwrap();
            assert!(grew < 24 * MIB, "the reads grew the peak by {grew} bytes");
            let control: u64 = out
                .lines()
                .find_map(|l| l.strip_prefix("control growth: "))
                .expect("a control")
                .parse()
                .unwrap();
            assert!(
                control >= 24 * MIB,
                "the measure missed a load: {control} bytes"
            );
        }
        return;
    }
    let mut f = fixture("radhugeissue");
    let id = {
        let description = "y".repeat(32 * MIB as usize);
        f.node
            .create_issue(f.rid, "Huge", &description, vec![])
            .expect("a huge issue")
            .to_string()
    };
    let (node, rid) = (&f.node, f.rid);
    let mut refused = 0;
    let grew = peak_growth(|| {
        let b = Bounds::for_binding();
        for result in [
            node.issues_bounded(rid, &b).map(|_| ()),
            node.issue_bounded(rid, &id, &b).map(|_| ()),
        ] {
            if matches!(result, Err(Error::TooLarge { .. })) {
                refused += 1;
            } else {
                println!("not refused: {result:?}");
            }
        }
    });
    println!("refused: {refused} of 2");
    if let Some(grew) = grew {
        println!("peak growth: {grew}");
    }
    // The control: the unbounded read, which loads it, shows on the same
    // measure.
    let loaded = peak_growth(|| {
        node.issue(rid, &id).expect("the unbounded read");
    });
    if let Some(loaded) = loaded {
        println!("control growth: {loaded}");
    }
    let _ = &mut f;
}

/// The bounded history is in radicle-surf's order, libgit2's revwalk
/// without sorting flags: through merges, and across commits whose times
/// tie.
#[test]
fn history_is_walked_in_radicle_surfs_order() {
    let mut f = fixture("radorder");
    let work = f.dir.join("order");
    let repo = git2::Repository::init(&work).unwrap();
    let blob = repo.blob(b"x").unwrap();
    let tree = {
        let mut builder = repo.treebuilder(None).unwrap();
        builder.insert("f", blob, 0o100644).unwrap();
        builder.write().unwrap()
    };
    let tree = repo.find_tree(tree).unwrap();
    let at = |seconds: i64| {
        git2::Signature::new("T", "t@example.com", &git2::Time::new(seconds, 0)).unwrap()
    };
    let make = |message: &str, seconds: i64, parents: &[git2::Oid]| -> git2::Oid {
        let parents = parents
            .iter()
            .map(|p| repo.find_commit(*p).unwrap())
            .collect::<Vec<_>>();
        repo.commit(
            None,
            &at(seconds),
            &at(seconds),
            message,
            &tree,
            &parents.iter().collect::<Vec<_>>(),
        )
        .unwrap()
    };
    let root = make("root", 100, &[]);
    let a = make("a", 300, &[root]);
    let b = make("b", 200, &[root]);
    let c = make("c", 300, &[b]);
    let d = make("d", 200, &[a]);
    let merge = make("merge", 250, &[d, c]);
    let e = make("e", 250, &[merge]);
    let tip = make("tip", 400, &[e, a]);
    repo.reference("refs/heads/main", tip, true, "test")
        .unwrap();
    repo.set_head("refs/heads/main").unwrap();
    let rid = f
        .node
        .import_repo(&work, "order", "walk order", "main")
        .expect("import");

    let stored = storage(&f).repository(rid).unwrap();
    let surf = radicle_surf::Repository::open(stored.backend.path()).unwrap();
    let expected = surf
        .history(radicle_surf::Oid::from(tip))
        .unwrap()
        .map(|c| c.unwrap().id.to_string())
        .collect::<Vec<_>>();
    assert_eq!(expected.len(), 8);
    let walked = f
        .node
        .commits(rid, &tip.to_string(), 0, 100)
        .unwrap()
        .into_iter()
        .map(|c| c.id.to_string())
        .collect::<Vec<_>>();
    assert_eq!(walked, expected);
}

// Review of libradicle #15, round 2: what the bounded reads still loaded
// before a budget applied. Each in a child, for its peak memory.

/// The child's peak growth and its control's, from its stdout.
fn growths(out: &str) -> (u64, u64) {
    let read = |prefix: &str| -> u64 {
        out.lines()
            .find_map(|l| l.strip_prefix(prefix))
            .unwrap_or_else(|| panic!("no {prefix:?} in {out}"))
            .parse()
            .unwrap()
    };
    (read("peak growth: "), read("control growth: "))
}

fn print_growth(grew: Option<u64>, control: Option<u64>) {
    if let (Some(grew), Some(control)) = (grew, control) {
        println!("peak growth: {grew}");
        println!("control growth: {control}");
    }
}

/// A commit adding one subtree of 100,000 names of 200 bytes (22.8 MB of
/// tree): the diff preflight refuses it from that subtree's header, before
/// libgit2's diff loads it.
#[test]
fn a_huge_subtree_in_a_diff_is_refused_before_it_is_loaded() {
    const NAME: &str = "a_huge_subtree_in_a_diff_is_refused_before_it_is_loaded";
    if !is_child(NAME) {
        let out = in_child(NAME);
        assert!(out.contains("commit: TooLarge"), "{out}");
        if cfg!(target_os = "linux") {
            let (grew, control) = growths(&out);
            assert!(grew < 12 * MIB, "the diff grew the peak by {grew} bytes");
            assert!(
                control >= 20 * MIB,
                "the measure missed a load: {control} bytes"
            );
        }
        return;
    }
    let f = fixture("radhugediff");
    let stored = storage(&f).repository(f.rid).unwrap();
    let git = &stored.backend;
    let blob = git.blob(b"x").unwrap();
    let mut big = git.treebuilder(None).unwrap();
    for i in 0..100_000 {
        big.insert(format!("{i:0>200}"), blob, 0o100644).unwrap();
    }
    let big = big.write().unwrap();
    let head = git
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    let mut root = git.treebuilder(Some(&head.tree().unwrap())).unwrap();
    root.insert("big", big, 0o040000).unwrap();
    let root = root.write().unwrap();
    drop(head);
    let oid = store_commit(&f, Some(root), "big subtree", false).to_string();

    let mut result = None;
    let grew = peak_growth(|| {
        result = Some(
            f.node
                .commit_bounded(f.rid, &oid, &Bounds::for_binding())
                .map(|_| ()),
        );
    });
    match result.unwrap() {
        Err(e @ Error::TooLarge { .. }) => println!("commit: {e:?}"),
        other => println!("commit: unexpected {other:?}"),
    }
    let control = peak_growth(|| {
        let tree = git.find_tree(big).unwrap();
        assert_eq!(tree.len(), 100_000);
    });
    print_growth(grew, control);
}

/// An identity document of 32 MiB at `refs/rad/id`: every read that loads
/// the identity refuses it from the blob's header.
#[test]
fn a_huge_identity_document_is_refused_before_it_is_loaded() {
    const NAME: &str = "a_huge_identity_document_is_refused_before_it_is_loaded";
    if !is_child(NAME) {
        let out = in_child(NAME);
        assert!(out.contains("refused: 4 of 4"), "{out}");
        if cfg!(target_os = "linux") {
            let (grew, control) = growths(&out);
            assert!(grew < 12 * MIB, "the reads grew the peak by {grew} bytes");
            assert!(
                control >= 24 * MIB,
                "the measure missed a load: {control} bytes"
            );
        }
        return;
    }
    let f = fixture("radhugedoc");
    let stored = storage(&f).repository(f.rid).unwrap();
    let git = &stored.backend;
    let doc = {
        let payload = format!(
            "{{\"payload\":{{\"x\":\"{}\"}}}}",
            "z".repeat(32 * MIB as usize)
        );
        git.blob(payload.as_bytes()).unwrap()
    };
    let mut embeds = git.treebuilder(None).unwrap();
    embeds.insert("radicle.json", doc, 0o100644).unwrap();
    let embeds = embeds.write().unwrap();
    let mut root = git.treebuilder(None).unwrap();
    root.insert("embeds", embeds, 0o040000).unwrap();
    let root = git.find_tree(root.write().unwrap()).unwrap();
    let signature =
        git2::Signature::new("Test", "test@example.com", &git2::Time::new(1, 0)).unwrap();
    let id = git
        .commit(None, &signature, &signature, "huge doc", &root, &[])
        .unwrap();
    git.reference("refs/rad/id", id, true, "test").unwrap();

    let (node, rid) = (&f.node, f.rid);
    let head = f.commits[2].clone();
    let mut refused = 0;
    let grew = peak_growth(|| {
        let b = Bounds::for_binding();
        for result in [
            node.repo_info_bounded(rid, &b).map(|_| ()),
            node.remotes_bounded(rid, &b).map(|_| ()),
            node.repo_stats_bounded(rid, &head, &b).map(|_| ()),
            node.list_repos_bounded(&b).map(|_| ()),
        ] {
            if matches!(result, Err(Error::TooLarge { .. })) {
                refused += 1;
            } else {
                println!("not refused: {result:?}");
            }
        }
    });
    println!("refused: {refused} of 4");
    let control = peak_growth(|| {
        assert!(git.find_blob(doc).unwrap().content().len() as u64 >= 32 * MIB);
    });
    print_growth(grew, control);
}

/// A signed-refs record of 80,000 branches with 190-byte names (about 18
/// MB): the reads that load signed refs refuse it from the blob's header.
#[test]
fn huge_signed_refs_are_refused_before_they_are_loaded() {
    const NAME: &str = "huge_signed_refs_are_refused_before_they_are_loaded";
    if !is_child(NAME) {
        let out = in_child(NAME);
        assert!(out.contains("refused: 3 of 3"), "{out}");
        if cfg!(target_os = "linux") {
            let (grew, control) = growths(&out);
            assert!(grew < 12 * MIB, "the reads grew the peak by {grew} bytes");
            assert!(
                control >= 16 * MIB,
                "the measure missed a load: {control} bytes"
            );
        }
        return;
    }
    let f = fixture("radhugesigrefs");
    let stored = storage(&f).repository(f.rid).unwrap();
    let git = &stored.backend;
    let nid = f.node.identity().nid;
    let sigrefs = format!("refs/namespaces/{nid}/refs/rad/sigrefs");
    let tip = git
        .find_reference(&sigrefs)
        .unwrap()
        .peel_to_commit()
        .unwrap();
    let refs = {
        let oid = f.commits[2].as_str();
        let mut text = String::new();
        for i in 0..80_000 {
            text.push_str(&format!("{oid} refs/heads/{i:0>190}\n"));
        }
        git.blob(text.as_bytes()).unwrap()
    };
    let signature_blob = git.blob(&[0u8; 64]).unwrap();
    let mut tree = git.treebuilder(None).unwrap();
    tree.insert("refs", refs, 0o100644).unwrap();
    tree.insert("signature", signature_blob, 0o100644).unwrap();
    let tree = git.find_tree(tree.write().unwrap()).unwrap();
    let signature =
        git2::Signature::new("Test", "test@example.com", &git2::Time::new(1, 0)).unwrap();
    let huge = git
        .commit(None, &signature, &signature, "huge sigrefs", &tree, &[&tip])
        .unwrap();
    git.reference(&sigrefs, huge, true, "test").unwrap();
    drop((tree, tip));

    let (node, rid) = (&f.node, f.rid);
    let head = f.commits[2].clone();
    let mut refused = 0;
    let grew = peak_growth(|| {
        let b = Bounds::for_binding();
        for result in [
            node.remotes_bounded(rid, &b).map(|_| ()),
            node.repo_stats_bounded(rid, &head, &b).map(|_| ()),
            node.list_repos_bounded(&b).map(|_| ()),
        ] {
            if matches!(result, Err(Error::TooLarge { .. })) {
                refused += 1;
            } else {
                println!("not refused: {result:?}");
            }
        }
    });
    println!("refused: {refused} of 3");
    let control = peak_growth(|| {
        assert!(git.find_blob(refs).unwrap().content().len() as u64 >= 16 * MIB);
    });
    print_growth(grew, control);
}

/// 18,000 commits of 1 KiB messages (about 21 MB of commits): the whole
/// history doesn't fit the binding's loads budget, so the stats refuse it,
/// and fit a budget that holds it.
#[test]
fn repo_stats_count_all_of_history_against_the_loads_budget() {
    const NAME: &str = "repo_stats_count_all_of_history_against_the_loads_budget";
    if !is_child(NAME) {
        let out = in_child(NAME);
        assert!(out.contains("binding: TooLarge"), "{out}");
        assert!(out.contains("larger: 18003 commits"), "{out}");
        return;
    }
    let f = fixture("radlonghistory");
    let stored = storage(&f).repository(f.rid).unwrap();
    let git = &stored.backend;
    let message = "m".repeat(1024);
    let signature =
        git2::Signature::new("Test", "test@example.com", &git2::Time::new(1, 0)).unwrap();
    let mut parent = git
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    let tree = parent.tree().unwrap();
    for _ in 0..18_000 {
        let oid = git
            .commit(None, &signature, &signature, &message, &tree, &[&parent])
            .unwrap();
        parent = git.find_commit(oid).unwrap();
    }
    let tip = parent.id().to_string();
    match f
        .node
        .repo_stats_bounded(f.rid, &tip, &Bounds::for_binding())
    {
        Err(e @ Error::TooLarge { .. }) => println!("binding: {e:?}"),
        other => println!("binding: unexpected {other:?}"),
    }
    let larger = budget(1 << 20).with_loads(64 * MIB);
    match f.node.repo_stats_bounded(f.rid, &tip, &larger) {
        Ok(stats) => println!("larger: {} commits", stats.commits),
        other => println!("larger: unexpected {other:?}"),
    }
}

// Review of libradicle #15, round 3: signed refs are read by heartwood's own
// reader, through a metered object reader, so every object it reads counts.

/// The node's current signed refs, as an old-format record (no
/// `refs/rad/sigrefs-parent`), signed with its key, in a tree with its
/// signature: what an older client wrote. heartwood walks the history under
/// a head of this kind.
fn old_format_tree(f: &Fixture, git: &git2::Repository) -> git2::Oid {
    use ed25519_dalek::Signer as _;

    let nid = f.node.identity().nid;
    let sigrefs = format!("refs/namespaces/{nid}/refs/rad/sigrefs");
    let tip = git
        .find_reference(&sigrefs)
        .unwrap()
        .peel_to_commit()
        .unwrap();
    let tree = tip.tree().unwrap();
    let refs = git.find_blob(tree.get_name("refs").unwrap().id()).unwrap();
    let old = std::str::from_utf8(refs.content())
        .unwrap()
        .lines()
        .filter(|line| !line.ends_with(" refs/rad/sigrefs-parent"))
        .map(|line| format!("{line}\n"))
        .collect::<String>();
    // The fixture's host key: its seed is the node's signing key.
    let key = ed25519_dalek::SigningKey::from_bytes(&[5u8; 32]);
    let signature = key.sign(old.as_bytes()).to_bytes();
    let mut builder = git.treebuilder(None).unwrap();
    builder
        .insert("refs", git.blob(old.as_bytes()).unwrap(), 0o100644)
        .unwrap();
    builder
        .insert("signature", git.blob(&signature).unwrap(), 0o100644)
        .unwrap();
    builder.write().unwrap()
}

fn sigrefs_commit(
    git: &git2::Repository,
    tree: git2::Oid,
    message: &str,
    parent: Option<git2::Oid>,
) -> git2::Oid {
    let signature = git2::Signature::new("radicle", "z6Mk", &git2::Time::new(1, 0)).unwrap();
    let tree = git.find_tree(tree).unwrap();
    let parents = parent
        .map(|p| git.find_commit(p).unwrap())
        .into_iter()
        .collect::<Vec<_>>();
    git.commit(
        None,
        &signature,
        &signature,
        message,
        &tree,
        &parents.iter().collect::<Vec<_>>(),
    )
    .unwrap()
}

fn set_sigrefs(f: &Fixture, git: &git2::Repository, head: git2::Oid) {
    let nid = f.node.identity().nid;
    git.reference(
        &format!("refs/namespaces/{nid}/refs/rad/sigrefs"),
        head,
        true,
        "test",
    )
    .unwrap();
}

/// LR15 round 3, 1: an old-format head whose parent record names, as its
/// identity root, a commit with a 32 MiB `embeds/radicle.json`. heartwood
/// reads that document before it verifies the record; the metered reader
/// refuses it from its header.
#[test]
fn an_old_records_identity_root_is_refused_before_it_is_loaded() {
    const NAME: &str = "an_old_records_identity_root_is_refused_before_it_is_loaded";
    if !is_child(NAME) {
        let out = in_child(NAME);
        assert!(out.contains("valid: true"), "{out}");
        assert!(out.contains("remotes: TooLarge"), "{out}");
        if cfg!(target_os = "linux") {
            let (grew, control) = growths(&out);
            assert!(grew < 12 * MIB, "the read grew the peak by {grew} bytes");
            assert!(
                control >= 24 * MIB,
                "the measure missed a load: {control} bytes"
            );
        }
        return;
    }
    let f = fixture("radoldroot");
    let stored = storage(&f).repository(f.rid).unwrap();
    let git = &stored.backend;
    let old = old_format_tree(&f, git);

    // The signature is the node's: an old-format root record verifies.
    let root = sigrefs_commit(git, old, "old root", None);
    set_sigrefs(&f, git, root);
    let nid: radicle::prelude::PublicKey = f.node.identity().nid.parse().unwrap();
    println!(
        "valid: {}",
        matches!(
            radicle::storage::refs::SignedRefs::load(nid, &stored),
            Ok(Some(_))
        )
    );

    // The record under the head names a huge identity root.
    let doc = git.blob(&vec![b'z'; 32 * MIB as usize]).unwrap();
    let mut embeds = git.treebuilder(None).unwrap();
    embeds.insert("radicle.json", doc, 0o100644).unwrap();
    let embeds = embeds.write().unwrap();
    let mut identity = git.treebuilder(None).unwrap();
    identity.insert("embeds", embeds, 0o040000).unwrap();
    let huge_root = sigrefs_commit(git, identity.write().unwrap(), "huge identity", None);
    let ancestor_refs = format!("{huge_root} refs/rad/root\n");
    let mut ancestor = git.treebuilder(None).unwrap();
    ancestor
        .insert(
            "refs",
            git.blob(ancestor_refs.as_bytes()).unwrap(),
            0o100644,
        )
        .unwrap();
    ancestor
        .insert("signature", git.blob(&[0u8; 64]).unwrap(), 0o100644)
        .unwrap();
    let ancestor = sigrefs_commit(git, ancestor.write().unwrap(), "ancestor", None);
    let head = sigrefs_commit(git, old, "old head", Some(ancestor));
    set_sigrefs(&f, git, head);

    let mut result = None;
    let grew = peak_growth(|| {
        result = Some(
            f.node
                .remotes_bounded(f.rid, &Bounds::for_binding())
                .map(|_| ()),
        );
    });
    match result.unwrap() {
        Err(e @ Error::TooLarge { .. }) => println!("remotes: {e:?}"),
        other => println!("remotes: unexpected {other:?}"),
    }
    let control = peak_growth(|| {
        assert!(git.find_blob(doc).unwrap().content().len() as u64 >= 32 * MIB);
    });
    print_growth(grew, control);
}

/// LR15 round 3, 1: a valid old-format chain of 600 records, each a commit
/// with a 28 KiB message (16.8 MB of commits alone, each well under the
/// per-object limit). heartwood walks all of it, reading each record's
/// commit for itself and again for its two blobs, and walks it twice when
/// signatures repeat: every one of those loads counts against the loads
/// budget, so the binding refuses it, and a budget that holds them all
/// reads it.
#[test]
fn an_old_records_history_counts_against_the_loads_budget() {
    const NAME: &str = "an_old_records_history_counts_against_the_loads_budget";
    if !is_child(NAME) {
        let out = in_child(NAME);
        assert!(out.contains("valid: true"), "{out}");
        assert!(out.contains("binding: TooLarge"), "{out}");
        assert!(out.contains("larger: 1 remote"), "{out}");
        return;
    }
    let f = fixture("radoldchain");
    let stored = storage(&f).repository(f.rid).unwrap();
    let git = &stored.backend;
    let old = old_format_tree(&f, git);
    let message = "m".repeat(28 * 1024);
    let mut head = sigrefs_commit(git, old, &message, None);
    for _ in 1..600 {
        head = sigrefs_commit(git, old, &message, Some(head));
    }
    set_sigrefs(&f, git, head);
    let nid: radicle::prelude::PublicKey = f.node.identity().nid.parse().unwrap();
    println!(
        "valid: {}",
        matches!(
            radicle::storage::refs::SignedRefs::load(nid, &stored),
            Ok(Some(_))
        )
    );
    match f.node.remotes_bounded(f.rid, &Bounds::for_binding()) {
        Err(e @ Error::TooLarge { .. }) => println!("binding: {e:?}"),
        other => println!("binding: unexpected {other:?}"),
    }
    let larger = budget(1 << 20).with_loads(512 * MIB);
    match f.node.remotes_bounded(f.rid, &larger) {
        Ok(remotes) => println!("larger: {} remote", remotes.len()),
        other => println!("larger: unexpected {other:?}"),
    }
}

/// LR15 round 3, 4: a valid root record whose tree also holds a 20 MiB blob
/// nobody reads. heartwood reads only `refs` and `signature`, so the bounded
/// read accepts it, as the unbounded one does.
#[test]
fn a_blob_heartwood_never_reads_is_not_charged() {
    let f = fixture("radunusedblob");
    let stored = storage(&f).repository(f.rid).unwrap();
    let git = &stored.backend;
    let nid = f.node.identity().nid;
    let sigrefs = format!("refs/namespaces/{nid}/refs/rad/sigrefs");
    let mut root = git
        .find_reference(&sigrefs)
        .unwrap()
        .peel_to_commit()
        .unwrap();
    while let Ok(parent) = root.parent(0) {
        root = parent;
    }
    let mut tree = git.treebuilder(Some(&root.tree().unwrap())).unwrap();
    tree.insert(
        "unused",
        git.blob(&vec![0u8; 20 * MIB as usize]).unwrap(),
        0o100644,
    )
    .unwrap();
    let head = sigrefs_commit(git, tree.write().unwrap(), "unused blob", None);
    set_sigrefs(&f, git, head);

    assert_eq!(f.node.remotes(f.rid).expect("unbounded").len(), 1);
    assert_eq!(
        f.node
            .remotes_bounded(f.rid, &Bounds::for_binding())
            .expect("bounded")
            .len(),
        1
    );
}

/// LR15 round 3, 3: a commit adding a subtree of 40,000 empty files (an
/// 8.3 MB tree) and one 9 MiB binary file: the tree and the blob, 17.7 MB
/// together, are over the loads budget, though each fits it and the diff's
/// JSON would too. They count against the same meter, so it is refused.
#[test]
fn a_diffs_trees_and_blobs_share_the_loads_budget() {
    const NAME: &str = "a_diffs_trees_and_blobs_share_the_loads_budget";
    if !is_child(NAME) {
        let out = in_child(NAME);
        assert!(out.contains("commit: TooLarge"), "{out}");
        assert!(out.contains("larger: ok"), "{out}");
        return;
    }
    let f = fixture("radshareddiff");
    let stored = storage(&f).repository(f.rid).unwrap();
    let git = &stored.backend;
    let empty = git.blob(b"").unwrap();
    let mut files = git.treebuilder(None).unwrap();
    for i in 0..40_000 {
        files.insert(format!("{i:0>178}"), empty, 0o100644).unwrap();
    }
    let files = files.write().unwrap();
    let tree_size = git.odb().unwrap().read_header(files).unwrap().0 as u64;
    assert!(tree_size > 8 * 1000 * 1000, "{tree_size}");
    let mut binary = vec![0u8; 9 * MIB as usize];
    binary[0] = 1;
    let binary = git.blob(&binary).unwrap();
    let head = git
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    let mut root = git.treebuilder(Some(&head.tree().unwrap())).unwrap();
    root.insert("files", files, 0o040000).unwrap();
    root.insert("big.bin", binary, 0o100644).unwrap();
    let root = root.write().unwrap();
    drop(head);
    let oid = store_commit(&f, Some(root), "files and a binary", false).to_string();

    match f.node.commit_bounded(f.rid, &oid, &Bounds::for_binding()) {
        Err(e @ Error::TooLarge { .. }) => println!("commit: {e:?}"),
        other => println!("commit: unexpected {other:?}"),
    }
    // With room for both, the same diff is read.
    let larger = Bounds::for_binding().with_loads(64 * MIB);
    match f.node.commit_bounded(f.rid, &oid, &larger) {
        Ok(_) => println!("larger: ok"),
        other => println!("larger: unexpected {other:?}"),
    }
}

// Review of libradicle #15, round 4: with no usable local HEAD, heartwood's
// `head()` falls back to the canonical head, through an unmetered read of
// the identity document. The bounded reads resolve it themselves.

/// A dangling local HEAD: the reads that resolve the head find the same
/// canonical head heartwood does, from the identity they size.
#[test]
fn a_dangling_head_resolves_to_the_canonical_head() {
    let f = fixture("raddangling");
    let stored = storage(&f).repository(f.rid).unwrap();
    stored
        .backend
        .set_head("refs/heads/does-not-exist")
        .unwrap();
    let (node, rid) = (&f.node, f.rid);
    let b = Bounds::for_binding();
    assert_eq!(
        node.list_files_bounded(rid, &b).unwrap(),
        node.list_files(rid).unwrap()
    );
    assert_eq!(
        node.tree_bounded(rid, "", &b).unwrap().len(),
        node.tree(rid, "").unwrap().len()
    );
    assert_eq!(
        node.read_blob_capped(rid, "a.txt", libradicle::BINDING_READ_BUDGET)
            .unwrap()
            .content,
        node.read_blob(rid, "a.txt").unwrap().content
    );
    assert_eq!(
        node.repo_info_bounded(rid, &b).unwrap().head,
        node.repo_info(rid).unwrap().head
    );
    assert_eq!(node.list_repos_bounded(&b).unwrap().len(), 1);
}

/// A dangling local HEAD and a 32 MiB identity document: every read that
/// resolves the head refuses it from the document's header, instead of
/// heartwood loading it on the way to the canonical head. In a child.
#[test]
fn a_dangling_head_with_a_huge_identity_is_refused_before_it_is_loaded() {
    const NAME: &str = "a_dangling_head_with_a_huge_identity_is_refused_before_it_is_loaded";
    if !is_child(NAME) {
        let out = in_child(NAME);
        assert!(out.contains("refused: 5 of 5"), "{out}");
        if cfg!(target_os = "linux") {
            let (grew, control) = growths(&out);
            assert!(grew < 12 * MIB, "the reads grew the peak by {grew} bytes");
            assert!(
                control >= 24 * MIB,
                "the measure missed a load: {control} bytes"
            );
        }
        return;
    }
    let f = fixture("raddanglinghuge");
    let stored = storage(&f).repository(f.rid).unwrap();
    let git = &stored.backend;
    let doc = git.blob(&vec![b'z'; 32 * MIB as usize]).unwrap();
    let mut embeds = git.treebuilder(None).unwrap();
    embeds.insert("radicle.json", doc, 0o100644).unwrap();
    let embeds = embeds.write().unwrap();
    let mut root = git.treebuilder(None).unwrap();
    root.insert("embeds", embeds, 0o040000).unwrap();
    let root = git.find_tree(root.write().unwrap()).unwrap();
    let signature =
        git2::Signature::new("Test", "test@example.com", &git2::Time::new(1, 0)).unwrap();
    let id = git
        .commit(None, &signature, &signature, "huge doc", &root, &[])
        .unwrap();
    git.reference("refs/rad/id", id, true, "test").unwrap();
    git.set_head("refs/heads/does-not-exist").unwrap();

    let (node, rid) = (&f.node, f.rid);
    let mut refused = 0;
    let grew = peak_growth(|| {
        let b = Bounds::for_binding();
        for result in [
            node.list_files_bounded(rid, &b).map(|_| ()),
            node.tree_bounded(rid, "", &b).map(|_| ()),
            node.read_blob_capped(rid, "a.txt", libradicle::BINDING_READ_BUDGET)
                .map(|_| ()),
            node.repo_info_bounded(rid, &b).map(|_| ()),
            node.list_repos_bounded(&b).map(|_| ()),
        ] {
            if matches!(result, Err(Error::TooLarge { .. })) {
                refused += 1;
            } else {
                println!("not refused: {result:?}");
            }
        }
    });
    println!("refused: {refused} of 5");
    let control = peak_growth(|| {
        assert!(git.find_blob(doc).unwrap().content().len() as u64 >= 32 * MIB);
    });
    print_growth(grew, control);
}

/// Makes the identity at `refs/rad/id` the node and two more delegates, two
/// of three needed, and returns the two others' keys. Neither has a branch.
fn two_of_three(git: &git2::Repository, nid: &str) -> [radicle::crypto::PublicKey; 2] {
    let others = [1u8, 2u8].map(|seed| radicle::crypto::PublicKey::from_bytes([seed; 32]));
    let id = git.refname_to_id("refs/rad/id").unwrap();
    let commit = git.find_commit(id).unwrap();
    let doc_entry = commit
        .tree()
        .unwrap()
        .get_path(std::path::Path::new("embeds/radicle.json"))
        .unwrap();
    let mut doc: serde_json::Value =
        serde_json::from_slice(git.find_blob(doc_entry.id()).unwrap().content()).unwrap();
    let mut delegates = vec![format!("did:key:{nid}")];
    delegates.extend(
        others
            .iter()
            .map(|key| radicle::identity::Did::from(*key).to_string()),
    );
    doc["delegates"] = serde_json::json!(delegates);
    doc["threshold"] = serde_json::json!(2);
    let blob = git.blob(&serde_json::to_vec(&doc).unwrap()).unwrap();
    let mut embeds = git.treebuilder(None).unwrap();
    embeds.insert("radicle.json", blob, 0o100644).unwrap();
    let embeds = embeds.write().unwrap();
    let mut root = git.treebuilder(None).unwrap();
    root.insert("embeds", embeds, 0o040000).unwrap();
    let root = git.find_tree(root.write().unwrap()).unwrap();
    let signature =
        git2::Signature::new("Test", "test@example.com", &git2::Time::new(1, 0)).unwrap();
    let id = git
        .commit(
            None,
            &signature,
            &signature,
            "three delegates",
            &root,
            &[&commit],
        )
        .unwrap();
    git.reference("refs/rad/id", id, true, "test").unwrap();
    others
}

/// LR15 round 5, 2: a dangling HEAD, an identity of three delegates at a
/// threshold of two, the third's branch absent, and the other two on
/// separate 20,000-commit histories with 1 KiB messages (the reviewer's
/// reproduction). heartwood would walk them for a merge base, unbounded and
/// uncancellable; a bounded read refuses at once with NoCanonicalHead. In a
/// child, for its time and peak memory, against heartwood's own resolution
/// as the control.
#[test]
fn divergent_delegates_are_refused_without_a_merge_base_walk() {
    const NAME: &str = "divergent_delegates_are_refused_without_a_merge_base_walk";
    if !is_child(NAME) {
        let out = in_child(NAME);
        assert!(out.contains("bounded: NoCanonicalHead"), "{out}");
        let millis: u128 = out
            .lines()
            .find_map(|l| l.strip_prefix("bounded took ms: "))
            .unwrap()
            .parse()
            .unwrap();
        assert!(millis < 2_000, "the refusal took {millis} ms");
        if cfg!(target_os = "linux") {
            let (grew, control) = growths(&out);
            assert!(grew < 4 * MIB, "the refusal grew the peak by {grew} bytes");
            assert!(
                control >= 16 * MIB,
                "the walk stayed within the binding's budget: {control} bytes"
            );
        }
        return;
    }
    let f = fixture("raddiverged");
    let stored = storage(&f).repository(f.rid).unwrap();
    let git = &stored.backend;
    let nid = f.node.identity().nid;
    let others = two_of_three(git, &nid);

    // Two divergent histories of the default branch, 20,000 commits each.
    let base = git
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    let tree = base.tree().unwrap();
    let signature =
        git2::Signature::new("Test", "test@example.com", &git2::Time::new(1, 0)).unwrap();
    let message = "d".repeat(1024);
    for (i, key) in [
        nid.parse::<radicle::crypto::PublicKey>().unwrap(),
        others[0],
    ]
    .iter()
    .enumerate()
    {
        let mut tip = base.clone();
        for n in 0..20_000 {
            let message = format!("{i} {n} {message}");
            let oid = git
                .commit(None, &signature, &signature, &message, &tree, &[&tip])
                .unwrap();
            tip = git.find_commit(oid).unwrap();
        }
        git.reference(
            &format!("refs/namespaces/{key}/refs/heads/main"),
            tip.id(),
            true,
            "test",
        )
        .unwrap();
    }
    git.set_head("refs/heads/does-not-exist").unwrap();

    let (node, rid) = (&f.node, f.rid);
    let mut result = None;
    let started = std::time::Instant::now();
    let grew = peak_growth(|| {
        result = Some(
            node.repo_info_bounded(rid, &Bounds::for_binding())
                .map(|_| ()),
        );
    });
    // On its own line: the harness prints the test's name before it.
    println!("\nbounded took ms: {}", started.elapsed().as_millis());
    match result.unwrap() {
        Err(e @ Error::NoCanonicalHead(_)) => println!("bounded: {e:?}"),
        other => println!("bounded: unexpected {other:?}"),
    }
    // The control: heartwood's own resolution walks both histories.
    let control = peak_growth(|| {
        let _ = node.repo_info(rid);
    });
    print_growth(grew, control);
}

/// LR15 round 5: delegates that agree still give the canonical head with no
/// local HEAD: two of three at a threshold of two, on the same tip, the
/// third's branch absent. The bounded read answers what heartwood answers.
#[test]
fn agreeing_delegates_give_heartwoods_canonical_head() {
    let f = fixture("radagreeing");
    let stored = storage(&f).repository(f.rid).unwrap();
    let git = &stored.backend;
    let nid = f.node.identity().nid;
    let others = two_of_three(git, &nid);
    let tip = git
        .refname_to_id(&format!("refs/namespaces/{nid}/refs/heads/main"))
        .unwrap();
    git.reference(
        &format!("refs/namespaces/{}/refs/heads/main", others[0]),
        tip,
        true,
        "test",
    )
    .unwrap();
    git.set_head("refs/heads/does-not-exist").unwrap();

    let bounded = f
        .node
        .repo_info_bounded(f.rid, &Bounds::for_binding())
        .unwrap();
    assert_eq!(bounded.head, tip.to_string());
    assert_eq!(bounded.head, f.node.repo_info(f.rid).unwrap().head);
}

/// The two present delegates of [`two_of_three`] with their default branch
/// pointed straight at `tips`, and a dangling local HEAD.
fn with_tips(git: &git2::Repository, nid: &str, tips: [git2::Oid; 2]) {
    let others = two_of_three(git, nid);
    let keys = [
        nid.parse::<radicle::crypto::PublicKey>().unwrap(),
        others[0],
    ];
    for (key, tip) in keys.iter().zip(tips) {
        git.reference(
            &format!("refs/namespaces/{key}/refs/heads/main"),
            tip,
            true,
            "test",
        )
        .unwrap();
    }
    git.set_head("refs/heads/does-not-exist").unwrap();
}

/// An annotated tag of `target`, named and messaged `name`.
fn annotated(git: &git2::Repository, target: git2::Oid, name: &str) -> git2::Oid {
    let signature =
        git2::Signature::new("Test", "test@example.com", &git2::Time::new(1, 0)).unwrap();
    let object = git.find_object(target, None).unwrap();
    git.tag(name, &object, &signature, name, false).unwrap()
}

/// LR15 round 6: two delegates' default branches on two different annotated
/// tags. heartwood's tag quorum needs no merge base, and would answer
/// `Quorum(NoCandidates)`; the bounded read refuses the disagreement as it
/// does for commits, with NoCanonicalHead.
#[test]
fn differing_tag_tips_are_refused_as_disagreement() {
    let f = fixture("radtagtips");
    let stored = storage(&f).repository(f.rid).unwrap();
    let git = &stored.backend;
    let nid = f.node.identity().nid;
    let head = git.refname_to_id("refs/heads/main").unwrap();
    let tags = [annotated(git, head, "one"), annotated(git, head, "two")];
    assert_ne!(tags[0], tags[1]);
    with_tips(git, &nid, tags);

    match f.node.repo_info_bounded(f.rid, &Bounds::for_binding()) {
        Err(Error::NoCanonicalHead(rid)) => assert_eq!(rid, f.rid),
        other => panic!("expected NoCanonicalHead, got {other:?}"),
    }
}

/// LR15 round 6: one delegate's default branch on a commit, the other's on
/// an annotated tag of it. Different OIDs of different types, so the bounded
/// read refuses with NoCanonicalHead, not heartwood's quorum error.
#[test]
fn a_commit_and_a_tag_tip_are_refused_as_disagreement() {
    let f = fixture("radmixedtips");
    let stored = storage(&f).repository(f.rid).unwrap();
    let git = &stored.backend;
    let nid = f.node.identity().nid;
    let head = git.refname_to_id("refs/heads/main").unwrap();
    with_tips(git, &nid, [head, annotated(git, head, "one")]);

    match f.node.repo_info_bounded(f.rid, &Bounds::for_binding()) {
        Err(Error::NoCanonicalHead(rid)) => assert_eq!(rid, f.rid),
        other => panic!("expected NoCanonicalHead, got {other:?}"),
    }
}
