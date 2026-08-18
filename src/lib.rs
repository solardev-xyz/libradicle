//! Embeddable Radicle facade.
//!
//! One struct, [`Embedded`], owns a Radicle profile and an in-process
//! `radicle-node` runtime running on a background thread. All reads go
//! straight to storage (bare git repos + sqlite) — no radicle-httpd, no
//! child processes on the client path.
//!
//! This is the shared core for the Freedom Browser desktop (napi-rs)
//! and iOS (UniFFI) bindings.

use std::path::PathBuf;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use radicle::cob;
use radicle::cob::common::Label;
use radicle::cob::issue::{CloseReason, State as IssueState};
use radicle::crypto::ssh::Keystore;
use radicle::identity::project::ProjectName;
use radicle::identity::RepoId;
use radicle::identity::Visibility;
use radicle::issue::cache::Issues as _;
use radicle::node::policy::Scope;
use radicle::node::{ConnectOptions, ConnectResult, FetchResult, Handle as _};
use radicle::patch::{cache::Patches as _, ByRevision};
use radicle::prelude::*;
use radicle::profile::{Home, Profile, Signer};
use radicle::storage::{ReadRepository, ReadStorage, WriteStorage};
use radicle_node::runtime::handle::Handle as NodeHandle;
use radicle_node::runtime::Runtime;

pub mod error;
pub use error::Error;

/// Options for starting an embedded Radicle stack.
#[derive(Debug, Clone)]
pub struct Options {
    /// Radicle home directory (profile, keys, storage, node dbs).
    pub home: PathBuf,
    /// Alias to use when initializing a fresh profile.
    pub alias: String,
    /// Addresses to listen on for inbound connections. Empty = outbound-only.
    pub listen: Vec<std::net::SocketAddr>,
}

/// Summary of a repository's identity payload plus head.
#[derive(Debug, Clone)]
pub struct RepoInfo {
    pub rid: RepoId,
    pub name: String,
    pub description: String,
    pub default_branch: String,
    pub head: String,
    pub issues_open: usize,
    pub patches_open: usize,
}

/// One entry in a repository tree listing.
#[derive(Debug, Clone)]
pub struct TreeEntry {
    pub name: String,
    pub path: String,
    /// "tree" | "blob" | "submodule"
    pub kind: String,
}

/// Blob content read from storage.
#[derive(Debug, Clone)]
pub struct Blob {
    pub binary: bool,
    pub content: Vec<u8>,
}

/// Public identity exposed to the browser provider.
#[derive(Debug, Clone)]
pub struct Identity {
    pub did: String,
    pub nid: String,
    pub alias: String,
}

/// Cloneable network-only view of an embedded node.
///
/// Hosts can take this snapshot while holding their lifecycle lock, then run
/// connect/fetch operations after releasing it. Storage reads and COB writes
/// therefore remain responsive while a network request is in flight.
#[derive(Clone)]
pub struct Network {
    profile: Profile,
    handle: NodeHandle,
}

impl Network {
    /// Connect to the profile's configured preferred seeds.
    /// Returns the number of seeds successfully connected.
    pub fn connect_preferred_seeds(&mut self, timeout: Duration) -> Result<usize, Error> {
        let mut connected = 0;
        let seeds = self.profile.config.preferred_seeds.clone();
        for seed in seeds {
            let opts = ConnectOptions {
                persistent: true,
                timeout,
            };
            match self.handle.connect(seed.id, seed.addr.clone(), opts) {
                Ok(ConnectResult::Connected) => connected += 1,
                Ok(ConnectResult::Disconnected { reason }) => {
                    log::warn!(target: "libradicle", "connect {}: {reason}", seed.addr);
                }
                Err(e) => log::warn!(target: "libradicle", "connect {}: {e}", seed.addr),
            }
        }
        Ok(connected)
    }

    /// Seed + fetch a repository from the network into local storage.
    /// No working copy is created; the bare repo in storage is the product.
    pub fn clone_repo(&mut self, rid: RepoId, timeout: Duration) -> Result<(), Error> {
        self.handle.seed(rid, Scope::All)?;

        // Try nodes that are known to seed this repo, preferring connected ones.
        let seeds = self.handle.seeds_for(rid, [*self.profile.did()])?;
        let (connected, disconnected) = seeds.partition();
        let mut candidates: Vec<_> = connected
            .into_iter()
            .map(|s| s.nid)
            .chain(disconnected.into_iter().map(|s| s.nid))
            .collect();

        // A fresh node has an empty routing table until gossip arrives;
        // fall back to the configured preferred seeds (same as `rad clone`).
        for seed in &self.profile.config.preferred_seeds {
            if !candidates.contains(&seed.id) {
                candidates.push(seed.id);
            }
        }

        if candidates.is_empty() {
            return Err(Error::NoSeeds(rid));
        }
        let mut last_err: Option<String> = None;
        let deadline = std::time::Instant::now() + timeout;
        for nid in candidates {
            // Session handshakes complete asynchronously after `connect`
            // returns; retry while the peer is still coming up.
            loop {
                match self.handle.fetch(rid, nid, timeout, None) {
                    Ok(FetchResult::Success { .. }) => return Ok(()),
                    Ok(FetchResult::Failed { reason }) => {
                        let retryable = reason.contains("not connected");
                        log::warn!(target: "libradicle", "fetch {rid} from {nid}: {reason}");
                        last_err = Some(reason);
                        if retryable && std::time::Instant::now() < deadline {
                            std::thread::sleep(Duration::from_millis(500));
                            continue;
                        }
                        break;
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        let retryable = msg.contains("not connected");
                        log::warn!(target: "libradicle", "fetch {rid} from {nid}: {msg}");
                        last_err = Some(msg);
                        if retryable && std::time::Instant::now() < deadline {
                            std::thread::sleep(Duration::from_millis(500));
                            continue;
                        }
                        break;
                    }
                }
            }
        }
        Err(Error::FetchFailed(
            rid,
            last_err.unwrap_or_else(|| "no candidate succeeded".into()),
        ))
    }
}

/// An embedded Radicle stack: profile + in-process node.
pub struct Embedded {
    profile: Profile,
    handle: NodeHandle,
    node_thread: Option<JoinHandle<Result<(), radicle_node::runtime::Error>>>,
    /// Keeps the node's signal channel open for the runtime's lifetime.
    _signals: mpsc::Sender<radicle_signals::Signal>,
}

impl Embedded {
    /// Initialize (or load) the profile at `opts.home` and start the node
    /// runtime on a background thread.
    ///
    /// The identity key is stored without a passphrase; key custody is the
    /// host's concern (OS keychain on iOS/desktop) and layered on later.
    pub fn start(opts: Options) -> Result<Self, Error> {
        let home = Home::new(opts.home.clone())?;
        let profile = if home.keys().join("radicle").exists() {
            Profile::load_from(home.clone())?
        } else {
            let alias = Alias::new(&opts.alias);
            let seed = radicle::profile::env::seed().unwrap_or_else(|| {
                use radicle::crypto::Seed;
                let mut seed = [0; Seed::BYTES];
                getrandom::fill(&mut seed).expect("failed to get OS randomness");
                Seed::new(seed)
            });
            Profile::init(home.clone(), alias, None, seed)?
        };

        // In-process signer: load the (unencrypted) secret key.
        let keystore = Keystore::from_secret_path(&profile.home.keys().join("radicle"));
        let signer = keystore
            .secret_key(None)?
            .ok_or_else(|| Error::NodeThread("secret key not found in keystore".into()))?;

        let (sig_tx, sig_rx) = mpsc::channel();
        let runtime = Runtime::init(
            profile.home.clone(),
            profile.config.node.clone(),
            profile.home.socket_from_env(),
            opts.listen.clone(),
            sig_rx,
            signer,
        )?;
        let handle = runtime.handle.clone();
        let node_thread = std::thread::Builder::new()
            .name("radicle-node".to_string())
            .spawn(move || runtime.run())?;

        Ok(Self {
            profile,
            handle,
            node_thread: Some(node_thread),
            _signals: sig_tx,
        })
    }

    /// The local node's DID.
    pub fn did(&self) -> String {
        self.profile.did().to_string()
    }

    /// The profile identity and public node alias.
    pub fn identity(&self) -> Identity {
        Identity {
            did: self.profile.did().to_string(),
            nid: self.profile.id().to_string(),
            alias: self.profile.config.node.alias.to_string(),
        }
    }

    /// Take a cloneable network-only view of this node.
    pub fn network(&self) -> Network {
        Network {
            profile: self.profile.clone(),
            handle: self.handle.clone(),
        }
    }

    /// Connect to the profile's configured preferred seeds.
    pub fn connect_preferred_seeds(&self, timeout: Duration) -> Result<usize, Error> {
        self.network().connect_preferred_seeds(timeout)
    }

    /// Seed + fetch a repository from the network into local storage.
    /// No working copy is created; the bare repo in storage is the product.
    pub fn clone_repo(&self, rid: RepoId, timeout: Duration) -> Result<(), Error> {
        self.network().clone_repo(rid, timeout)
    }

    /// Stop seeding a repository. The bare repository remains in storage.
    pub fn unseed_repo(&mut self, rid: RepoId) -> Result<bool, Error> {
        Ok(self.handle.unseed(rid)?)
    }

    /// List all locally stored repositories.
    pub fn list_repos(&self) -> Result<Vec<RepoInfo>, Error> {
        let policies = self.profile.policies()?;
        let mut repos = Vec::new();
        for repo in self.profile.storage.repositories()? {
            if policies.is_seeding(&repo.rid)? {
                repos.push(self.repo_info(repo.rid)?);
            }
        }
        Ok(repos)
    }

    fn signer(&self) -> Result<Signer, Error> {
        Ok(self.profile.signer()?)
    }

    fn announce_refs(&self, rid: RepoId) -> Result<(), Error> {
        let mut handle = self.handle.clone();
        handle.announce_refs_for(rid, [*self.profile.did()])?;
        Ok(())
    }

    /// Create an issue directly in the repository's COB store.
    pub fn create_issue(
        &self,
        rid: RepoId,
        title: &str,
        description: &str,
        labels: Vec<String>,
    ) -> Result<String, Error> {
        let repo = self.profile.storage.repository_mut(rid)?;
        let signer = self.signer()?;
        let mut issues = self.profile.issues_mut(&repo, &signer)?;
        let title = cob::Title::new(title).map_err(|e| Error::NodeThread(e.to_string()))?;
        let labels = labels
            .into_iter()
            .map(Label::new)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| Error::NodeThread(e.to_string()))?;
        let issue = issues.create(title, description, &labels, &[], [])?;
        let id = issue.id().to_string();
        self.announce_refs(rid)?;
        Ok(id)
    }

    /// Add a comment to an issue.
    pub fn comment_issue(
        &self,
        rid: RepoId,
        issue_id: &str,
        body: &str,
        reply_to: Option<&str>,
    ) -> Result<String, Error> {
        let repo = self.profile.storage.repository_mut(rid)?;
        let signer = self.signer()?;
        let mut issues = self.profile.issues_mut(&repo, &signer)?;
        validate_cob_id(issue_id)?;
        let issue_id: cob::ObjectId = repo.backend.revparse_single(issue_id)?.id().into();
        let mut issue = issues.get_mut(&issue_id)?;
        let parent = match reply_to {
            Some(id) => {
                validate_cob_id(id)?;
                repo.backend.revparse_single(id)?.id().into()
            }
            None => *issue.root().0,
        };
        let id = issue.comment(body, parent, [])?.to_string();
        self.announce_refs(rid)?;
        Ok(id)
    }

    /// Transition an issue between open, closed and solved.
    pub fn edit_issue_state(
        &self,
        rid: RepoId,
        issue_id: &str,
        state: &str,
    ) -> Result<String, Error> {
        let repo = self.profile.storage.repository_mut(rid)?;
        let signer = self.signer()?;
        let mut issues = self.profile.issues_mut(&repo, &signer)?;
        validate_cob_id(issue_id)?;
        let issue_id: cob::ObjectId = repo.backend.revparse_single(issue_id)?.id().into();
        let state = match state {
            "open" => IssueState::Open,
            "closed" => IssueState::Closed {
                reason: CloseReason::Other,
            },
            "solved" => IssueState::Closed {
                reason: CloseReason::Solved,
            },
            other => return Err(Error::NodeThread(format!("invalid issue state {other:?}"))),
        };
        let mut issue = issues.get_mut(&issue_id)?;
        issue.lifecycle(state)?;
        self.announce_refs(rid)?;
        Ok(issue_id.to_string())
    }

    /// Add a comment to a patch revision.
    pub fn comment_patch(
        &self,
        rid: RepoId,
        revision_id: &str,
        body: &str,
    ) -> Result<String, Error> {
        let repo = self.profile.storage.repository_mut(rid)?;
        let signer = self.signer()?;
        let mut patches = self.profile.patches_mut(&repo, &signer)?;
        validate_cob_id(revision_id)?;
        let entry: cob::EntryId = repo.backend.revparse_single(revision_id)?.id().into();
        let revision_id = radicle::cob::patch::RevisionId::from(entry);
        let ByRevision { id, patch, .. } = patches
            .find_by_revision(&revision_id)?
            .ok_or_else(|| Error::NodeThread(format!("patch revision {revision_id} not found")))?;
        let mut patch = radicle::cob::patch::PatchMut::new(id, patch, &mut patches);
        let id = patch
            .comment(revision_id, body, None, None, [])?
            .to_string();
        self.announce_refs(rid)?;
        Ok(id)
    }

    /// Import the default branch of an existing working Git repository.
    pub fn import_repo(
        &mut self,
        repo_path: &std::path::Path,
        name: &str,
        description: &str,
        default_branch: &str,
    ) -> Result<RepoId, Error> {
        let repo = radicle::git::raw::Repository::open(repo_path)?;
        let name = ProjectName::try_from(name).map_err(|e| Error::NodeThread(e.to_string()))?;
        let branch = radicle::git::BranchName::try_from(default_branch.to_string())
            .map_err(|e| Error::NodeThread(e.to_string()))?;
        let signer = self.signer()?;
        let (rid, _, _) = radicle::rad::init(
            &repo,
            name,
            description,
            branch,
            Visibility::Public,
            &signer,
            &self.profile.storage,
        )?;
        self.handle.seed(rid, Scope::All)?;
        self.handle.add_inventory(rid)?;
        self.announce_refs(rid)?;
        Ok(rid)
    }

    /// Read a repository's identity payload + head from local storage.
    pub fn repo_info(&self, rid: RepoId) -> Result<RepoInfo, Error> {
        let repo = self.profile.storage.repository(rid)?;
        let doc = repo.identity_doc()?;
        let proj = doc.project()?;
        let (_, head) = repo.head()?;

        let issues = self.profile.issues(&repo)?;
        let patches = self.profile.patches(&repo)?;

        Ok(RepoInfo {
            rid,
            name: proj.name().to_string(),
            description: proj.description().to_string(),
            default_branch: proj.default_branch().to_string(),
            head: head.to_string(),
            issues_open: issues.counts()?.open,
            patches_open: patches.counts()?.open,
        })
    }

    /// List file paths at the head of the default branch, straight from
    /// the bare repo in storage.
    pub fn list_files(&self, rid: RepoId) -> Result<Vec<String>, Error> {
        let repo = self.profile.storage.repository(rid)?;
        let (_, head) = repo.head()?;
        let commit = repo.backend.find_commit(head.into())?;
        let tree = commit.tree()?;
        let mut files = Vec::new();
        tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
            if entry.kind() == Some(git2::ObjectType::Blob) {
                files.push(format!("{dir}{}", entry.name().unwrap_or("?")));
            }
            git2::TreeWalkResult::Ok
        })?;
        Ok(files)
    }

    /// Number of currently connected peer sessions.
    pub fn connected_peers(&self) -> Result<usize, Error> {
        let sessions = self.handle.sessions()?;
        Ok(sessions
            .iter()
            .filter(|s| matches!(s.state, radicle::node::State::Connected { .. }))
            .count())
    }

    /// Number of nodes known to seed the given repository.
    pub fn seeders(&mut self, rid: RepoId) -> Result<usize, Error> {
        Ok(self.handle.seeds_for(rid, [*self.profile.did()])?.len())
    }

    /// Entries of the tree at the head of the default branch, under
    /// `path` (empty = repository root).
    pub fn tree(&self, rid: RepoId, path: &str) -> Result<Vec<TreeEntry>, Error> {
        let repo = self.profile.storage.repository(rid)?;
        let (_, head) = repo.head()?;
        let commit = repo.backend.find_commit(head.into())?;
        let root = commit.tree()?;
        let tree = if path.is_empty() {
            root
        } else {
            let entry = root.get_path(std::path::Path::new(path))?;
            entry
                .to_object(&repo.backend)?
                .into_tree()
                .map_err(|_| Error::NotATree(path.to_string()))?
        };
        let prefix = if path.is_empty() {
            String::new()
        } else {
            format!("{}/", path.trim_end_matches('/'))
        };
        Ok(tree
            .iter()
            .map(|e| {
                let name = e.name().unwrap_or("?").to_string();
                let kind = match e.kind() {
                    Some(git2::ObjectType::Tree) => "tree",
                    Some(git2::ObjectType::Commit) => "submodule",
                    _ => "blob",
                };
                TreeEntry {
                    path: format!("{prefix}{name}"),
                    name,
                    kind: kind.to_string(),
                }
            })
            .collect())
    }

    /// Blob content at the head of the default branch.
    pub fn read_blob(&self, rid: RepoId, path: &str) -> Result<Blob, Error> {
        let repo = self.profile.storage.repository(rid)?;
        let (_, head) = repo.head()?;
        let commit = repo.backend.find_commit(head.into())?;
        let entry = commit.tree()?.get_path(std::path::Path::new(path))?;
        let blob = entry
            .to_object(&repo.backend)?
            .into_blob()
            .map_err(|_| Error::NotABlob(path.to_string()))?;
        Ok(Blob {
            binary: blob.is_binary(),
            content: blob.content().to_vec(),
        })
    }

    /// Gracefully shut the node down and join its thread.
    pub fn shutdown(mut self) -> Result<(), Error> {
        self.handle.shutdown()?;
        if let Some(t) = self.node_thread.take() {
            t.join()
                .map_err(|_| Error::NodeThread("node thread panicked".into()))?
                .map_err(|e| Error::NodeThread(e.to_string()))?;
        }
        Ok(())
    }
}

fn validate_cob_id(id: &str) -> Result<(), Error> {
    if (6..=40).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Ok(())
    } else {
        Err(Error::InvalidCobId(id.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::validate_cob_id;

    #[test]
    fn cob_ids_accept_only_lowercase_hex_object_ids() {
        assert!(validate_cob_id("012abc").is_ok());
        assert!(validate_cob_id("0123456789abcdef0123456789abcdef01234567").is_ok());

        for invalid in [
            "012ab",
            "0123456789abcdef0123456789abcdef012345678",
            "HEAD",
            "main~3",
            "012ABC",
            "012abg",
            "éééééé",
        ] {
            assert!(validate_cob_id(invalid).is_err(), "accepted {invalid:?}");
        }
    }
}
