//! Embeddable Radicle facade.
//!
//! One struct, [`Embedded`], owns a Radicle profile and an in-process
//! `radicle-node` runtime running on a background thread. All reads go
//! straight to storage (bare git repos + sqlite) — no radicle-httpd, no
//! child processes on the client path.
//!
//! This is the shared core for the Freedom Browser desktop (napi-rs)
//! and iOS (UniFFI) bindings.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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
    pub delegates: Vec<String>,
    pub threshold: usize,
    pub visibility: Visibility,
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

/// Git signature information exposed with a commit.
#[derive(Debug, Clone)]
pub struct CommitSignature {
    pub name: String,
    pub email: String,
    pub time: i64,
}

/// Commit metadata used by repository browsers.
#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub id: String,
    pub summary: String,
    pub description: String,
    pub author: CommitSignature,
    pub committer: CommitSignature,
}

/// One signed Radicle remote and its published branch heads.
#[derive(Debug, Clone)]
pub struct RemoteInfo {
    pub id: String,
    pub did: String,
    pub delegate: bool,
    pub heads: BTreeMap<String, String>,
}

/// Repository history statistics at a specific revision.
#[derive(Debug, Clone)]
pub struct RepoStats {
    pub commits: usize,
    pub branches: usize,
    pub contributors: usize,
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

/// A progress event emitted while cloning/fetching a repository.
///
/// These are the peer-level ("Tier 1") transitions the clone loop drives
/// itself; byte-level object counts are a separate, later addition that
/// requires node-side plumbing.
#[derive(Debug, Clone)]
pub enum Progress {
    /// Candidate seeds resolved; the fetch is about to begin.
    Resolving { candidates: usize },
    /// Connecting to a candidate seed (1-based `index` of `total`).
    Connecting {
        nid: String,
        addr: String,
        index: usize,
        total: usize,
    },
    /// Fetching pack data from a connected seed.
    Fetching {
        nid: String,
        index: usize,
        total: usize,
    },
    /// A candidate failed; the loop advances to the next.
    PeerFailed {
        nid: String,
        index: usize,
        total: usize,
        reason: String,
    },
    /// The fetch completed successfully.
    Done,
    /// The fetch failed after exhausting candidates or timing out.
    Failed { reason: String },
    /// The fetch was cancelled by the host.
    Cancelled,
}

impl Progress {
    /// A stable `phase` tag for this event (host-facing).
    pub fn phase(&self) -> &'static str {
        match self {
            Progress::Resolving { .. } => "resolving",
            Progress::Connecting { .. } => "connecting",
            Progress::Fetching { .. } => "fetching",
            Progress::PeerFailed { .. } => "peer-failed",
            Progress::Done => "done",
            Progress::Failed { .. } => "failed",
            Progress::Cancelled => "cancelled",
        }
    }
}

/// Retry and timeout policy for a clone/fetch.
#[derive(Debug, Clone)]
pub struct FetchPolicy {
    /// Overall wall-clock budget for the whole clone across all candidates.
    pub overall_timeout: Duration,
    /// Per-attempt timeout handed to the node's fetch command.
    pub peer_timeout: Duration,
    /// Retries per candidate while its session is still coming up
    /// ("not connected"); other failures advance immediately.
    pub max_retries_per_peer: u32,
    /// Delay between per-peer retries.
    pub backoff: Duration,
}

impl Default for FetchPolicy {
    fn default() -> Self {
        Self {
            overall_timeout: Duration::from_secs(120),
            peer_timeout: Duration::from_secs(60),
            max_retries_per_peer: 240, // ~2 min of 500ms handshake polling
            backoff: Duration::from_millis(500),
        }
    }
}

impl FetchPolicy {
    /// Policy from a single overall timeout, keeping sensible defaults for
    /// the rest (used by the legacy `clone_repo(rid, timeout)` entry point).
    pub fn from_timeout(timeout: Duration) -> Self {
        Self {
            overall_timeout: timeout,
            peer_timeout: timeout,
            ..Self::default()
        }
    }
}

/// A cheap, cloneable cancellation flag shared with a running clone.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    /// Request cancellation of the associated clone.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
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
        self.clone_repo_with(
            rid,
            &FetchPolicy::from_timeout(timeout),
            &CancelToken::new(),
            &mut |_| {},
        )
    }

    /// Seed + fetch a repository, streaming [`Progress`] to `progress` and
    /// honoring `cancel`. Retry/timeout behavior comes from `policy`.
    ///
    /// The per-peer fetch is a blocking node command that cannot be
    /// interrupted mid-transfer, so cancellation is observed between
    /// candidates and during handshake-retry backoff — prompt in practice
    /// since a stuck peer sits in the backoff loop.
    pub fn clone_repo_with(
        &mut self,
        rid: RepoId,
        policy: &FetchPolicy,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<(), Error> {
        let result = self.run_clone(rid, policy, cancel, progress);
        match &result {
            Ok(()) => progress(Progress::Done),
            Err(Error::Cancelled) => progress(Progress::Cancelled),
            Err(e) => progress(Progress::Failed {
                reason: e.to_string(),
            }),
        }
        result
    }

    fn run_clone(
        &mut self,
        rid: RepoId,
        policy: &FetchPolicy,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<(), Error> {
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
        progress(Progress::Resolving {
            candidates: candidates.len(),
        });

        // Addresses for nicer "Connecting to <host>" progress labels.
        let addr_of = |nid: &NodeId| -> String {
            self.profile
                .config
                .preferred_seeds
                .iter()
                .find(|s| s.id == *nid)
                .map(|s| s.addr.to_string())
                .unwrap_or_else(|| nid.to_string())
        };

        let total = candidates.len();
        let mut last_err: Option<String> = None;
        let deadline = Instant::now() + policy.overall_timeout;

        for (i, nid) in candidates.into_iter().enumerate() {
            let index = i + 1;
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout(rid));
            }
            progress(Progress::Connecting {
                nid: nid.to_string(),
                addr: addr_of(&nid),
                index,
                total,
            });

            let mut retries = 0u32;
            let mut announced_fetching = false;
            loop {
                if cancel.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                if Instant::now() >= deadline {
                    return Err(Error::Timeout(rid));
                }
                // Emit "fetching" once per peer; the "not connected" retries
                // below are handshake waiting, not repeated fetches.
                if !announced_fetching {
                    progress(Progress::Fetching {
                        nid: nid.to_string(),
                        index,
                        total,
                    });
                    announced_fetching = true;
                }

                let outcome = self.handle.fetch(rid, nid, policy.peer_timeout, None);
                let reason = match outcome {
                    Ok(FetchResult::Success { .. }) => return Ok(()),
                    Ok(FetchResult::Failed { reason }) => reason,
                    Err(e) => e.to_string(),
                };
                log::warn!(target: "libradicle", "fetch {rid} from {nid}: {reason}");
                last_err = Some(reason.clone());

                // "not connected" means the session is still handshaking;
                // retry the same peer with backoff up to the cap.
                let retryable = reason.contains("not connected")
                    && retries < policy.max_retries_per_peer
                    && Instant::now() + policy.backoff < deadline;
                if retryable {
                    retries += 1;
                    if cancel_sleep(&policy.backoff, cancel) {
                        return Err(Error::Cancelled);
                    }
                    continue;
                }

                progress(Progress::PeerFailed {
                    nid: nid.to_string(),
                    index,
                    total,
                    reason,
                });
                break;
            }
        }
        Err(Error::FetchFailed(
            rid,
            last_err.unwrap_or_else(|| "no candidate succeeded".into()),
        ))
    }
}

/// Sleep for `dur` but wake early if cancellation is requested.
/// Returns true if cancellation was observed.
fn cancel_sleep(dur: &Duration, cancel: &CancelToken) -> bool {
    let step = Duration::from_millis(50);
    let end = Instant::now() + *dur;
    while Instant::now() < end {
        if cancel.is_cancelled() {
            return true;
        }
        std::thread::sleep(step.min(end.saturating_duration_since(Instant::now())));
    }
    cancel.is_cancelled()
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

        let socket = profile.home.socket_from_env();
        prepare_control_socket(&socket)?;

        let (sig_tx, sig_rx) = mpsc::channel();
        let runtime = Runtime::init(
            profile.home.clone(),
            profile.config.node.clone(),
            socket,
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

    /// Seed + fetch with streamed [`Progress`] and cancellation.
    pub fn clone_repo_with(
        &self,
        rid: RepoId,
        policy: &FetchPolicy,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<(), Error> {
        self.network()
            .clone_repo_with(rid, policy, cancel, progress)
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

    fn announce_refs(&self, rid: RepoId) {
        let mut handle = self.handle.clone();
        if let Err(err) = handle.announce_refs_for(rid, [*self.profile.did()]) {
            log::warn!(target: "libradicle", "announce refs for {rid}: {err}");
        }
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
        self.announce_refs(rid);
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
        self.announce_refs(rid);
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
        self.announce_refs(rid);
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
        self.announce_refs(rid);
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
        self.announce_refs(rid);
        Ok(rid)
    }

    /// Read all issues in repository storage.
    pub fn issues(&self, rid: RepoId) -> Result<Vec<(cob::ObjectId, cob::issue::Issue)>, Error> {
        let repo = self.profile.storage.repository(rid)?;
        let issues = self.profile.issues(&repo)?;
        let result = issues.list()?.collect::<Result<Vec<_>, _>>()?;
        Ok(result)
    }

    /// Read one issue from repository storage.
    pub fn issue(
        &self,
        rid: RepoId,
        issue_id: &str,
    ) -> Result<(cob::ObjectId, cob::issue::Issue), Error> {
        validate_cob_id(issue_id)?;
        let repo = self.profile.storage.repository(rid)?;
        let id: cob::ObjectId = repo.backend.revparse_single(issue_id)?.id().into();
        let issues = self.profile.issues(&repo)?;
        issues
            .get(&id)?
            .map(|issue| (id, issue))
            .ok_or_else(|| Error::NodeThread(format!("issue {issue_id} not found")))
    }

    /// Read all patches in repository storage.
    pub fn patches(&self, rid: RepoId) -> Result<Vec<(cob::ObjectId, cob::patch::Patch)>, Error> {
        let repo = self.profile.storage.repository(rid)?;
        let patches = self.profile.patches(&repo)?;
        let result = patches.list()?.collect::<Result<Vec<_>, _>>()?;
        Ok(result)
    }

    /// Read one patch from repository storage.
    pub fn patch(
        &self,
        rid: RepoId,
        patch_id: &str,
    ) -> Result<(cob::ObjectId, cob::patch::Patch), Error> {
        validate_cob_id(patch_id)?;
        let repo = self.profile.storage.repository(rid)?;
        let id: cob::ObjectId = repo.backend.revparse_single(patch_id)?.id().into();
        let patches = self.profile.patches(&repo)?;
        patches
            .get(&id)?
            .map(|patch| (id, patch))
            .ok_or_else(|| Error::NodeThread(format!("patch {patch_id} not found")))
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
            delegates: doc
                .delegates()
                .as_ref()
                .iter()
                .map(ToString::to_string)
                .collect(),
            threshold: doc.threshold(),
            visibility: doc.visibility().clone(),
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

    fn commit_at<'repo>(
        repo: &'repo radicle::storage::git::Repository,
        revision: &str,
    ) -> Result<git2::Commit<'repo>, Error> {
        let oid = parse_revision(revision)?;
        Ok(repo.backend.find_commit(oid)?)
    }

    fn commit_info(commit: &git2::Commit<'_>) -> CommitInfo {
        fn signature(value: git2::Signature<'_>) -> CommitSignature {
            CommitSignature {
                name: value.name().unwrap_or_default().to_string(),
                email: value.email().unwrap_or_default().to_string(),
                time: value.when().seconds(),
            }
        }

        let message = commit.message().unwrap_or_default();
        let mut lines = message.lines();
        let summary = lines.next().unwrap_or_default().to_string();
        let description = lines.collect::<Vec<_>>().join("\n");
        CommitInfo {
            id: commit.id().to_string(),
            summary,
            description,
            author: signature(commit.author()),
            committer: signature(commit.committer()),
        }
    }

    /// Entries of the tree at a specific commit, under `path`
    /// (empty = repository root).
    pub fn tree_at(
        &self,
        rid: RepoId,
        revision: &str,
        path: &str,
    ) -> Result<(Vec<TreeEntry>, CommitInfo), Error> {
        let repo = self.profile.storage.repository(rid)?;
        let commit = Self::commit_at(&repo, revision)?;
        let commit_info = Self::commit_info(&commit);
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
        let entries = tree
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
            .collect();
        Ok((entries, commit_info))
    }

    /// Entries of the tree at the head of the default branch, under
    /// `path` (empty = repository root).
    pub fn tree(&self, rid: RepoId, path: &str) -> Result<Vec<TreeEntry>, Error> {
        let repo = self.profile.storage.repository(rid)?;
        let (_, head) = repo.head()?;
        Ok(self.tree_at(rid, &head.to_string(), path)?.0)
    }

    /// Blob content at a specific commit.
    pub fn read_blob_at(&self, rid: RepoId, revision: &str, path: &str) -> Result<Blob, Error> {
        let repo = self.profile.storage.repository(rid)?;
        let commit = Self::commit_at(&repo, revision)?;
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

    /// Blob content at the head of the default branch.
    pub fn read_blob(&self, rid: RepoId, path: &str) -> Result<Blob, Error> {
        let repo = self.profile.storage.repository(rid)?;
        let (_, head) = repo.head()?;
        self.read_blob_at(rid, &head.to_string(), path)
    }

    /// Signed repository remotes and their published branch heads.
    pub fn remotes(&self, rid: RepoId) -> Result<Vec<RemoteInfo>, Error> {
        let repo = self.profile.storage.repository(rid)?;
        let doc = repo.identity_doc()?;
        let mut result = Vec::new();
        for remote in repo.remotes()? {
            let (id, remote) = remote?;
            let heads = remote
                .iter()
                .filter_map(|(name, oid)| {
                    name.as_str()
                        .strip_prefix("refs/heads/")
                        .map(|branch| (branch.to_string(), oid.to_string()))
                })
                .collect();
            result.push(RemoteInfo {
                id: id.to_string(),
                did: radicle::identity::Did::from(id).to_string(),
                delegate: doc.is_delegate(&id.into()),
                heads,
            });
        }
        result.sort_by(|a, b| b.delegate.cmp(&a.delegate).then_with(|| a.id.cmp(&b.id)));
        Ok(result)
    }

    /// Commit, branch, and contributor counts reachable from `revision`.
    pub fn repo_stats(&self, rid: RepoId, revision: &str) -> Result<RepoStats, Error> {
        let repo = self.profile.storage.repository(rid)?;
        let commit = Self::commit_at(&repo, revision)?;
        let mut walk = repo.backend.revwalk()?;
        walk.push(commit.id())?;

        let mut commits = 0;
        let mut contributors = HashSet::new();
        for oid in walk {
            let commit = repo.backend.find_commit(oid?)?;
            let author = commit.author();
            contributors.insert((
                author.name().unwrap_or_default().to_string(),
                author.email().unwrap_or_default().to_string(),
            ));
            commits += 1;
        }

        let branches = self
            .remotes(rid)?
            .into_iter()
            .flat_map(|remote| remote.heads.into_keys())
            .collect::<HashSet<_>>()
            .len();
        Ok(RepoStats {
            commits,
            branches,
            contributors: contributors.len(),
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

/// Remove a Unix control socket left behind by an unclean process exit.
///
/// A successful connection means another node really is listening, so the
/// path is preserved and `Runtime::init` reports `AlreadyRunning`. We only
/// remove an existing socket after the kernel confirms that nobody accepts
/// connections on it; regular files and other path types are never touched.
#[cfg(unix)]
fn prepare_control_socket(path: &Path) -> Result<(), Error> {
    use std::io::ErrorKind;
    use std::os::unix::fs::FileTypeExt as _;
    use std::os::unix::net::UnixStream;

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    if !metadata.file_type().is_socket() {
        return Ok(());
    }

    match UnixStream::connect(path) {
        Ok(_) => Ok(()),
        Err(err) if err.kind() == ErrorKind::ConnectionRefused => {
            std::fs::remove_file(path)?;
            Ok(())
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(_) => Ok(()),
    }
}

#[cfg(not(unix))]
fn prepare_control_socket(_path: &Path) -> Result<(), Error> {
    Ok(())
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

fn parse_revision(revision: &str) -> Result<git2::Oid, Error> {
    if revision.len() != 40 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Error::InvalidRevision(revision.to_string()));
    }
    git2::Oid::from_str(revision).map_err(|_| Error::InvalidRevision(revision.to_string()))
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use std::path::PathBuf;
    #[cfg(unix)]
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{parse_revision, prepare_control_socket, validate_cob_id};
    use super::{CancelToken, FetchPolicy, Progress};
    use std::time::{Duration, Instant};

    #[test]
    fn cancel_token_flips() {
        let t = CancelToken::new();
        assert!(!t.is_cancelled());
        let clone = t.clone();
        clone.cancel();
        assert!(t.is_cancelled(), "cancel must be visible across clones");
    }

    #[test]
    fn policy_from_timeout_keeps_defaults() {
        let p = FetchPolicy::from_timeout(Duration::from_secs(30));
        assert_eq!(p.overall_timeout, Duration::from_secs(30));
        assert_eq!(p.peer_timeout, Duration::from_secs(30));
        assert_eq!(p.backoff, FetchPolicy::default().backoff);
    }

    #[test]
    fn progress_phase_tags_are_stable() {
        assert_eq!(Progress::Done.phase(), "done");
        assert_eq!(Progress::Cancelled.phase(), "cancelled");
        assert_eq!(Progress::Resolving { candidates: 2 }.phase(), "resolving");
    }

    #[test]
    fn cancel_sleep_wakes_early() {
        let t = CancelToken::new();
        t.cancel();
        let start = Instant::now();
        assert!(super::cancel_sleep(&Duration::from_secs(5), &t));
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "must not sleep the full duration"
        );
    }

    #[cfg(unix)]
    fn socket_test_dir(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is before Unix epoch")
            .as_nanos();
        let path =
            PathBuf::from("/tmp").join(format!("libradicle-{name}-{}-{nonce}", std::process::id()));
        fs::create_dir(&path).expect("create socket test directory");
        path
    }

    #[cfg(unix)]
    #[test]
    fn startup_removes_only_stale_control_sockets() {
        use std::os::unix::net::UnixListener;

        let dir = socket_test_dir("stale-socket");
        let socket = dir.join("control.sock");
        let listener = UnixListener::bind(&socket).expect("bind stale socket");
        drop(listener);

        prepare_control_socket(&socket).expect("remove stale socket");

        assert!(!socket.exists());
        fs::remove_dir(dir).expect("remove socket test directory");
    }

    #[cfg(unix)]
    #[test]
    fn startup_preserves_live_control_sockets_and_regular_files() {
        use std::os::unix::net::UnixListener;

        let dir = socket_test_dir("live-socket");
        let socket = dir.join("control.sock");
        let listener = UnixListener::bind(&socket).expect("bind live socket");

        prepare_control_socket(&socket).expect("probe live socket");
        assert!(socket.exists());

        let regular_file = dir.join("not-a-socket");
        fs::write(&regular_file, b"keep").expect("create regular file");
        prepare_control_socket(&regular_file).expect("inspect regular file");
        assert_eq!(fs::read(&regular_file).expect("read regular file"), b"keep");

        drop(listener);
        fs::remove_file(socket).expect("remove live socket");
        fs::remove_file(regular_file).expect("remove regular file");
        fs::remove_dir(dir).expect("remove socket test directory");
    }

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

    #[test]
    fn revisions_accept_only_full_commit_object_ids() {
        assert!(parse_revision("0123456789abcdef0123456789abcdef01234567").is_ok());

        for invalid in [
            "HEAD",
            "main~3",
            "0123456",
            "0123456789abcdef0123456789abcdef012345678",
            "0123456789abcdef0123456789abcdef0123456g",
        ] {
            assert!(parse_revision(invalid).is_err(), "accepted {invalid:?}");
        }
    }
}
