//! Repository objects loaded only after their size is known: what the
//! bounded reads use, so nothing the repository controls is allocated
//! before a budget applies to it (review of libradicle #15).
//!
//! [`Objects`] reads an object's size and type from its header (libgit2's
//! `git_odb_read_header`, which for a packed delta resolves only the
//! header), refuses one over its limit with [`Error::TooLarge`], charges
//! what libgit2 will hold of it once loaded ([`tree_held`],
//! [`commit_held`], [`raw_held`]) to a [`Meter`] of what the read has loaded
//! so far, and only then loads it. [`Walk`] is commit history on top of it.
//!
//! **What libgit2 holds** (1.9.6, as libgit2-sys 0.18.7 vendors it, built
//! for SHA-1 object IDs as heartwood builds it): a loaded object's bytes, in
//! a buffer one byte longer, and, once a tree or commit is parsed, what the
//! parse allocates from them. Each figure below is an upper bound, from the
//! object's header size alone, of the bytes libgit2 asks its allocator for
//! while it loads and parses the object and while the object lives: its
//! structs included, the allocator's own rounding not, as for every figure
//! of a budget. A tree's or commit's charge is several times its size, so
//! a meter counts what is really held (review of freedom-core #277, round
//! 2).
//!
//! An object the repository's object cache keeps is held past its walk:
//! the host that bounds memory turns libgit2's cache off (freedom-radicled
//! does, at its start). With the cache on, what a bounded read loads is
//! still charged once, but the cache may keep it, and what it parsed, for
//! as long as the read's repository is open.

use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};

use crate::meter::Meter;
use crate::Error;

/// What libgit2 allocates beside an object's bytes, at most: the object's
/// struct (`git_tree`, `git_blob`, `git_commit`'s first part), the object
/// database's (`git_odb_object`), and their allocations' headers.
const OBJECT_FIXED: u64 = 256;

/// The shortest entry libgit2 parses in a tree of SHA-1 IDs: a mode of one
/// octal digit, its space, a name of one byte, its NUL and the 20-byte ID.
/// libgit2 checks neither the mode nor the names' order, so a tree of
/// `size` bytes has at most `size / 24` entries.
const TREE_ENTRY_SHORTEST: u64 = 24;

/// A parsed tree entry, `git_tree_entry`: its mode and name length, its ID
/// and a pointer to its name in the tree's bytes.
const TREE_ENTRY_PARSED: u64 = 32;

// A parsed entry's ID is the 20 bytes of a SHA-1 `git_oid`: a libgit2 built
// for SHA-256 IDs makes it larger, and every tree's figure wrong.
const _: () = assert!(std::mem::size_of::<git2::Oid>() == 20);

/// The entries libgit2's tree parse allocates for first.
const TREE_ENTRIES_FIRST: u64 = 16;

/// A commit's header line naming a parent, `parent <40 hex>\n`.
const PARENT_LINE: u64 = 48;

/// What a commit's parse allocates beside what grows with its bytes, at
/// most: the rest of `git_commit` (its parents' array, tree ID and string
/// pointers), its two signatures, its parents' first arrays (one ID, then
/// eight, both held while the array grows), and their allocations' headers.
const COMMIT_FIXED: u64 = 512;

/// What libgit2 holds of an object of `size` bytes it loads unparsed (a
/// blob, or an object database read): its bytes, in a buffer one byte
/// longer for a NUL, and its structs.
pub const fn raw_held(size: u64) -> u64 {
    size.saturating_add(1 + OBJECT_FIXED)
}

/// What libgit2 holds of a tree of `size` bytes, by its header, once it is
/// loaded and parsed, at most: the tree's bytes, which the parsed tree
/// keeps ([`raw_held`]), and its array of parsed entries, 32 bytes each
/// (`git_tree_entry`). A tree of the shortest entries, 24 bytes each, has
/// the most of them. The array starts at 16 entries and grows by half each
/// time it is full (libgit2's `git_array_alloc`): while it grows, the
/// array it leaves and the one it moves to are both held. So this is the
/// tree's bytes, and both of those arrays for the most entries the tree can
/// have: at most about 4.4 times its size.
pub const fn tree_held(size: u64) -> u64 {
    let entries = size / TREE_ENTRY_SHORTEST;
    let (mut before, mut capacity) = (0u64, TREE_ENTRIES_FIRST);
    while capacity < entries {
        before = capacity;
        capacity = capacity.saturating_mul(3) / 2;
    }
    raw_held(size).saturating_add(TREE_ENTRY_PARSED.saturating_mul(before + capacity))
}

/// What libgit2 holds of a commit of `size` bytes, by its header, while it
/// is loaded and parsed, at most (its parse keeps no buffer of the object
/// once done, so this is its peak):
/// - its bytes ([`raw_held`]), until the parse is done;
/// - its copies of the header and of the message, each with a NUL: at most
///   `size + 2` together;
/// - what it parses out of the header, each from bytes of its own: the
///   author's and committer's names and emails (and one more author's, as
///   it skips another), each at most its line; each encoding, at most its
///   line; its parents' IDs, 20 bytes for each 48-byte `parent` line, and
///   at most 50 while their array grows by half. At most `size` and 2 bytes
///   more for each `parent` line;
/// - [`COMMIT_FIXED`].
///
/// About 3 times its size.
pub const fn commit_held(size: u64) -> u64 {
    let parents = size / PARENT_LINE;
    raw_held(size)
        .saturating_add(size.saturating_add(2))
        .saturating_add(size.saturating_add(2 * parents))
        .saturating_add(COMMIT_FIXED)
}

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
    /// the limit; what libgit2 will hold of it, `held` of its size, charged
    /// to `read`.
    fn admit(
        &self,
        oid: git2::Oid,
        kind: git2::ObjectType,
        held: fn(u64) -> u64,
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
        read.charge(held(size))?;
        Ok(size)
    }

    /// The size of the blob `oid`, from its header, checked and charged its
    /// size: for a blob something else loads (heartwood's readers). Nothing
    /// of it is loaded here.
    pub(crate) fn blob(&self, oid: git2::Oid, read: &mut Meter) -> Result<u64, Error> {
        self.admit(oid, git2::ObjectType::Blob, |size| size, read)
    }

    /// The object `oid`, of any kind, sized from its header, checked and
    /// charged; nothing of it is loaded here. Its kind, or `None` when it is
    /// missing: whoever loads it reports that.
    pub(crate) fn any(
        &self,
        oid: git2::Oid,
        read: &mut Meter,
    ) -> Result<Option<git2::ObjectType>, Error> {
        read.check()?;
        let (size, kind) = match self.odb.read_header(oid) {
            Ok((size, kind)) => (size as u64, kind),
            Err(e) if e.code() == git2::ErrorCode::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if size > self.limit {
            return Err(Error::TooLarge {
                size,
                limit: self.limit,
            });
        }
        read.charge(size)?;
        Ok(Some(kind))
    }

    /// The commit `oid`, its size checked and what libgit2 holds of it
    /// ([`commit_held`]) charged before it is loaded.
    pub(crate) fn commit(
        &self,
        oid: git2::Oid,
        read: &mut Meter,
    ) -> Result<git2::Commit<'r>, Error> {
        self.admit(oid, git2::ObjectType::Commit, commit_held, read)?;
        Ok(self.repo.find_commit(oid)?)
    }

    /// The tree `oid`, its size checked and what libgit2 holds of it
    /// ([`tree_held`]) charged before it is loaded.
    pub(crate) fn tree(&self, oid: git2::Oid, read: &mut Meter) -> Result<git2::Tree<'r>, Error> {
        self.admit(oid, git2::ObjectType::Tree, tree_held, read)?;
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

    /// The object `oid`, its kind, once its size is admitted and what the
    /// reader will hold of it, `held` of its size, charged; `None` when it
    /// isn't there. A refusal is kept for [`Metered::refused`].
    fn admit(
        &self,
        oid: git2::Oid,
        held: fn(u64) -> u64,
    ) -> Result<Option<git2::ObjectType>, Admit> {
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
            read.charge(held(size))
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

    use super::{commit_held, raw_held, tree_held, Metered};

    /// What reading an object unparsed and copying its bytes holds while it
    /// copies them: libgit2's buffer ([`raw_held`]) and the copy.
    fn copied(size: u64) -> u64 {
        raw_held(size).saturating_add(size)
    }

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
        // As heartwood's git2 reader: the commit's raw bytes, read from the
        // object database and copied, both held as it copies them.
        fn read_commit(&self, oid: &Oid) -> Result<Option<Vec<u8>>, ReadCommit> {
            let id = git2::Oid::from(*oid);
            match self.admit(id, copied) {
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
            match self.admit(id, commit_held) {
                Ok(Some(git2::ObjectType::Commit)) => {}
                Ok(_) => return Err(ReadBlob::commit_not_found_error(*commit)),
                Err(e) => return Err(e.blob()),
            }
            let commit_object = repo.find_commit(id).map_err(ReadBlob::other)?;
            let mut tree_id = commit_object.tree_id();
            drop(commit_object);
            let mut components = path.iter().peekable();
            while let Some(component) = components.next() {
                match self.admit(tree_id, tree_held) {
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
                return match self.admit(entry_id, copied) {
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

/// heartwood's canonical-ref effects (`FindObjects`, `FindMergeBase`,
/// `Ancestry`) over a stored repository, for a bounded read's canonical head
/// (review of libradicle #15, round 5):
/// - `find_objects` looks each delegate's ref up once, sizes and charges
///   its target on the read's meter, and answers those pinned `(DID, OID,
///   type)` tips. Nothing looks the refs up again.
/// - Tips that agree are their own merge base. Tips that differ in any
///   way (commits, tags, or a commit and a tag) are refused as soon as they
///   are pinned ([`Tips::diverged`]): different commits would need a merge
///   base, an unbounded, uncancellable libgit2 walk, which isn't computed.
pub(crate) struct Tips<'a, 'r, 'b> {
    repo: &'a radicle::storage::git::Repository,
    objects: &'a Objects<'r>,
    read: &'a RefCell<Meter<'b>>,
    refused: RefCell<Option<Error>>,
    diverged: std::cell::Cell<bool>,
}

impl<'a, 'r, 'b> Tips<'a, 'r, 'b> {
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
            diverged: std::cell::Cell::new(false),
        }
    }

    /// The refusal of the bounds that failed the resolution, if one did.
    pub(crate) fn refused(&self) -> Option<Error> {
        self.refused.borrow_mut().take()
    }

    /// Whether the pinned tips disagreed (any two different OIDs, of
    /// either type), so the resolution was refused.
    pub(crate) fn diverged(&self) -> bool {
        self.diverged.get()
    }
}

mod canonical_tips {
    use std::collections::{BTreeMap, BTreeSet};

    use radicle::git::canonical::effects::{
        Ancestry, FindMergeBase, FindObjects, FindObjectsError, GraphDescendant, MergeBaseError,
    };
    use radicle::git::canonical::{FoundObjects, GraphAheadBehind, MergeBase, Object};
    use radicle::git::fmt::Qualified;
    use radicle::git::Oid;
    use radicle::prelude::Did;

    use super::{Refused, Tips};

    /// What heartwood's canonical computation sees for disagreeing tips: the
    /// read is refused.
    #[derive(Debug)]
    struct Diverged;

    impl std::fmt::Display for Diverged {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("the pinned tips disagree, which a bounded read doesn't resolve")
        }
    }

    impl std::error::Error for Diverged {}

    impl FindObjects for Tips<'_, '_, '_> {
        fn find_objects<'a, 'b, I>(
            &self,
            refname: &Qualified<'a>,
            dids: I,
        ) -> Result<FoundObjects, FindObjectsError>
        where
            I: Iterator<Item = &'b Did>,
        {
            let mut objects = BTreeMap::new();
            let mut missing_refs = BTreeSet::new();
            let mut missing_objects = BTreeMap::new();
            for did in dids {
                let name = refname.with_namespace(did.as_key().into());
                // The one lookup of this ref: its target is what is sized
                // and answered. As in heartwood, a ref with no direct
                // target (a symbolic one) is skipped, not followed.
                let reference = match self.repo.backend.find_reference(name.as_str()) {
                    Ok(reference) => reference,
                    Err(e) if e.code() == git2::ErrorCode::NotFound => {
                        missing_refs.insert(name.to_owned());
                        continue;
                    }
                    Err(e) => return Err(FindObjectsError::find_reference(name.to_owned(), e)),
                };
                let Some(oid) = reference.target() else {
                    continue;
                };
                #[cfg(test)]
                crate::seams::tip_pinned();
                let kind = {
                    let mut read = self.read.borrow_mut();
                    match self.objects.any(oid, &mut read) {
                        Ok(kind) => kind,
                        Err(e) => {
                            *self.refused.borrow_mut() = Some(e);
                            return Err(FindObjectsError::other(Refused));
                        }
                    }
                };
                let id = Oid::from(oid);
                let object = match kind {
                    None => {
                        missing_objects.insert(*did, id);
                        continue;
                    }
                    Some(git2::ObjectType::Commit) => Object::Commit { id },
                    Some(git2::ObjectType::Tag) => Object::Tag { id },
                    Some(other) => {
                        return Err(FindObjectsError::invalid_object_type(
                            *did,
                            id,
                            Some(other.to_string()),
                        ))
                    }
                };
                objects.insert(*did, object);
            }
            // Any disagreement among the pinned tips is refused here, before
            // the quorum: commits would need a merge base, and tags or a
            // mix of commit and tag would fail heartwood's quorum with no
            // walk, but under its own error.
            let distinct: BTreeSet<Oid> = objects.values().map(Object::id).collect();
            if distinct.len() > 1 {
                self.diverged.set(true);
                return Err(FindObjectsError::other(Diverged));
            }
            Ok(FoundObjects {
                objects,
                missing_refs,
                missing_objects,
            })
        }
    }

    impl FindMergeBase for Tips<'_, '_, '_> {
        fn merge_base(&self, a: Oid, b: Oid) -> Result<MergeBase, MergeBaseError> {
            // Heartwood pairs every vote, agreeing ones too. A commit is its
            // own merge base, which needs no walk. `find_objects` has
            // already refused tips that differ; should two ever reach here,
            // they are refused too.
            if a == b {
                return Ok(MergeBase { a, b, base: a });
            }
            self.diverged.set(true);
            Err(MergeBaseError::new(a, b, Diverged))
        }
    }

    impl Ancestry for Tips<'_, '_, '_> {
        // Only convergence checks ask this, and the canonical head doesn't
        // check convergence. Should one ever ask, the read is refused as for
        // a merge base.
        fn graph_ahead_behind(
            &self,
            _commit: Oid,
            _upstream: Oid,
        ) -> Result<GraphAheadBehind, GraphDescendant> {
            self.diverged.set(true);
            Ok(GraphAheadBehind {
                ahead: 0,
                behind: 0,
            })
        }
    }
}
