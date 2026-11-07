//! An import of a working copy into storage, with no node running
//! (freedom-core design 0008, "Writes"; design 0010, D5).
//!
//! [`Embedded::import_repo`](crate::Embedded::import_repo) needs a running
//! node: it seeds through it, adds the repository to its inventory and
//! announces it, and heartwood's `rad::init`, which it calls, writes to the
//! working copy (a `rad` remote, `refs/remotes/rad/<branch>` and the
//! repository's configuration). This one runs on the home's storage alone,
//! as a host runs it in a process of its own, with no node, and reads the
//! working copy without writing to it:
//!
//! 1. **[`Import::prepare`] changes nothing.** It opens the working copy
//!    with libgit2, exactly at its path (no search upward), and checks what
//!    the import would refuse: the project's name, description and default
//!    branch, as `rad init` checks them; that the working copy has that
//!    branch; that its path is one libgit2's local transport can take; and
//!    that the repository it would make, whose ID the identity document
//!    alone decides, isn't already in storage ([`Error::Exists`]). It reads
//!    the home's `config.json` for the alias the storage names.
//! 2. **[`Import::run`] makes the repository** in a temporary directory of
//!    the storage, as a clone does (`<rid>.tmp`), so that a kill
//!    or a crash leaves only a temporary clone, which reconciliation
//!    removes at the next start:
//!    - the identity document and its first commit, signed by `key`;
//!    - the working copy's default branch, fetched into `key`'s namespace
//!      by libgit2's local transport, in this process: no `git` process
//!      runs, so none of the working copy's hooks, nor commands its
//!      configuration names; and the fetch is the storage's anonymous
//!      remote, so no `url.<base>.insteadOf` of the working copy applies;
//!    - the canonical references, then `key`'s signed references;
//!    - the seeding policy (`Scope::All`, as the node's import seeds it),
//!      written to the policy store directly, under the home's node lock;
//!    - then the repository is moved into place, the one step that makes it
//!      visible, and the storage directory is flushed.
//!
//!    `stage` is told of each durable step, for a host's test seams.
//!
//! **Atomicity:** nothing before the move is a repository of the storage. A
//! failure before it removes the temporary clone; the seeding policy, if
//! written, stays, and names a repository the storage doesn't hold, until a
//! later import of it makes it. A failure of the move itself leaves the
//! temporary clone, which the next start removes.
//!
//! **libgit2's options are the host's:** its configuration search paths
//! ([`crate::isolate_git_config`]) and its fsync (a host runs
//! reconciliation first, which turns it on) are process-wide, and this
//! module changes neither.

use std::path::Path;

use radicle::crypto::SigningKey;
use radicle::git;
use radicle::identity::project::{Project, ProjectName};
use radicle::identity::{Doc, RepoId, Visibility};
use radicle::node::policy::Scope;
use radicle::profile::Home;
use radicle::storage::git::{paths, Storage};
use radicle::storage::{ReadStorage as _, SignRepository as _, WriteRepository as _};

use crate::Error;

/// A durable step of [`Import::run`], each told once it is done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Stage {
    /// The temporary repository exists, with the identity document and its
    /// first commit.
    Created,
    /// The working copy's default branch is in the signer's namespace.
    Fetched,
    /// The canonical references are set and the signer's are signed.
    Signed,
    /// The seeding policy is written.
    Seeded,
}

/// An import, checked: nothing is changed until [`Import::run`].
pub struct Import {
    home: Home,
    storage: Storage,
    key: SigningKey,
    doc: Doc,
    doc_bytes: Vec<u8>,
    rid: RepoId,
    /// The working copy's git directory, canonical, as the local transport
    /// takes it.
    working: String,
    branch: git::BranchName,
}

impl std::fmt::Debug for Import {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Import")
            .field("rid", &self.rid)
            .finish_non_exhaustive()
    }
}

impl Import {
    /// Checks an import of `path`'s default branch `default_branch` into
    /// the storage of the Radicle home `home`, as a project named `name`
    /// with `description`, delegated to `key`'s identity: what
    /// [`Import::run`] would refuse, it refuses here, and it changes
    /// nothing, beyond making the storage directory if there is none.
    ///
    /// An argument Radicle refuses, or a default branch the working copy
    /// doesn't have, is [`Error::InvalidImport`]; a repository already in
    /// storage, [`Error::Exists`]. A working copy libgit2 can't open is its
    /// error, as is a home without `config.json`.
    pub fn prepare(
        home: &Path,
        key: &SigningKey,
        path: &Path,
        name: &str,
        description: &str,
        default_branch: &str,
    ) -> Result<Self, Error> {
        let name = ProjectName::try_from(name).map_err(|e| Error::InvalidImport(e.to_string()))?;
        let branch = git::BranchName::try_from(default_branch.to_string())
            .map_err(|e| Error::InvalidImport(e.to_string()))?;
        let project =
            Project::new(name, description.to_owned(), branch.clone()).map_err(|errs| {
                Error::InvalidImport(
                    errs.into_iter()
                        .map(|err| err.to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                )
            })?;
        // Exactly at `path`: `git_repository_open` doesn't search upward.
        let repo = git::raw::Repository::open(path)?;
        let qualified = git::fmt::Qualified::from(git::fmt::lit::refs_heads(&branch));
        if repo.refname_to_id(qualified.as_str()).is_err() {
            return Err(Error::InvalidImport(format!(
                "the working copy has no branch {branch}"
            )));
        }
        // As libgit2's local transport takes it: by a path string. One that
        // isn't valid Unicode can't be passed as it is, and a lossy form
        // could name another repository.
        let working = dunce::canonicalize(repo.path())?;
        let Some(working) = working.to_str().map(str::to_owned) else {
            return Err(Error::InvalidImport(
                "the working copy's path isn't valid Unicode".into(),
            ));
        };
        drop(repo);
        let public_key = *radicle::crypto::Signer::public_key(key);

        let home = Home::new(home)?;
        let config = radicle::profile::Config::load(home.config().as_path())
            .map_err(radicle::profile::Error::from)?;
        let storage = Storage::open(
            home.storage(),
            git::UserInfo {
                alias: config.alias().clone(),
                key: public_key,
            },
        )?;
        let doc = Doc::initial(project, public_key.into(), Visibility::Public);
        let (doc_oid, doc_bytes) = doc.encode()?;
        let rid = RepoId::from(doc_oid);
        if storage.contains(&rid)? {
            return Err(Error::Exists(rid));
        }
        Ok(Self {
            home,
            storage,
            key: key.clone(),
            doc,
            doc_bytes,
            rid,
            working,
            branch,
        })
    }

    /// The repository the import makes.
    pub fn rid(&self) -> RepoId {
        self.rid
    }

    /// Runs the import (see the module's doc), telling `stage` of each
    /// durable step. Once it returns `Ok`, the repository is in storage,
    /// signed and seeded.
    pub fn run(self, stage: &mut dyn FnMut(Stage)) -> Result<RepoId, Error> {
        let temp = self.storage.temporary_repository(self.rid)?;
        match self.build(temp.as_ref(), stage) {
            Ok(()) => {}
            Err(e) => {
                temp.cleanup();
                return Err(e);
            }
        }
        let to = paths::repository(&self.storage, &self.rid);
        temp.mv(&to)?;
        sync_dir(self.storage.path())?;
        Ok(self.rid)
    }

    /// Everything before the move, in the temporary repository `repo`.
    fn build(
        &self,
        repo: &radicle::storage::git::Repository,
        stage: &mut dyn FnMut(Stage),
    ) -> Result<(), Error> {
        let pk = radicle::crypto::Signer::public_key(&self.key);
        // `Repository::init`, in the temporary directory.
        repo.backend.blob(&self.doc_bytes)?;
        let identity = self.doc.init(repo, &self.key)?;
        stage(Stage::Created);

        // `rad::init`'s copy of the default branch, without the working
        // copy's `rad` remote.
        let branch = git::fmt::Qualified::from(git::fmt::lit::refs_heads(&self.branch));
        let ours = branch.with_namespace(git::fmt::Component::from(pk));
        let fetchspec = git::fmt::refspec::Refspec {
            src: branch.clone(),
            dst: ours.clone(),
            force: false,
        }
        .to_string();
        repo.backend.remote_anonymous(&self.working)?.fetch(
            &[fetchspec.as_str()],
            Some(
                git2::FetchOptions::new()
                    .update_fetchhead(false)
                    .download_tags(git2::AutotagOption::None),
            ),
            None,
        )?;
        // A branch gone from the working copy since `prepare` fetches
        // nothing, without an error.
        if repo.backend.refname_to_id(ours.as_str()).is_err() {
            return Err(Error::InvalidImport(format!(
                "the working copy's branch {} wasn't fetched",
                self.branch
            )));
        }
        stage(Stage::Fetched);

        repo.set_remote_identity_root_to(pk, identity)?;
        repo.set_identity_head_to(identity)?;
        repo.set_canonical_symbolic_refs("set-canonical from import (libradicle)")?;
        repo.set_default_branch_to_canonical_head()?;
        repo.sign_refs(&self.key)?;
        stage(Stage::Signed);

        // As `Profile::seed` writes it without a node: under the home's node
        // lock, so that no node starts in between.
        let _node = self.home.lock_node().map_err(|e| {
            if e.kind() == std::io::ErrorKind::WouldBlock {
                Error::NodeThread("a node holds the home: the import runs without one".into())
            } else {
                Error::Io(e)
            }
        })?;
        self.home.policies_mut()?.seed(&self.rid, Scope::All)?;
        stage(Stage::Seeded);
        Ok(())
    }
}

/// Flushes the directory `dir`, on Unix: the move into it is durable.
/// Windows has no directory flush.
fn sync_dir(dir: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}
