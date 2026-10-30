//! Repository objects loaded only after their size is known: what the
//! bounded reads use, so nothing the repository controls is allocated
//! before a budget applies to it (review of libradicle #15).
//!
//! [`Objects`] reads an object's size and type from its header (libgit2's
//! `git_odb_read_header`, which for a packed delta resolves only the
//! header), refuses one over its limit with [`Error::TooLarge`], charges it
//! to a [`Meter`] of what the read has loaded so far, and only then loads
//! it. [`Walk`] is commit history on top of it.

use std::cell::RefCell;
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

    /// The size of the blob `oid`, from its header, checked and charged:
    /// for a blob something else loads (heartwood's readers). Nothing of
    /// it is loaded here.
    pub(crate) fn blob(&self, oid: git2::Oid, read: &mut Meter) -> Result<u64, Error> {
        self.admit(oid, git2::ObjectType::Blob, read)
    }

    /// The object `oid`, of any kind, sized from its header, checked and
    /// charged; nothing of it is loaded here. Missing is fine: whoever
    /// loads it reports that.
    pub(crate) fn any(&self, oid: git2::Oid, read: &mut Meter) -> Result<(), Error> {
        read.check()?;
        let size = match self.odb.read_header(oid) {
            Ok((size, _)) => size as u64,
            Err(e) if e.code() == git2::ErrorCode::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        if size > self.limit {
            return Err(Error::TooLarge {
                size,
                limit: self.limit,
            });
        }
        read.charge(size)
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

/// heartwood's signed-refs object reader (`sigrefs::git::object::Reader`,
/// with `reference::Reader` and `HasRepoId`) over a stored repository, with
/// every object it reads admitted first: sized from its header, refused over
/// the limit, and charged to one meter shared by everything the read loads
/// (review of libradicle #15, round 3). heartwood's own `SignedRefs` reader
/// drives it, so the objects admitted are exactly the ones heartwood reads,
/// its old-format history walk and identity-root documents included.
///
/// A refusal reaches heartwood as an opaque error, which fails its read; the
/// refusal itself is kept, and [`Metered::refused`] hands it back.
pub(crate) struct Metered<'a, 'r, 'b> {
    repo: &'a radicle::storage::git::Repository,
    objects: &'a Objects<'r>,
    read: &'a RefCell<Meter<'b>>,
    refused: RefCell<Option<Error>>,
}

/// What heartwood sees of a refusal: [`Metered::refused`] has the error.
#[derive(Debug)]
struct Refused;

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("refused by the read's bounds")
    }
}

impl std::error::Error for Refused {}

impl<'a, 'r, 'b> Metered<'a, 'r, 'b> {
    pub(crate) fn new(
        repo: &'a radicle::storage::git::Repository,
        objects: &'a Objects<'r>,
        read: &'a RefCell<Meter<'b>>,
    ) -> Self {
        Self {
            repo,
            objects,
            read,
            refused: RefCell::new(None),
        }
    }

    /// The refusal that failed a read through this reader, if one did.
    pub(crate) fn refused(&self) -> Option<Error> {
        self.refused.borrow_mut().take()
    }

    /// The object `oid`, its kind, once its size is admitted and charged;
    /// `None` when it isn't there. A refusal is kept for
    /// [`Metered::refused`].
    fn admit(&self, oid: git2::Oid) -> Result<Option<git2::ObjectType>, Admit> {
        let (size, kind) = match self.objects.odb.read_header(oid) {
            Ok((size, kind)) => (size as u64, kind),
            Err(e) if e.code() == git2::ErrorCode::NotFound => return Ok(None),
            Err(e) => return Err(Admit::Git(e)),
        };
        let mut read = self.read.borrow_mut();
        let admitted = read.check().and_then(|()| {
            if size > self.objects.limit {
                return Err(Error::TooLarge {
                    size,
                    limit: self.objects.limit,
                });
            }
            read.charge(size)
        });
        match admitted {
            Ok(()) => Ok(Some(kind)),
            Err(e) => {
                *self.refused.borrow_mut() = Some(e);
                Err(Admit::Refused)
            }
        }
    }
}

/// Why [`Metered::admit`] didn't admit an object.
enum Admit {
    Git(git2::Error),
    Refused,
}

impl Admit {
    fn commit(self) -> radicle::storage::refs::sigrefs::git::object::error::ReadCommit {
        use radicle::storage::refs::sigrefs::git::object::error::ReadCommit;
        match self {
            Self::Git(e) => ReadCommit::other(e),
            Self::Refused => ReadCommit::other(Refused),
        }
    }

    fn blob(self) -> radicle::storage::refs::sigrefs::git::object::error::ReadBlob {
        use radicle::storage::refs::sigrefs::git::object::error::ReadBlob;
        match self {
            Self::Git(e) => ReadBlob::other(e),
            Self::Refused => ReadBlob::other(Refused),
        }
    }
}

mod sigrefs_reader {
    use std::path::Path;

    use radicle::git::Oid;
    use radicle::storage::refs::sigrefs::git::object::error::{ReadBlob, ReadCommit};
    use radicle::storage::refs::sigrefs::git::object::{Blob, Reader};
    use radicle::storage::refs::sigrefs::git::reference;

    use super::Metered;

    impl radicle::storage::HasRepoId for Metered<'_, '_, '_> {
        fn rid(&self) -> radicle::identity::RepoId {
            self.repo.id
        }
    }

    impl reference::Reader for Metered<'_, '_, '_> {
        fn find_reference(
            &self,
            reference: &radicle::git::fmt::Namespaced,
        ) -> Result<Option<Oid>, reference::error::FindReference> {
            reference::Reader::find_reference(self.repo, reference)
        }
    }

    impl Reader for Metered<'_, '_, '_> {
        // As heartwood's git2 reader: the commit's raw bytes.
        fn read_commit(&self, oid: &Oid) -> Result<Option<Vec<u8>>, ReadCommit> {
            let id = git2::Oid::from(*oid);
            match self.admit(id) {
                Ok(None) => Ok(None),
                Ok(Some(git2::ObjectType::Commit)) => {
                    let object = self.objects.odb.read(id).map_err(ReadCommit::other)?;
                    Ok(Some(object.data().to_vec()))
                }
                Ok(Some(kind)) => Err(ReadCommit::incorrect_object_error(*oid, kind)),
                Err(e) => Err(e.commit()),
            }
        }

        // As heartwood's git2 reader: the blob at `path` in the commit's
        // tree, every object on the way admitted first.
        fn read_blob(&self, commit: &Oid, path: &Path) -> Result<Option<Blob>, ReadBlob> {
            let repo = self.objects.repo;
            let id = git2::Oid::from(*commit);
            match self.admit(id) {
                Ok(Some(git2::ObjectType::Commit)) => {}
                Ok(_) => return Err(ReadBlob::commit_not_found_error(*commit)),
                Err(e) => return Err(e.blob()),
            }
            let commit_object = repo.find_commit(id).map_err(ReadBlob::other)?;
            let mut tree_id = commit_object.tree_id();
            drop(commit_object);
            let mut components = path.iter().peekable();
            while let Some(component) = components.next() {
                match self.admit(tree_id) {
                    Ok(Some(git2::ObjectType::Tree)) => {}
                    Ok(_) => return Ok(None),
                    Err(e) => return Err(e.blob()),
                }
                let tree = repo.find_tree(tree_id).map_err(ReadBlob::other)?;
                let found = component
                    .to_str()
                    .and_then(|name| tree.get_name(name))
                    .map(|entry| (entry.id(), entry.kind()));
                drop(tree);
                let Some((entry_id, entry_kind)) = found else {
                    return Ok(None);
                };
                if components.peek().is_some() {
                    if entry_kind != Some(git2::ObjectType::Tree) {
                        return Ok(None);
                    }
                    tree_id = entry_id;
                    continue;
                }
                return match self.admit(entry_id) {
                    Ok(Some(git2::ObjectType::Blob)) => {
                        let blob = repo.find_blob(entry_id).map_err(ReadBlob::other)?;
                        Ok(Some(Blob {
                            oid: blob.id().into(),
                            bytes: blob.content().to_vec(),
                        }))
                    }
                    Ok(Some(kind)) => Err(ReadBlob::incorrect_object_error(
                        *commit,
                        path.to_path_buf(),
                        kind,
                    )),
                    Ok(None) => Ok(None),
                    Err(e) => Err(e.blob()),
                };
            }
            Ok(None)
        }
    }
}
