//! The import with no node running (`libradicle::import`; freedom-core
//! design 0008, "Writes", and design 0010, D5):
//! - it makes a repository whose namespace holds the working copy's default
//!   branch, signed, with its identity, canonical references and seeding
//!   policy, through each stage in order;
//! - the working copy is read, never written: every file under it is the
//!   same after, its hooks don't run, and its configuration's
//!   `url.<base>.insteadOf` doesn't apply;
//! - a linked worktree and a separate git directory import as the working
//!   copy itself does;
//! - what `prepare` refuses changes nothing: a name or branch Radicle
//!   refuses, a branch the working copy lacks, a path that isn't a
//!   repository, a repository already in storage;
//! - a failure after the start leaves no repository and no temporary clone,
//!   and a run cut off before its end (a panic here, as a kill) leaves only
//!   a temporary clone, never the repository.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use libradicle::import::{Import, Stage};
use radicle::crypto::{Seed, SigningKey};
use radicle::git::raw as git2;
use radicle::node::policy::{Scope, SeedingPolicy};
use radicle::prelude::RepoId;
use radicle::storage::refs::SignedRefs;
use radicle::storage::{ReadRepository as _, ValidateRepository as _};

const NAME: &str = "imported";
const DESCRIPTION: &str = "An import test.";

fn key() -> SigningKey {
    SigningKey::from_seed(Seed::new([7; 32]))
}

/// A fresh directory under TMPDIR for this test process.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("import-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A Radicle home with a `config.json`, as a node's first start leaves it.
fn home(name: &str) -> PathBuf {
    let home = scratch(&format!("{name}-home"));
    std::fs::write(
        home.join("config.json"),
        r#"{"preferredSeeds":[],"node":{"alias":"importer","network":"test","peers":{"type":"static"}}}"#,
    )
    .unwrap();
    home
}

/// A commit on `refs/heads/master` of `repo`, whose work tree is `dir`.
fn commit(repo: &git2::Repository, dir: &Path, text: &str) -> git2::Oid {
    std::fs::write(dir.join("README.md"), text).unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("README.md")).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let signature = git2::Signature::now("Import", "import@example.invalid").unwrap();
    let parent = repo
        .refname_to_id("refs/heads/master")
        .ok()
        .map(|oid| repo.find_commit(oid).unwrap());
    repo.commit(
        Some("refs/heads/master"),
        &signature,
        &signature,
        text,
        &tree,
        &parent.iter().collect::<Vec<_>>(),
    )
    .unwrap()
}

/// A working copy with two commits on `master`, hooks that would leave a
/// file named `hooked` beside it if they ran, and a `url.<base>.insteadOf`
/// that, if it applied, would send a fetch of the working copy elsewhere.
fn working_copy(name: &str) -> (PathBuf, git2::Oid) {
    let root = scratch(&format!("{name}-work"));
    let dir = root.join("work");
    let repo = git2::Repository::init(&dir).unwrap();
    commit(&repo, &dir, "# One\n");
    let tip = commit(&repo, &dir, "# Two\n");
    let hooked = root.join("hooked");
    for hook in [
        "pre-push",
        "post-update",
        "reference-transaction",
        "post-checkout",
        "pre-upload-pack",
    ] {
        let path = repo.path().join("hooks").join(hook);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("#!/bin/sh\ntouch '{}'\n", hooked.display())).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    let mut config = repo.config().unwrap();
    let gitdir = dunce::canonicalize(repo.path()).unwrap();
    config
        .set_str(
            &format!("url.{}.insteadOf", root.join("elsewhere").display()),
            gitdir.to_str().unwrap(),
        )
        .unwrap();
    config.set_str("core.hooksPath", "hooks").unwrap();
    (dir, tip)
}

/// Every file under `dir`, by its path, with its bytes.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(at) = stack.pop() {
        for entry in std::fs::read_dir(&at).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                stack.push(entry.path());
            } else {
                files.insert(
                    entry.path().strip_prefix(dir).unwrap().to_path_buf(),
                    std::fs::read(entry.path()).unwrap(),
                );
            }
        }
    }
    files
}

/// The entries of `home`'s storage, by name.
fn storage_entries(home: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(home.join("storage"))
        .map(|dir| {
            dir.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn seed_policy(home: &Path, rid: RepoId) -> Option<SeedingPolicy> {
    let db = home.join("node").join(radicle::node::POLICIES_DB_FILE);
    if !db.exists() {
        return None;
    }
    let store = radicle::node::policy::store::Store::reader(db).unwrap();
    store.seed_policy(&rid).unwrap().map(|p| p.policy)
}

fn prepare(home: &Path, work: &Path) -> Result<Import, libradicle::Error> {
    Import::prepare(home, &key(), work, NAME, DESCRIPTION, "master")
}

/// Checks the repository `rid` in `home`'s storage: the working copy's
/// `tip` on `master` in the signer's namespace and canonically, its refs
/// signed, its identity the project's, and seeded.
fn check_imported(home: &Path, rid: RepoId, tip: git2::Oid) {
    let pk = *radicle::crypto::Signer::public_key(&key());
    let repo =
        radicle::storage::git::Repository::open(home.join("storage").join(rid.canonical()), rid)
            .unwrap();
    let ours = format!("refs/namespaces/{pk}/refs/heads/master");
    assert_eq!(repo.backend.refname_to_id(&ours).unwrap(), tip);
    assert_eq!(
        repo.backend.refname_to_id("refs/heads/master").unwrap(),
        tip
    );
    assert!(SignedRefs::load(pk, &repo).unwrap().is_some());
    let validations = repo.validate().unwrap();
    assert!(validations.is_empty(), "{validations:?}");
    let doc = repo.identity_doc().unwrap();
    let project = doc.project().unwrap();
    assert_eq!(project.name(), NAME);
    assert_eq!(project.description(), DESCRIPTION);
    assert_eq!(project.default_branch().as_str(), "master");
    assert_eq!(
        doc.delegates().iter().map(|did| **did).collect::<Vec<_>>(),
        vec![pk]
    );
    assert_eq!(
        seed_policy(home, rid),
        Some(SeedingPolicy::Allow { scope: Scope::All })
    );
}

#[test]
fn an_import_makes_a_signed_seeded_repository_and_leaves_the_working_copy_as_it_was() {
    let home = home("whole");
    let (work, tip) = working_copy("whole");
    let before = snapshot(work.parent().unwrap());

    let import = prepare(&home, &work).unwrap();
    let rid = import.rid();
    // `prepare` made nothing.
    assert!(storage_entries(&home).is_empty());
    assert_eq!(seed_policy(&home, rid), None);

    let mut stages = Vec::new();
    let made = import.run(&mut |stage| stages.push(stage)).unwrap();
    assert_eq!(made, rid);
    assert_eq!(
        stages,
        [Stage::Created, Stage::Fetched, Stage::Signed, Stage::Seeded]
    );
    assert_eq!(storage_entries(&home), [rid.canonical()]);
    check_imported(&home, rid, tip);

    // The working copy is as it was: no `rad` remote, no ref, no hook run.
    assert_eq!(snapshot(work.parent().unwrap()), before);
    let repo = git2::Repository::open(&work).unwrap();
    assert!(repo.find_remote("rad").is_err());
    assert!(!work.parent().unwrap().join("hooked").exists());
}

#[test]
fn a_repository_already_in_storage_is_refused_before_anything_changes() {
    let home = home("exists");
    let (work, _) = working_copy("exists");
    let rid = prepare(&home, &work).unwrap().run(&mut |_| {}).unwrap();
    let storage = snapshot(&home.join("storage"));

    let refused = prepare(&home, &work).unwrap_err();
    assert!(
        matches!(refused, libradicle::Error::Exists(r) if r == rid),
        "{refused:?}"
    );
    assert_eq!(
        refused.to_string(),
        format!("{rid} already exists in storage")
    );
    assert_eq!(snapshot(&home.join("storage")), storage);
}

#[test]
fn what_prepare_refuses_changes_nothing() {
    let home = home("refused");
    let (work, _) = working_copy("refused");
    let key = key();
    let invalid = |result: Result<Import, libradicle::Error>| match result {
        Err(libradicle::Error::InvalidImport(message)) => message,
        other => panic!("{other:?}"),
    };
    // A name Radicle refuses, a branch name it refuses, and a branch the
    // working copy doesn't have.
    invalid(Import::prepare(
        &home,
        &key,
        &work,
        "",
        DESCRIPTION,
        "master",
    ));
    invalid(Import::prepare(
        &home,
        &key,
        &work,
        NAME,
        DESCRIPTION,
        "a..b",
    ));
    let message = invalid(Import::prepare(
        &home,
        &key,
        &work,
        NAME,
        DESCRIPTION,
        "trunk",
    ));
    assert_eq!(message, "the working copy has no branch trunk");
    // Not a repository: libgit2's own error.
    let empty = scratch("refused-empty");
    assert!(matches!(
        Import::prepare(&home, &key, &empty, NAME, DESCRIPTION, "master"),
        Err(libradicle::Error::Git(_))
    ));
    // A home with no `config.json`.
    let bare = scratch("refused-bare-home");
    assert!(matches!(
        Import::prepare(&bare, &key, &work, NAME, DESCRIPTION, "master"),
        Err(libradicle::Error::Profile(_))
    ));
    assert!(storage_entries(&home).is_empty());
}

#[test]
fn a_linked_worktree_and_a_separate_git_directory_import_as_the_working_copy_does() {
    // A linked worktree of a repository: its git directory is under the
    // main one's `worktrees/`, and its objects and refs are the main one's.
    let linked_home = home("linked");
    let (work, tip) = working_copy("linked");
    let repo = git2::Repository::open(&work).unwrap();
    let linked = work.parent().unwrap().join("linked");
    repo.worktree("linked", &linked, None).unwrap();
    assert!(linked.join(".git").is_file());
    let import =
        Import::prepare(&linked_home, &key(), &linked, NAME, DESCRIPTION, "master").unwrap();
    let rid = import.run(&mut |_| {}).unwrap();
    check_imported(&linked_home, rid, tip);

    // A separate git directory: `.git` is a file naming it.
    let separate_home = home("separate");
    let root = scratch("separate-work");
    let dir = root.join("work");
    let repo = git2::Repository::init(&dir).unwrap();
    let tip = commit(&repo, &dir, "# Separate\n");
    drop(repo);
    let gitdir = root.join("elsewhere.git");
    std::fs::rename(dir.join(".git"), &gitdir).unwrap();
    std::fs::write(dir.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
    let rid = prepare(&separate_home, &dir)
        .unwrap()
        .run(&mut |_| {})
        .unwrap();
    check_imported(&separate_home, rid, tip);
}

#[test]
fn a_failure_after_the_start_leaves_no_repository() {
    let home = home("failed");
    let (work, _) = working_copy("failed");
    let import = prepare(&home, &work).unwrap();
    let rid = import.rid();
    // The branch is gone from the working copy before the fetch: the fetch
    // fetches nothing, and the run fails.
    let repo = git2::Repository::open(&work).unwrap();
    let failed = import.run(&mut |stage| {
        if stage == Stage::Created {
            repo.find_reference("refs/heads/master")
                .unwrap()
                .delete()
                .unwrap();
        }
    });
    assert!(failed.is_err(), "{failed:?}");
    assert!(storage_entries(&home).is_empty());
    assert_eq!(seed_policy(&home, rid), None);
}

#[test]
fn a_run_cut_off_before_its_end_leaves_only_a_temporary_clone() {
    for at in [Stage::Created, Stage::Fetched, Stage::Signed, Stage::Seeded] {
        let home = home(&format!("cut-{at:?}"));
        let (work, _) = working_copy(&format!("cut-{at:?}"));
        let import = prepare(&home, &work).unwrap();
        let rid = import.rid();
        // A panic at the stage, as a kill there: nothing after it runs.
        let cut = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            import.run(&mut |stage| {
                if stage == at {
                    panic!("cut off");
                }
            })
        }));
        assert!(cut.is_err());
        let entries = storage_entries(&home);
        assert_eq!(entries.len(), 1, "{at:?}: {entries:?}");
        let name = &entries[0];
        // As heartwood names a clone's: what reconciliation removes.
        assert_eq!(name, &format!("{}.tmp", rid.canonical()), "{at:?}");
        assert!(!home.join("storage").join(rid.canonical()).exists());
    }
}
