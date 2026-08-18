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
    Repository(#[from] radicle::storage::RepositoryError),
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
    #[error("node thread: {0}")]
    NodeThread(String),
    #[error("not a directory: {0}")]
    NotATree(String),
    #[error("not a file: {0}")]
    NotABlob(String),
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
    #[error(transparent)]
    Policy(#[from] radicle::node::policy::store::Error),
}
