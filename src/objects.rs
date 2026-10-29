//! Repository objects loaded only after their size is known: what the
//! bounded reads use, so nothing the repository controls is allocated
//! before a budget applies to it (review of libradicle #15).
//!
//! [`Objects`] reads an object's size and type from its header (libgit2's
//! `git_odb_read_header`, which for a packed delta resolves only the
//! header), refuses one over its limit with [`Error::TooLarge`], charges it
//! to a [`Meter`] of what the read has loaded so far, and only then loads
//! it. [`Walk`] is commit history on top of it.

use std::collections::{HashSet, VecDeque};

use crate::meter::Meter;
use crate::Error;

/// A repository's objects, each loaded after its header is read.
pub(crate) struct Objects<'r> {
    repo: &'r git2::Repository,
    odb: git2::Odb<'r>,
    /// The most bytes any one object may take.
    limit: u64,
}

impl<'r> Objects<'r> {
    pub(crate) fn new(repo: &'r git2::Repository, limit: u64) -> Result<Self, Error> {
        Ok(Self {
            repo,
            odb: repo.odb()?,
            limit,
        })
    }

    /// The size of `oid`, from its header, when it is of `kind` and within
    /// the limit; charged to `read`.
    fn admit(
        &self,
        oid: git2::Oid,
        kind: git2::ObjectType,
        read: &mut Meter,
    ) -> Result<u64, Error> {
        read.check()?;
        let (size, found) = self.odb.read_header(oid)?;
        if found != kind {
            return Err(Error::Git(git2::Error::new(
                git2::ErrorCode::InvalidSpec,
                git2::ErrorClass::Object,
                format!("the object {oid} is a {found}, not a {kind}"),
            )));
        }
        let size = size as u64;
        if size > self.limit {
            return Err(Error::TooLarge {
                size,
                limit: self.limit,
            });
        }
        read.charge(size)?;
        Ok(size)
    }

    /// The commit `oid`, its size checked and charged before it is loaded.
    pub(crate) fn commit(
        &self,
        oid: git2::Oid,
        read: &mut Meter,
    ) -> Result<git2::Commit<'r>, Error> {
        self.admit(oid, git2::ObjectType::Commit, read)?;
        Ok(self.repo.find_commit(oid)?)
    }

    /// The tree `oid`, its size checked and charged before it is loaded.
    pub(crate) fn tree(&self, oid: git2::Oid, read: &mut Meter) -> Result<git2::Tree<'r>, Error> {
        self.admit(oid, git2::ObjectType::Tree, read)?;
        Ok(self.repo.find_tree(oid)?)
    }

    /// The tree at `path` under `root`, each tree on the way loaded as
    /// [`Objects::tree`] loads it. `path` is `/`-separated; empty
    /// components are skipped. Not found, it fails as libgit2's
    /// `git_tree_entry_bypath` does; found but not a tree, with
    /// [`Error::NotATree`].
    pub(crate) fn tree_at_path(
        &self,
        root: git2::Tree<'r>,
        path: &str,
        read: &mut Meter,
    ) -> Result<git2::Tree<'r>, Error> {
        let mut tree = root;
        for component in path.split('/').filter(|c| !c.is_empty()) {
            let id = match tree.get_name(component) {
                Some(entry) if entry.kind() == Some(git2::ObjectType::Tree) => entry.id(),
                Some(_) => return Err(Error::NotATree(path.to_string())),
                None => {
                    return Err(Error::Git(git2::Error::new(
                        git2::ErrorCode::NotFound,
                        git2::ErrorClass::Tree,
                        format!("the path '{component}' does not exist in the given tree"),
                    )))
                }
            };
            tree = self.tree(id, read)?;
        }
        Ok(tree)
    }
}

/// A commit queued in a [`Walk`]: its committer time and parents.
struct Queued {
    oid: git2::Oid,
    time: i64,
    parents: Vec<git2::Oid>,
}

/// The commits reachable from a head, in the order of libgit2's revwalk
/// without sorting flags, which radicle-surf's history and heartwood's
/// walks use: the newest committer time first, ties in the order they were
/// found, each commit's parents queued as it is taken (libgit2's
/// `add_parents_to_list` and `git_commit_list_insert_by_date`).
///
/// Every commit is loaded through [`Objects`], its size checked and
/// charged first, so a walk never loads a commit over the limit, and never
/// more than its read meter allows in all, skipped commits included.
pub(crate) struct Walk<'o, 'r> {
    objects: &'o Objects<'r>,
    queue: VecDeque<Queued>,
    seen: HashSet<git2::Oid>,
}

impl<'o, 'r> Walk<'o, 'r> {
    pub(crate) fn new(
        objects: &'o Objects<'r>,
        head: git2::Oid,
        read: &mut Meter,
    ) -> Result<Self, Error> {
        let mut walk = Self {
            objects,
            queue: VecDeque::new(),
            seen: HashSet::new(),
        };
        let queued = walk.parse(head, read)?;
        walk.seen.insert(head);
        walk.queue.push_back(queued);
        Ok(walk)
    }

    fn parse(&self, oid: git2::Oid, read: &mut Meter) -> Result<Queued, Error> {
        let commit = self.objects.commit(oid, read)?;
        Ok(Queued {
            oid,
            time: commit.time().seconds(),
            parents: commit.parent_ids().collect(),
        })
    }

    /// The next commit's id; its parents are queued, each loaded (and
    /// charged) once.
    pub(crate) fn next(&mut self, read: &mut Meter) -> Result<Option<git2::Oid>, Error> {
        let Some(next) = self.queue.pop_front() else {
            return Ok(None);
        };
        for parent in next.parents {
            if self.seen.contains(&parent) {
                continue;
            }
            let queued = self.parse(parent, read)?;
            self.seen.insert(parent);
            let at = self
                .queue
                .iter()
                .position(|q| q.time < queued.time)
                .unwrap_or(self.queue.len());
            self.queue.insert(at, queued);
        }
        Ok(Some(next.oid))
    }

    /// A commit this walk has returned, loaded again: its size was checked
    /// when it was queued.
    pub(crate) fn load(&self, oid: git2::Oid) -> Result<git2::Commit<'r>, Error> {
        Ok(self.objects.repo.find_commit(oid)?)
    }
}
