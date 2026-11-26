use radicle::identity::RepoId;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Profile(#[from] radicle::profile::Error),
    #[error(transparent)]
    Keystore(#[from] radicle::crypto::ssh::keystore::Error),
    #[error(transparent)]
    Runtime(#[from] radicle_node::runtime::Error),
    #[error(transparent)]
    Node(#[from] radicle::node::Error),
    #[error(transparent)]
    NodeHandle(#[from] radicle_node::runtime::handle::Error),
    #[error(transparent)]
    Storage(#[from] radicle::storage::Error),
    #[error(transparent)]
    StorageRefs(#[from] radicle::storage::refs::Error),
    #[error(transparent)]
    Repository(#[from] radicle::storage::RepositoryError),
    /// The identity document has no project payload. heartwood's
    /// `project()` answers `None` for it since upstream 138404df, where it
    /// was `PayloadError::NotFound`: the text is that error's, unchanged.
    #[error("payload 'xyz.radicle.project' not found in identity document")]
    NoProject,
    #[error(transparent)]
    Surf(#[from] radicle_surf::Error),
    #[error(transparent)]
    Git(#[from] radicle::git::raw::Error),
    #[error(transparent)]
    Doc(#[from] radicle::identity::doc::DocError),
    #[error(transparent)]
    Payload(#[from] radicle::identity::doc::PayloadError),
    #[error("no seeds found for {0}")]
    NoSeeds(RepoId),
    #[error("fetching {0} failed: {1}")]
    FetchFailed(RepoId, String),
    #[error("fetching {0} timed out")]
    Timeout(RepoId),
    /// A clone, or a bounded walk, cancelled through its `CancelToken`.
    #[error("cancelled")]
    Cancelled,
    /// An announcement of this node's refs failed, other than at its bound
    /// or the cap ([`Error::NodeDeadline`], [`Error::NodeBusy`]): the node
    /// answered it with a failure, or the wait for its answer failed (its
    /// thread didn't start, or ended without the answer). A COB change or an
    /// import is committed before it's announced, and stands
    /// ([`Error::is_announce`]).
    #[error("announce refs failed: {0}")]
    Announce(String),
    #[error("node thread: {0}")]
    NodeThread(String),
    #[error("the node didn't stop within {0:?}")]
    ShutdownTimeout(std::time::Duration),
    /// The node's reactor didn't answer `call` within `within`
    /// ([`crate::Embedded::connected_peers_within`],
    /// [`crate::Embedded::seeders_within`],
    /// [`crate::Embedded::unseed_repo_within`],
    /// [`crate::Embedded::remove_quarantined_within`], and the announcement
    /// of [`crate::Embedded::create_issue_within`] and the other COB
    /// changes, after the change is committed). The request was sent: for
    /// one that changes the node's state, whether it took effect is unknown,
    /// as the reactor may still carry it out. The call goes on waiting in the
    /// background ([`crate::Embedded::waiting_on_node`]).
    #[error("the node didn't answer {call} within {within:?}")]
    NodeDeadline {
        call: &'static str,
        within: std::time::Duration,
    },
    /// `call` wasn't sent: `waiting` calls were already waiting on the node's
    /// reactor, as many as may at once ([`crate::NODE_WAIT_LIMIT`]), within
    /// their bounds or past them. Nothing of `call` reached the node.
    #[error("{call} wasn't sent: {waiting} calls are already waiting on the node")]
    NodeBusy { call: &'static str, waiting: usize },
    /// An alias Radicle refuses (empty, too long, or with whitespace or
    /// control characters), for a fresh profile's `config.json`.
    #[error("invalid node alias: {0}")]
    Alias(#[from] radicle::node::AliasError),
    #[error("invalid collaborative object id {0:?}")]
    InvalidCobId(String),
    #[error("not a directory: {0}")]
    NotATree(String),
    #[error("not a file: {0}")]
    NotABlob(String),
    #[error("{size} bytes is over the limit of {limit}")]
    TooLarge { size: u64, limit: u64 },
    /// A bounded tree walk that would descend past
    /// [`crate::MAX_TREE_DEPTH`].
    #[error("the tree is nested deeper than {limit} levels")]
    TooDeep { limit: usize },
    /// A bounded read of a repository with no canonical identity head
    /// (`refs/rad/id`): heartwood would search every remote's identity
    /// history for it, which can't be sized before it is read.
    #[error("{0} has no canonical identity head to size its identity document from")]
    NoIdentityHead(RepoId),
    /// A bounded read of a repository with no usable local `HEAD` whose
    /// delegates' default branches point at different objects (commits,
    /// tags, or a mix): for commits, finding its canonical head would need a
    /// merge-base walk, which a bounded read doesn't make.
    #[error("{0} has no usable local HEAD, and its delegates' default branches disagree: its canonical head can't be found within a bounded read")]
    NoCanonicalHead(RepoId),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("invalid commit revision {0:?}")]
    InvalidRevision(String),
    #[error(transparent)]
    Cob(#[from] radicle::cob::store::Error),
    #[error(transparent)]
    IssueCache(#[from] radicle::issue::cache::Error),
    #[error(transparent)]
    Issue(#[from] radicle::issue::Error),
    #[error(transparent)]
    PatchCache(#[from] radicle::patch::cache::Error),
    #[error(transparent)]
    Patch(#[from] radicle::patch::Error),
    #[error(transparent)]
    Signer(#[from] radicle::profile::SignerError),
    #[error(transparent)]
    Init(#[from] radicle::rad::InitError),
    /// An import's argument Radicle refuses (the project's name, its
    /// description or its default branch, as `rad init` checks them), or a
    /// default branch the working copy doesn't have ([`crate::import`]).
    /// Nothing is made.
    #[error("invalid import: {0}")]
    InvalidImport(String),
    /// The repository an import would make is already in storage
    /// ([`crate::import`]). Nothing is made.
    #[error("{0} already exists in storage")]
    Exists(RepoId),
    #[error(transparent)]
    Policy(#[from] radicle::node::policy::store::Error),
    #[error(transparent)]
    Sqlite(#[from] sqlite::Error),
    /// The repository is being repaired or is quarantined: it isn't seeded,
    /// cloned or imported until its host opens it.
    #[error("{rid} is {state}: it can't be seeded until it opens")]
    Gated {
        rid: RepoId,
        state: radicle::node::gate::RepoState,
    },
    /// A quarantined repository's removal asked while it is open at the
    /// gate: it runs only while the repository is shut.
    #[error("{0} is open at the repository gate: shut it before it is removed")]
    Open(RepoId),
    /// A unit of an online repair failed outside its repository (heartwood's
    /// reconciliation driver): the progress state, the quarantine record, or
    /// the host's environment (`driver::Error::Host`: libgit2's configuration
    /// outside any repository doesn't load, or the repository isn't the
    /// process's to open), which would fail every repository alike. None of
    /// these quarantines the repository: the repair's progress stays, and
    /// the next run continues it.
    ///
    /// Boxed, as the next two are, so that every `Result` of this crate
    /// stays small.
    #[error(transparent)]
    Reconcile(Box<radicle::storage::git::reconcile::driver::Error>),
    #[error(transparent)]
    Quarantine(Box<radicle::storage::git::reconcile::quarantine::Error>),
    /// A quarantined repository's removal failed part way: its record stays,
    /// and the removal completes when run again with the same `qid`.
    #[error(transparent)]
    Remove(Box<radicle::storage::git::reconcile::quarantine::RemoveError>),
    /// libgit2's configuration search paths couldn't be emptied
    /// ([`crate::isolate_git_config`]).
    #[error("libgit2's configuration search paths: {0}")]
    GitConfig(String),
    /// The out-of-memory hooks of [`crate::process_init`] weren't installed:
    /// SQLite was already initialized, or the libgit2 linked isn't 1.9.6.
    /// Nothing was changed.
    #[error("the out-of-memory hooks: {0}")]
    ProcessInit(String),
}

/// The name, in [`Error::NodeDeadline`] and [`Error::NodeBusy`], of the
/// announcement that follows a committed COB change or import.
pub(crate) const ANNOUNCE: &str = "announce_refs_for";

impl Error {
    /// Whether this is the error of an announcement, which follows a COB
    /// change ([`crate::Embedded::create_issue_within`] and the others) or an
    /// import: [`Error::Announce`], or [`Error::NodeDeadline`] or
    /// [`Error::NodeBusy`] for `announce_refs_for`. The change itself was
    /// committed before it, and stands in storage: the error says only that
    /// the node didn't confirm the announcement (`NodeDeadline`, sent), wasn't
    /// sent it (`NodeBusy`), or that it failed otherwise (`Announce`: the
    /// node's failure, or the wait's). Any other error of a COB change leaves
    /// open whether the change was committed.
    pub fn is_announce(&self) -> bool {
        match self {
            Error::Announce(_) => true,
            Error::NodeDeadline { call, .. } | Error::NodeBusy { call, .. } => *call == ANNOUNCE,
            _ => false,
        }
    }

    /// Whether this is, or wraps, an out-of-memory error of libgit2
    /// (`GIT_ERROR_NOMEMORY`) or SQLite (`SQLITE_NOMEM`, or the extended
    /// `SQLITE_IOERR_NOMEM`). A service process ends with
    /// [`crate::OUT_OF_MEMORY_EXIT`] when one reaches it, and never answers it
    /// as the request's error (freedom-core design 0010, "The memory cap").
    ///
    /// It is the backstop behind [`crate::process_init`]'s hooks. With them
    /// installed, an allocation libgit2 or SQLite can't get has already ended
    /// the process. What can still arrive here is an out-of-memory error that
    /// no allocation of theirs failed:
    ///
    /// - zlib's, inside libgit2: zlib allocates through its own default, and
    ///   libgit2 reports its `Z_MEM_ERROR` from inflating or deflating a
    ///   stream (packs, loose objects, the indexer) as `GIT_ERROR_NOMEMORY`;
    /// - libgit2's size checks: a size whose arithmetic overflows is
    ///   `GIT_ERROR_NOMEMORY`, with no allocation tried;
    /// - SQLite's own refusal of an allocation of 2^31 - 256 bytes or more,
    ///   which never reaches its `xMalloc`.
    ///
    /// It follows the error's chain: `source()`, an `io::Error`'s inner
    /// error, this crate's variants, and the errors that the heartwood types
    /// those hold, and the public types they hold, wrap without making them
    /// a `source()` (thiserror's `transparent`, or a field without
    /// `#[source]`). It doesn't see an out-of-memory error:
    ///
    /// - made text, by heartwood or by libradicle: a fetch's failure
    ///   ([`Error::FetchFailed`], from heartwood's `FetchResult::Failed`), the
    ///   node's control replies ([`radicle::node::Error`]'s `Node` and
    ///   `Command`), an announcement's ([`Error::Announce`]), the node
    ///   thread's ([`Error::NodeThread`]), and [`Error::GitConfig`];
    /// - wrapped `transparent`ly in a type heartwood keeps private
    ///   (radicle-surf's reference, branch, tag, commit and repository
    ///   errors; the signed refs' boxed reference and object errors;
    ///   radicle-cob's change errors), or in radicle-protocol's service error
    ///   (`radicle_node::runtime::Error::Service`), which this crate doesn't
    ///   depend on;
    /// - zlib's `deflateInit` refused for want of memory when libgit2 writes
    ///   a loose object: libgit2 reports it as `GIT_ERROR_ZLIB` ("failed to
    ///   initialize zlib"), which isn't counted, since a corrupt object's
    ///   inflate is `GIT_ERROR_ZLIB` too.
    pub fn is_out_of_memory(&self) -> bool {
        crate::oom::in_chain(self)
    }

    /// The error this one wraps without making it its `source()`: each
    /// `transparent` variant's, whose `source()` is the wrapped error's own
    /// `source()` and skips that error ([`Error::is_out_of_memory`]).
    pub(crate) fn wrapped(&self) -> Option<&(dyn std::error::Error + 'static)> {
        use std::error::Error as StdError;
        fn dyn_error<E: StdError + 'static>(e: &E) -> &(dyn StdError + 'static) {
            e
        }
        match self {
            Error::Io(e) => Some(dyn_error(e)),
            Error::Profile(e) => Some(dyn_error(e)),
            Error::Keystore(e) => Some(dyn_error(e)),
            Error::Runtime(e) => Some(dyn_error(e)),
            Error::Node(e) => Some(dyn_error(e)),
            Error::NodeHandle(e) => Some(dyn_error(e)),
            Error::Storage(e) => Some(dyn_error(e)),
            Error::StorageRefs(e) => Some(dyn_error(e)),
            Error::Repository(e) => Some(dyn_error(e)),
            Error::Surf(e) => Some(dyn_error(e)),
            Error::Git(e) => Some(dyn_error(e)),
            Error::Doc(e) => Some(dyn_error(e)),
            Error::Payload(e) => Some(dyn_error(e)),
            Error::Json(e) => Some(dyn_error(e)),
            Error::Cob(e) => Some(dyn_error(e)),
            Error::IssueCache(e) => Some(dyn_error(e)),
            Error::Issue(e) => Some(dyn_error(e)),
            Error::PatchCache(e) => Some(dyn_error(e)),
            Error::Patch(e) => Some(dyn_error(e)),
            Error::Signer(e) => Some(dyn_error(e)),
            Error::Init(e) => Some(dyn_error(e)),
            Error::Policy(e) => Some(dyn_error(e)),
            Error::Sqlite(e) => Some(dyn_error(e)),
            Error::Reconcile(e) => Some(dyn_error(&**e)),
            Error::Quarantine(e) => Some(dyn_error(&**e)),
            Error::Remove(e) => Some(dyn_error(&**e)),
            // Its `source()` is the alias error itself.
            Error::Alias(_) => None,
            Error::NoProject
            | Error::NoSeeds(_)
            | Error::FetchFailed(..)
            | Error::Timeout(_)
            | Error::Cancelled
            | Error::Announce(_)
            | Error::NodeThread(_)
            | Error::ShutdownTimeout(_)
            | Error::NodeDeadline { .. }
            | Error::NodeBusy { .. }
            | Error::InvalidCobId(_)
            | Error::NotATree(_)
            | Error::NotABlob(_)
            | Error::TooLarge { .. }
            | Error::TooDeep { .. }
            | Error::NoIdentityHead(_)
            | Error::NoCanonicalHead(_)
            | Error::InvalidRevision(_)
            | Error::InvalidImport(_)
            | Error::Exists(_)
            | Error::Gated { .. }
            | Error::Open(_)
            | Error::GitConfig(_)
            | Error::ProcessInit(_) => None,
        }
    }
}

impl From<radicle::storage::git::reconcile::driver::Error> for Error {
    fn from(e: radicle::storage::git::reconcile::driver::Error) -> Self {
        Self::Reconcile(Box::new(e))
    }
}

impl From<radicle::storage::git::reconcile::quarantine::Error> for Error {
    fn from(e: radicle::storage::git::reconcile::quarantine::Error) -> Self {
        Self::Quarantine(Box::new(e))
    }
}

impl From<radicle::storage::git::reconcile::quarantine::RemoveError> for Error {
    fn from(e: radicle::storage::git::reconcile::quarantine::RemoveError) -> Self {
        Self::Remove(Box::new(e))
    }
}
