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
    #[error("announce refs failed: {0}")]
    Announce(String),
    #[error("node thread: {0}")]
    NodeThread(String),
    #[error("the node didn't stop within {0:?}")]
    ShutdownTimeout(std::time::Duration),
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
