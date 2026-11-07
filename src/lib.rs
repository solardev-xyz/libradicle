//! Embeddable Radicle facade.
//!
//! One struct, [`Embedded`], owns a Radicle profile and an in-process
//! `radicle-node` runtime running on a background thread. All reads go
//! straight to storage (bare git repos + sqlite) — no radicle-httpd, no
//! child processes on the client path.
//!
//! This is the shared core for the Freedom Browser desktop (napi-rs)
//! and iOS (UniFFI) bindings.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
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
use radicle::storage::git::reconcile;
use radicle::storage::{ReadRepository, ReadStorage, RemoteRepository as _, WriteStorage};
use radicle_node::runtime::handle::Handle as NodeHandle;
use radicle_node::runtime::Runtime;

pub mod error;
pub use error::Error;
/// The dial gate every unit test node gets: loopback only, and loudly.
#[cfg(test)]
#[path = "../tests/common/hermetic.rs"]
mod hermetic;
pub mod json;
mod meter;
mod objects;
#[cfg(test)]
mod seams;
mod seeds;

use meter::Meter;
pub use objects::{commit_held, raw_held, tree_held};
use objects::{Objects, Walk};

/// Options for starting an embedded Radicle stack.
#[derive(Debug, Clone)]
pub struct Options {
    /// Radicle home directory (profile, keys, storage, node dbs).
    pub home: PathBuf,
    /// Alias to use when initializing a fresh profile. One Radicle refuses
    /// (empty, over 32 bytes, or with whitespace or control characters) is
    /// [`Error::Alias`] on a fresh home, before anything is written to it.
    pub alias: String,
    /// Addresses to listen on for inbound connections. Empty = outbound-only.
    pub listen: Vec<std::net::SocketAddr>,
    /// Whether the node listens on a control socket (`<home>/node/control.sock`,
    /// or `RAD_SOCKET`) for `rad` and other local clients. Without one, the
    /// host controls the node through this handle alone.
    pub control_socket: bool,
    /// How the node reaches peers, set in memory over `config.json`. `None`
    /// leaves `config.json`'s routing as it is.
    pub routing: Option<Routing>,
    /// The node's default seeding policy, for a repository with no policy of
    /// its own, set in memory over the profile's: `config.json`'s
    /// `node.seedingPolicy`, or the deprecated `node.policy` and `node.scope`
    /// that heartwood lets override it. `config.json` is never written.
    /// `None` leaves the profile's as it is. freedom-core design 0008's
    /// service passes `Block`, so that its node seeds only what it is told
    /// to ("Seeding and serving").
    ///
    /// The override reaches this node and this `Embedded`'s own reads
    /// ([`Embedded::list_repos`] and the others), and nothing else. Since it
    /// isn't on disk, another reader of the same home still sees
    /// `config.json`'s default: the `rad` CLI, git's remote helper, or a
    /// fresh `Profile::load`. A host that needs every reader of the home to
    /// agree must either write `config.json` itself or keep the home to
    /// itself. Design 0008's service does the latter: its home is its own,
    /// and no CLI or remote helper runs on it.
    pub seeding_policy: Option<radicle::node::config::DefaultSeedingPolicy>,
    /// Whether the node reaches no one, whatever `config.json` and `routing`
    /// say (freedom-core design 0008, R-12 part 3):
    /// - its peer set is static, with no `connect` peers, so it takes no
    ///   bootstrap nodes and dials no one;
    /// - its dial gate refuses every target and resolves no name, so an
    ///   explicit connect, to the seed book among others, fails before any
    ///   socket, and no address a peer announces enters its address book;
    /// - it listens on no address, `listen` notwithstanding, and the shipped
    ///   seeds aren't added to its address book;
    /// - it has no proxy, and drops onion and I2P addresses, whatever
    ///   `routing` and `config.json` say: heartwood reaches a proxy without
    ///   asking the gate.
    ///
    /// It still serves the host from local storage.
    pub isolated: bool,
    /// The repository gate the node consults before it fetches a
    /// repository, updates its COB cache, announces it or serves it
    /// (heartwood's `RepoGate`; freedom-core design 0008, "The repository
    /// gate"). The host keeps a clone, which shares it, and sets each
    /// repository's state through it: being repaired, or quarantined.
    ///
    /// `None` gives the node a gate that admits every repository, as the
    /// node had before heartwood had one.
    pub repositories: Option<radicle::node::gate::RepoGate>,
}

/// The dial gate of an isolated node: it refuses every target, and resolves
/// no name.
#[derive(Debug)]
struct Isolated;

impl radicle::node::dial::Gate for Isolated {
    fn allows(&self, _: &radicle::node::Address, _: &std::net::SocketAddr) -> bool {
        false
    }

    fn resolve(&self, _: &str, _: u16) -> std::io::Result<Vec<std::net::SocketAddr>> {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "an isolated node resolves no name",
        ))
    }
}

/// How the node reaches peers. Given in [`Options::routing`], it replaces
/// every routing field of the profile's node configuration, in memory only:
/// `config.json` is never written, and whatever it says about routing has
/// no effect.
#[derive(Debug, Clone)]
pub struct Routing {
    /// The SOCKS5 proxy every peer dial goes through, or `None` to dial
    /// directly (`node.proxy`). Through a proxy, the node isolates each
    /// remote node's streams: it gives the proxy credentials of each peer's
    /// own (RFC 1929), and only those.
    pub proxy: Option<std::net::SocketAddr>,
    /// How onion addresses are reached (`node.onion`). I2P addresses are
    /// always dropped (`node.i2p`).
    pub onion: radicle::node::config::AddressConfig,
    /// Which addresses the node may dial, and how it resolves names.
    pub gate: Arc<dyn radicle::node::dial::Gate>,
}

impl Routing {
    /// The profile's node configuration with this routing in place: `proxy`
    /// and `onion` as given, I2P dropped, and no external addresses, so the
    /// node advertises no address of its own.
    fn apply(&self, config: &radicle::node::Config) -> radicle::node::Config {
        let mut config = config.clone();
        config.proxy = self.proxy;
        config.onion = self.onion;
        config.i2p = radicle::node::config::AddressConfig::Drop;
        config.external_addresses = vec![];
        config
    }

    /// The gate the node dials through: the host's, isolating each remote
    /// node's streams when the node dials through a proxy (freedom-core
    /// design 0008, R-12 part 4). heartwood then gives the proxy credentials
    /// of each remote node's own (RFC 1929), and only those, so that Arti
    /// keeps each peer's connections on circuits of their own.
    fn gate(&self) -> Arc<dyn radicle::node::dial::Gate> {
        if self.proxy.is_some() {
            Arc::new(Isolating(self.gate.clone()))
        } else {
            self.gate.clone()
        }
    }
}

/// A gate that isolates each remote node's streams through a proxy, and
/// otherwise is the one it wraps.
#[derive(Debug)]
struct Isolating(Arc<dyn radicle::node::dial::Gate>);

impl radicle::node::dial::Gate for Isolating {
    fn allows(&self, target: &radicle::node::Address, addr: &std::net::SocketAddr) -> bool {
        self.0.allows(target, addr)
    }

    fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<std::net::SocketAddr>> {
        self.0.resolve(host, port)
    }

    fn resolve_timeout(&self) -> std::time::Duration {
        self.0.resolve_timeout()
    }

    fn proxy_resolves_names(&self) -> bool {
        self.0.proxy_resolves_names()
    }

    fn isolates_streams(&self) -> bool {
        true
    }
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

/// A repository covered by an explicit allow-seeding policy. Metadata is
/// present once the repository has successfully landed in local storage.
#[derive(Debug, Clone)]
pub struct SeededRepoInfo {
    pub rid: RepoId,
    pub name: Option<String>,
    pub description: Option<String>,
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

/// A blob's object id and size, read from its object header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobHeader {
    pub oid: String,
    pub size: u64,
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

/// One commit and its diff against its first parent (or the empty tree for
/// an initial commit), in the same schema Radicle's HTTP API exposes.
#[derive(Debug, Clone)]
pub struct CommitDetail {
    pub commit: radicle_surf::Commit,
    pub diff: radicle_surf::diff::Diff,
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
    seed_book: Arc<Vec<seeds::Candidate>>,
    /// The node's repository gate ([`Options::repositories`]).
    repositories: radicle::node::gate::RepoGate,
}

/// One failed seed dial observed before bootstrap reached its readiness target
/// or exhausted its overall time budget.
#[derive(Debug, Clone)]
pub struct SeedConnectFailure {
    pub nid: String,
    pub addr: String,
    pub reason: String,
}

/// Diagnostics for one concurrent seed bootstrap pass.
#[derive(Debug, Clone)]
pub struct SeedConnectReport {
    pub attempted: usize,
    pub connected: usize,
    pub target: usize,
    pub target_reached: bool,
    pub elapsed: Duration,
    pub failures: Vec<SeedConnectFailure>,
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

/// A cheap, cloneable cancellation flag shared with a running clone, or with
/// a bounded walk ([`Bounds`]).
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    /// Request cancellation of the associated clone or walk.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// The limits on a read that walks storage, the `_bounded` reads: a
/// cancellation token, a budget in bytes of the result's JSON, as the
/// [`json`] module writes it, and a budget in bytes of the repository
/// objects (commits, trees) it may load.
///
/// A walk checks the token at each step, and stops with
/// [`Error::Cancelled`] once it is cancelled. It charges each item as it is
/// built, and stops with [`Error::TooLarge`] when the next one doesn't fit:
/// a result it returns never writes more JSON than its budget. It reads each
/// object's size from its header before loading it, and stops with
/// [`Error::TooLarge`] before loading one that `loads` has no room left for,
/// counting every object it has loaded so far, skipped ones included. A
/// tree or commit counts what libgit2 holds of it once parsed, from its
/// header size ([`tree_held`], [`commit_held`]): several times its size.
#[derive(Debug, Clone)]
pub struct Bounds {
    pub cancel: CancelToken,
    pub budget: u64,
    pub loads: u64,
}

impl Bounds {
    /// A JSON budget, and no limit on loads.
    pub fn new(cancel: CancelToken, budget: u64) -> Self {
        Self {
            cancel,
            budget,
            loads: u64::MAX,
        }
    }

    /// These bounds, loading at most `loads` bytes of objects.
    pub fn with_loads(mut self, loads: u64) -> Self {
        self.loads = loads;
        self
    }

    /// No limit, and a token nobody holds: what the unbounded reads use.
    pub fn unbounded() -> Self {
        Self::new(CancelToken::new(), u64::MAX)
    }

    /// The bounds of one read a binding serves: [`BINDING_READ_BUDGET`] of
    /// JSON, and as much of loaded objects, and a token nobody cancels.
    pub fn for_binding() -> Self {
        Self::new(CancelToken::new(), BINDING_READ_BUDGET).with_loads(BINDING_READ_BUDGET)
    }

    /// The loads budget, as a JSON budget: what a meter of loaded bytes
    /// counts against.
    fn loading(&self) -> Self {
        Self::new(self.cancel.clone(), self.loads)
    }
}

/// The most one read through the bindings (N-API, UniFFI) returns: the JSON
/// a walk may write, and a blob's size in bytes. The bindings run every read
/// under the node's global mutex on a host thread, so an unbounded one (a
/// huge blob, diff, history page or file list) holds both, and its whole
/// result in memory, for as long as it takes (R-18). Larger reads fail with
/// [`Error::TooLarge`].
pub const BINDING_READ_BUDGET: u64 = 16 * 1024 * 1024;

/// The largest page of commits a binding's `commits` returns, whatever page
/// size it is asked for.
pub const BINDING_MAX_PER_PAGE: usize = 100;

/// The shortest JSON a diff line can take: an empty addition or deletion,
/// `{"line":"","lineNo":1,"type":"addition"}`. A context line takes more.
const LINE_JSON: u64 = 40;

/// The shortest JSON a diff's file record can take, its paths aside: a move
/// whose blob is unchanged, `{"status":"moved","oldPath":"","newPath":"",
/// "current":{"oid":"<40 hex>","mode":"blob"}}`.
const FILE_JSON: u64 = 119;

/// The shortest JSON a diff hunk can take, its header aside: an empty one,
/// `{"header":"","lines":[],"old":{"start":0,"end":0},"new":{"start":0,"end":0}}`.
const HUNK_JSON: u64 = 76;

/// What an entry `repo_stats_bounded` holds while it counts costs beyond its
/// bytes: its strings' headers and its slot in the set.
const ENTRY_COST: u64 = 64;

/// The shortest JSON a listed repository can take: an empty name and
/// description, and the shortest RID, whose 20 bytes take at least 20
/// base58 digits, `{"rid":"rad:z11111111111111111111","name":"",
/// "description":""}`.
const REPO_JSON: u64 = 62;

/// The COB cache's tables, and their queries for sizes: the same rows, in
/// the same order, as heartwood's `list` and `get` read.
#[derive(Clone, Copy)]
enum CobTable {
    Issues,
    Patches,
}

impl CobTable {
    fn sizes(self) -> &'static str {
        match self {
            Self::Issues => "SELECT id, octet_length(issue) AS size FROM issues WHERE repo = ?1",
            Self::Patches => {
                "SELECT id, octet_length(patch) AS size FROM patches WHERE repo = ?1 ORDER BY id"
            }
        }
    }

    fn size(self) -> &'static str {
        match self {
            Self::Issues => {
                "SELECT octet_length(issue) AS size FROM issues WHERE id = ?1 AND repo = ?2"
            }
            Self::Patches => {
                "SELECT octet_length(patch) AS size FROM patches WHERE id = ?1 AND repo = ?2"
            }
        }
    }
}

/// The deepest a bounded walk of a tree descends (`list_files_bounded`).
/// libgit2's own walk recurses, and a chain of nested trees deep enough
/// overflows the stack; the bounded walk is iterative, and refuses a deeper
/// tree with [`Error::TooDeep`] before it descends. Git's own default for
/// `core.maxTreeDepth` is 4096.
pub const MAX_TREE_DEPTH: usize = 4096;

impl Network {
    fn connected_seed_ids(&mut self) -> Result<HashSet<NodeId>, Error> {
        let seed_ids = self
            .seed_book
            .iter()
            .map(seeds::Candidate::nid)
            .collect::<HashSet<_>>();
        Ok(self
            .handle
            .sessions()?
            .into_iter()
            .filter(|session| seed_ids.contains(&session.nid))
            .filter(|session| matches!(session.state, radicle::node::State::Connected { .. }))
            .map(|session| session.nid)
            .collect())
    }

    /// Dial the device-ranked seed book with bounded concurrency. Each dial
    /// gets at most five seconds; the pass returns as soon as four seed
    /// sessions stand, while already in-flight attempts finish in the
    /// background and Heartwood maintains persistent successes.
    pub fn connect_seed_book(&mut self, timeout: Duration) -> Result<SeedConnectReport, Error> {
        struct DialResult {
            candidate: seeds::Candidate,
            error: Option<String>,
        }

        let started = Instant::now();
        let deadline = started + timeout;
        let target = seeds::READY_TARGET.min(self.seed_book.len());
        let sessions = self.handle.sessions()?;
        let seed_ids = self
            .seed_book
            .iter()
            .map(seeds::Candidate::nid)
            .collect::<HashSet<_>>();
        let occupied = sessions
            .iter()
            .map(|session| session.nid)
            .collect::<HashSet<_>>();
        let initially_connected = sessions
            .iter()
            .filter(|session| seed_ids.contains(&session.nid))
            .filter(|session| matches!(session.state, radicle::node::State::Connected { .. }))
            .count();
        if initially_connected >= target || target == 0 {
            return Ok(SeedConnectReport {
                attempted: 0,
                connected: initially_connected,
                target,
                target_reached: initially_connected >= target,
                elapsed: started.elapsed(),
                failures: Vec::new(),
            });
        }

        let queue = Arc::new(Mutex::new(
            self.seed_book
                .iter()
                .filter(|candidate| !occupied.contains(&candidate.nid()))
                .cloned()
                .collect::<VecDeque<_>>(),
        ));
        let successes = Arc::new(AtomicUsize::new(initially_connected));
        let attempted = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (send, recv) = mpsc::channel::<DialResult>();

        for worker in 0..seeds::DIAL_CONCURRENCY.min(self.seed_book.len()) {
            let queue = Arc::clone(&queue);
            let successes = Arc::clone(&successes);
            let attempted = Arc::clone(&attempted);
            let stop = Arc::clone(&stop);
            let send = send.clone();
            let mut handle = self.handle.clone();
            std::thread::Builder::new()
                .name(format!("radicle-seed-{worker}"))
                .spawn(move || {
                    while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
                        let candidate = match queue.lock() {
                            Ok(mut queue) => queue.pop_front(),
                            Err(_) => None,
                        };
                        let Some(candidate) = candidate else {
                            break;
                        };
                        attempted.fetch_add(1, Ordering::AcqRel);
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            break;
                        }
                        let opts = ConnectOptions {
                            persistent: true,
                            timeout: remaining.min(Duration::from_secs(5)),
                        };
                        let result = handle.connect(
                            candidate.address.id,
                            candidate.address.addr.clone(),
                            opts,
                        );
                        let error = match result {
                            Ok(ConnectResult::Connected) => {
                                let ready = successes.fetch_add(1, Ordering::AcqRel) + 1;
                                if ready >= target {
                                    stop.store(true, Ordering::Release);
                                }
                                None
                            }
                            Ok(ConnectResult::Disconnected { reason }) => Some(reason),
                            Err(err) => Some(err.to_string()),
                        };
                        if send.send(DialResult { candidate, error }).is_err() {
                            break;
                        }
                    }
                })?;
        }
        drop(send);

        let mut failures = Vec::new();
        while successes.load(Ordering::Acquire) < target {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match recv.recv_timeout(remaining) {
                Ok(DialResult {
                    candidate,
                    error: Some(reason),
                }) => failures.push(SeedConnectFailure {
                    nid: candidate.nid().to_string(),
                    addr: candidate.address.addr.to_string(),
                    reason,
                }),
                Ok(DialResult { error: None, .. }) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        stop.store(true, Ordering::Release);

        let connected = successes.load(Ordering::Acquire);
        Ok(SeedConnectReport {
            attempted: attempted.load(Ordering::Acquire),
            connected,
            target,
            target_reached: connected >= target,
            elapsed: started.elapsed(),
            failures,
        })
    }

    /// Compatibility entry point returning only the connected seed count.
    pub fn connect_preferred_seeds(&mut self, timeout: Duration) -> Result<usize, Error> {
        Ok(self.connect_seed_book(timeout)?.connected)
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
        seed(&self.repositories, &mut self.handle, rid)?;

        // Try nodes that are known to seed this repo, preferring connected ones.
        let seeds = self.handle.seeds_for(rid, [*self.profile.did()])?;
        let (connected, disconnected) = seeds.partition();
        let mut candidates: Vec<_> = connected.into_iter().map(|seed| seed.nid).collect();

        // A fresh node has an empty routing table until gossip arrives. Fall
        // back to the effective seed book, but only to sessions that are
        // actually connected; an undialed seed would otherwise consume the
        // clone's retry budget while reporting "not connected".
        let connected_sessions = self.connected_seed_ids()?;
        for seed in self.seed_book.iter() {
            if connected_sessions.contains(&seed.nid()) && !candidates.contains(&seed.nid()) {
                candidates.push(seed.nid());
            }
        }
        // Known-but-disconnected repository seeders remain useful as a last
        // resort, after every live candidate. This avoids spending the whole
        // handshake retry budget before trying an already-connected seed.
        for seed in disconnected {
            if !candidates.contains(&seed.nid) {
                candidates.push(seed.nid);
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
            self.seed_book
                .iter()
                .find(|seed| seed.nid() == *nid)
                .map(|seed| seed.address.addr.to_string())
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

/// The profile at `home` for a node running as the host's key
/// `public_key`: [`Profile::init`] without writing a key pair, or
/// [`Profile::load_from`] without reading one. `keys/` is not touched.
fn host_profile(home: Home, alias: &str, public_key: PublicKey) -> Result<Profile, Error> {
    use radicle::profile::Config;
    use radicle::storage::git::transport;

    let config_path = home.config();
    let config = if config_path.exists() {
        Config::load(config_path.as_path()).map_err(radicle::profile::Error::from)?
    } else {
        Config::init(alias.parse::<Alias>()?, config_path.as_path())
            .map_err(radicle::profile::Error::from)?
    };
    let storage = radicle::Storage::open(
        home.storage(),
        radicle::git::UserInfo {
            alias: config.alias().clone(),
            key: public_key,
        },
    )?;
    // What `Profile::init` creates for a fresh home; opening them again is harmless.
    home.policies_mut()?;
    home.notifications_mut()
        .map_err(radicle::profile::Error::from)?;
    home.cobs_db_mut()?
        .migrate(radicle::cob::migrate::ignore)
        .map_err(radicle::profile::Error::from)?;
    transport::local::register(storage.clone());

    Ok(Profile {
        keystore: Keystore::new(&home.keys()),
        home,
        storage,
        public_key,
        config,
    })
}

/// A node identity the host holds and hands in at start (see
/// [`Embedded::start_with_key`]): the 32-byte Ed25519 secret seed, as
/// RFC 8032 names it. Zeroed on drop, and never printed.
pub struct HostKey(zeroize::Zeroizing<[u8; 32]>);

impl HostKey {
    /// Takes the 32-byte seed out of `bytes`, zeroing `bytes` either way.
    /// `None` if it isn't 32 bytes long.
    pub fn from_bytes(bytes: &mut [u8]) -> Option<Self> {
        use zeroize::Zeroize as _;
        // Copy straight into the zeroizing buffer, never through a bare
        // stack array that would outlive the call unwiped.
        let key = (bytes.len() == 32).then(|| {
            let mut seed = zeroize::Zeroizing::new([0u8; 32]);
            seed.copy_from_slice(bytes);
            Self(seed)
        });
        bytes.zeroize();
        key
    }
}

impl std::fmt::Debug for HostKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostKey(..)")
    }
}

/// An embedded Radicle stack: profile + in-process node.
pub struct Embedded {
    profile: Profile,
    /// The signing key the host handed in ([`Embedded::start_with_key`]),
    /// kept in memory only; `None` when the profile's own key file signs.
    host_signer: Option<radicle::crypto::SigningKey>,
    handle: NodeHandle,
    seed_book: Arc<Vec<seeds::Candidate>>,
    node_thread: Option<JoinHandle<Result<(), radicle_node::runtime::Error>>>,
    /// Asks the node's reactor to stop, through its controller: no socket, so
    /// nothing to wait on. [`Embedded::shutdown_within`] and `Drop` stop the
    /// node with it.
    stop: Box<dyn Fn() + Send + Sync>,
    /// Keeps the node's signal channel open for the runtime's lifetime.
    _signals: mpsc::Sender<radicle_signals::Signal>,
    /// Whether a stop call of this `Embedded` has asked the node to stop
    /// ([`Shutdown::asked`]).
    asked: bool,
    /// Whether the node's home has been seen let go of since its thread
    /// ended ([`Embedded::shutdown`]).
    released: bool,
    /// How the node's thread ended: [`ThreadEnd::Running`] until it is joined,
    /// and kept from then on, for every stop call to report
    /// ([`Embedded::shutdown_within`]).
    end: ThreadEnd,
    /// The node's repository gate ([`Options::repositories`]): the host's,
    /// or one that admits everything.
    repositories: radicle::node::gate::RepoGate,
}

impl Embedded {
    /// Initialize (or load) the profile at `opts.home` and start the node
    /// runtime on a background thread.
    ///
    /// The identity key is stored without a passphrase; key custody is the
    /// host's concern (OS keychain on iOS/desktop) and layered on later.
    ///
    /// libgit2's configuration search paths stay as the process has them. A
    /// host that wants them empty calls [`isolate_git_config`] first, under
    /// its `# Safety` contract.
    pub fn start(opts: Options) -> Result<Self, Error> {
        let home = Home::new(opts.home.clone())?;
        let profile = if home.keys().join("radicle").exists() {
            Profile::load_from(home.clone())?
        } else {
            let seed = radicle::profile::env::seed().unwrap_or_else(|| {
                use radicle::crypto::Seed;
                let mut seed = [0; Seed::BYTES];
                getrandom::fill(&mut seed).expect("failed to get OS randomness");
                Seed::new(seed)
            });
            if home.config().exists() {
                // The home was created by `start_with_key`: it has a config
                // (perhaps customized since) and databases, just no key file
                // of its own. Add the key and keep the rest — `Profile::init`
                // would overwrite config.json with defaults.
                Keystore::new(&home.keys())
                    .init("radicle", None, seed)
                    .map_err(radicle::profile::Error::from)?;
                Profile::load_from(home.clone())?
            } else {
                Profile::init(home.clone(), opts.alias.parse::<Alias>()?, None, seed)?
            }
        };
        // In-process signer: load the (unencrypted) secret key.
        let keystore = Keystore::from_secret_path(&profile.home.keys().join("radicle"));
        let signer = keystore
            .secret_key(None)?
            .ok_or_else(|| Error::NodeThread("secret key not found in keystore".into()))?;
        Self::run(profile, signer, None, opts)
    }

    /// Like [`Embedded::start`], but the node runs as `key`, an identity
    /// the host keeps itself (Freedom derives it from the wallet's
    /// recovery phrase and keeps it sealed) instead of the profile's own
    /// key file.
    ///
    /// Nothing secret is written: the profile at `opts.home` (created if
    /// there is none, with the node's config and databases) only gets the
    /// public key, in memory, and every signature — the node's and the
    /// issue/patch writes' — uses `key` from memory. A key file already in
    /// `keys/` (the device's own identity, from an earlier [`Embedded::start`])
    /// is left untouched, so starting without a host key later brings that
    /// identity back. Storage, seeding policies and the address book are
    /// shared by both identities: a repository seeded under one stays
    /// seeded under the other.
    pub fn start_with_key(opts: Options, key: HostKey) -> Result<Self, Error> {
        let home = Home::new(opts.home.clone())?;
        // Expand the seed by reference: `Seed::new` takes the array by value,
        // which would leave a bare copy of it on the stack. dalek's key
        // zeroizes itself on drop.
        let signer =
            radicle::crypto::SigningKey::from(ed25519_dalek::SigningKey::from_bytes(&key.0));
        drop(key);
        let public_key = *AsRef::<radicle::crypto::PublicKey>::as_ref(&signer);
        let profile = host_profile(home, &opts.alias, public_key)?;
        Self::run(profile, signer.clone(), Some(signer), opts)
    }

    fn run(
        mut profile: Profile,
        signer: radicle::crypto::SigningKey,
        host_signer: Option<radicle::crypto::SigningKey>,
        opts: Options,
    ) -> Result<Self, Error> {
        let seed_book = Arc::new(seeds::effective(&profile)?);

        let socket = if opts.control_socket {
            let socket = profile.home.socket_from_env();
            Some(socket)
        } else {
            None
        };

        let (sig_tx, sig_rx) = mpsc::channel();
        let (mut config, mut gate) = match &opts.routing {
            Some(routing) => (routing.apply(&profile.config.node), routing.gate()),
            None => (
                profile.config.node.clone(),
                Arc::new(radicle::node::dial::Open) as Arc<dyn radicle::node::dial::Gate>,
            ),
        };
        let mut listen = opts.listen.clone();
        if opts.isolated {
            config.peers = radicle::node::config::PeerConfig::Static;
            config.connect = Default::default();
            // No route that opens a socket without asking the gate: heartwood
            // connects to an onion or I2P proxy, and to the global proxy for a
            // forwarded name, without asking it.
            config.proxy = None;
            config.onion = radicle::node::config::AddressConfig::Drop;
            config.i2p = radicle::node::config::AddressConfig::Drop;
            config.external_addresses = vec![];
            gate = Arc::new(Isolated);
            listen.clear();
        }
        let repositories = opts.repositories.clone().unwrap_or_else(repositories);
        // Last, over whatever the profile loaded, the legacy fields' override
        // included. On the profile too, so that this crate's own policy reads
        // (`list_repos`, among others) agree with the node's.
        if let Some(policy) = opts.seeding_policy {
            config.seeding_policy = policy;
            profile.config.node.seeding_policy = policy;
        }
        let runtime = Runtime::init(
            profile.home.clone(),
            config,
            socket,
            listen,
            sig_rx,
            signer,
            gate,
            repositories.clone(),
        )?;
        if !opts.isolated && !seed_book.is_empty() {
            seeds::install(&profile)?;
        }
        let handle = runtime.handle.clone();
        let controller = runtime.controller();
        let stop = Box::new(move || {
            // An error means the reactor has stopped already.
            controller.clone().shutdown().ok();
        });
        let node_thread = std::thread::Builder::new()
            .name("radicle-node".to_string())
            .spawn(move || runtime.run())?;

        Ok(Self {
            profile,
            host_signer,
            handle,
            seed_book,
            node_thread: Some(node_thread),
            stop,
            _signals: sig_tx,
            asked: false,
            released: false,
            end: ThreadEnd::Running,
            repositories,
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
            seed_book: Arc::clone(&self.seed_book),
            repositories: self.repositories.clone(),
        }
    }

    /// Connect to the effective seed book and return detailed diagnostics.
    pub fn connect_seed_book(&self, timeout: Duration) -> Result<SeedConnectReport, Error> {
        self.network().connect_seed_book(timeout)
    }

    /// Compatibility entry point returning only the connected seed count.
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
        self.list_repos_bounded(&Bounds::unbounded())
    }

    /// [`Embedded::list_repos`], within `bounds`: each repository is charged
    /// as [`json::repos`] writes it.
    ///
    /// Storage is walked one entry at a time, and nothing of a repository is
    /// loaded before the token is checked for it, and the budget has room
    /// for it at its shortest, [`REPO_JSON`] and a comma. A repository that
    /// isn't seeded is skipped before it is opened; a seeded one is refused
    /// unopened once there is no room for it, even one that would then have
    /// been skipped as invalid.
    pub fn list_repos_bounded(&self, bounds: &Bounds) -> Result<Vec<RepoInfo>, Error> {
        let mut meter = Meter::array(bounds)?;
        self.list_repos_metered(&mut meter)
    }

    fn list_repos_metered(&self, meter: &mut Meter) -> Result<Vec<RepoInfo>, Error> {
        let loading = meter.bounds().loading();
        let read = RefCell::new(Meter::new(&loading));
        let policies = self.profile.policies()?;
        let mut repos = Vec::new();
        let entries = std::fs::read_dir(self.profile.storage.path())?;
        #[cfg(test)]
        let entries = entries.inspect(|_| seams::bump(&seams::ENTRIES));
        for entry in entries {
            meter.check()?;
            let Some(rid) = stored_repository(&entry?)? else {
                continue;
            };
            if !policies.is_seeding(&rid)? {
                continue;
            }
            meter.admits(REPO_JSON)?;
            let Some(identity) = self.is_listed(rid, &read)? else {
                continue;
            };
            let info = self.repo_info_with(rid, identity, &mut read.borrow_mut())?;
            meter.item(|w| json::repo(w, &info))?;
            repos.push(info);
        }
        Ok(repos)
    }

    /// Whether heartwood's `Storage::repositories` lists `rid`, checked as it
    /// checks each repository: one that doesn't open with an identity
    /// document and a head is skipped; one whose local signed refs don't
    /// read is an error, the error it returns. Signed refs that are absent,
    /// or that need migrating, are accepted, as it accepts them.
    ///
    /// Within a loads budget, `read` sizes first what those checks load
    /// (review of libradicle #15): the identity document, and the local
    /// signed refs. One with no room is `TooLarge`; one whose identity can't
    /// be sized up front (no `refs/rad/id`) is skipped as invalid.
    ///
    /// Listed, it answers the identity head it sized, for the read of the
    /// repository's details to use (`None` for an unbounded listing).
    fn is_listed(
        &self,
        rid: RepoId,
        read: &RefCell<Meter>,
    ) -> Result<Option<Option<git2::Oid>>, Error> {
        use radicle::storage::refs::{sigrefs::read::error::Read, FeatureLevel, SignedRefs};

        #[cfg(test)]
        seams::bump(&seams::OPENED);
        let repo = match self.profile.storage.repository(rid) {
            Ok(repo) => repo,
            Err(e) => {
                log::warn!(target: "libradicle", "Repository {rid} is invalid: {e}");
                return Ok(None);
            }
        };
        let local = self.profile.storage.info().key;
        let bounded = read.borrow().counts();
        let objects = Objects::new(&repo.backend, read.borrow().bounds().budget)?;
        let identity = if bounded {
            match Self::identity_fits(&repo, &objects, &mut read.borrow_mut()) {
                Ok(head) => Some(head),
                Err(e @ (Error::TooLarge { .. } | Error::Cancelled)) => return Err(e),
                Err(e) => {
                    log::warn!(target: "libradicle", "Repository {rid} is invalid: sizing its doc: {e}");
                    return Ok(None);
                }
            }
        } else {
            None
        };
        if let Err(e) = Self::identity_doc(&repo, identity) {
            log::warn!(target: "libradicle", "Repository {rid} is invalid: looking up doc: {e}");
            return Ok(None);
        }
        let head = Self::head_bounded(&repo, &objects, &mut read.borrow_mut(), identity);
        match head {
            Ok(_) => {}
            Err(e @ (Error::TooLarge { .. } | Error::Cancelled)) => return Err(e),
            Err(e) => {
                log::warn!(target: "libradicle", "Repository {rid} is invalid: looking up head: {e}");
                return Ok(None);
            }
        }
        // heartwood's `SignedRefsInfo::new`, private to it, and the sync time
        // it then reads for signed refs that need no migration. Within a
        // loads budget, read through the metered reader, pinned to its head.
        let loaded = if bounded {
            Self::signed_refs_at(&repo, &local, &objects, read)?
        } else {
            SignedRefs::load(local, &repo)
        };
        match loaded {
            Ok(Some(refs))
                if refs.feature_level() >= FeatureLevel::LATEST && refs.parent().is_some() =>
            {
                radicle::node::SyncedAt::new(refs.at, &repo)
                    .map_err(radicle::storage::Error::from)?;
            }
            Ok(_) | Err(Read::Downgrade { .. }) => {}
            Err(err) => {
                let err = radicle::storage::refs::Error::Read(err);
                return Err(radicle::storage::Error::from(err).into());
            }
        }
        Ok(Some(identity))
    }

    /// List explicit allow-seeding policies, including repositories whose
    /// first fetch has not succeeded yet.
    pub fn list_seeded_repos(&self) -> Result<Vec<SeededRepoInfo>, Error> {
        self.list_seeded_repos_bounded(&Bounds::unbounded())
    }

    /// [`Embedded::list_seeded_repos`], within `bounds`: the stored
    /// repositories are listed as [`Embedded::list_repos_bounded`] lists
    /// them, and each policy without one is charged as
    /// [`json::seeded_repos`] writes it.
    pub fn list_seeded_repos_bounded(&self, bounds: &Bounds) -> Result<Vec<SeededRepoInfo>, Error> {
        let mut meter = Meter::array(bounds)?;
        let policies = self.profile.policies()?;
        let mut repos = self
            .list_repos_metered(&mut meter)?
            .into_iter()
            .map(|repo| {
                (
                    repo.rid,
                    SeededRepoInfo {
                        rid: repo.rid,
                        name: Some(repo.name),
                        description: Some(repo.description),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        for policy in policies.seed_policies()? {
            meter.check()?;
            let policy = policy?;
            if !policy.policy.is_allow() || repos.contains_key(&policy.rid) {
                continue;
            }
            let info = SeededRepoInfo {
                rid: policy.rid,
                name: None,
                description: None,
            };
            meter.item(|w| json::seeded_repo(w, &info))?;
            repos.insert(policy.rid, info);
        }
        Ok(repos.into_values().collect())
    }

    fn signer(&self) -> Result<Signer, Error> {
        match &self.host_signer {
            Some(key) => Ok(Signer::Key(key.clone())),
            None => Ok(self.profile.signer()?),
        }
    }

    fn announce_refs(&self, rid: RepoId) -> Result<(), Error> {
        let mut handle = self.handle.clone();
        handle
            .announce_refs_for(rid, [*self.profile.did()])
            .map(|_| ())
            .map_err(|err| Error::Announce(err.to_string()))
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
        seed(&self.repositories, &mut self.handle, rid)?;
        self.handle.add_inventory(rid)?;
        self.announce_refs(rid)?;
        Ok(rid)
    }

    /// Read all issues in repository storage.
    pub fn issues(&self, rid: RepoId) -> Result<Vec<(cob::ObjectId, cob::issue::Issue)>, Error> {
        self.issues_bounded(rid, &Bounds::unbounded())
    }

    /// [`Embedded::issues`], within `bounds`: each issue is charged as
    /// [`json::issues`] writes it.
    ///
    /// Each cached issue's size is read first, without loading it (review
    /// of libradicle #15), and counted against the loads budget: one it
    /// has no room left for is refused before it is deserialized.
    pub fn issues_bounded(
        &self,
        rid: RepoId,
        bounds: &Bounds,
    ) -> Result<Vec<(cob::ObjectId, cob::issue::Issue)>, Error> {
        let mut meter = Meter::array(bounds)?;
        let loading = bounds.loading();
        let mut read = Meter::new(&loading);
        let repo = self.profile.storage.repository(rid)?;
        let issues = self.profile.issues(&repo)?;
        let mut result = Vec::new();
        self.each_cob(CobTable::Issues, rid, |id, size| {
            meter.check()?;
            read.charge(size)?;
            // Gone since it was sized: as if it had not been listed.
            if let Some(issue) = issues.get(&id)? {
                meter.item(|w| json::issue(w, &id, &issue))?;
                result.push((id, issue));
            }
            Ok(())
        })?;
        Ok(result)
    }

    /// Read one issue from repository storage.
    pub fn issue(
        &self,
        rid: RepoId,
        issue_id: &str,
    ) -> Result<(cob::ObjectId, cob::issue::Issue), Error> {
        self.issue_bounded(rid, issue_id, &Bounds::unbounded())
    }

    /// [`Embedded::issue`], within `bounds`: its cached size is checked
    /// against the loads budget before it is loaded, and the result charged
    /// as [`json::issue`] writes it.
    pub fn issue_bounded(
        &self,
        rid: RepoId,
        issue_id: &str,
        bounds: &Bounds,
    ) -> Result<(cob::ObjectId, cob::issue::Issue), Error> {
        let not_found = || Error::NodeThread(format!("issue {issue_id} not found"));
        let mut meter = Meter::new(bounds);
        let repo = self.profile.storage.repository(rid)?;
        let id = Self::cob_id(&repo, issue_id)?.ok_or_else(not_found)?;
        let size = self
            .cob_size(CobTable::Issues, rid, &id)?
            .ok_or_else(not_found)?;
        Meter::new(&bounds.loading()).charge(size)?;
        let issue = self
            .profile
            .issues(&repo)?
            .get(&id)?
            .ok_or_else(not_found)?;
        meter.whole(|w| json::issue(w, &id, &issue))?;
        Ok((id, issue))
    }

    /// Read all patches in repository storage.
    pub fn patches(&self, rid: RepoId) -> Result<Vec<(cob::ObjectId, cob::patch::Patch)>, Error> {
        self.patches_bounded(rid, &Bounds::unbounded())
    }

    /// [`Embedded::patches`], within `bounds`: each patch is charged as
    /// [`json::patches`] writes it, and sized first, as
    /// [`Embedded::issues_bounded`] sizes issues.
    pub fn patches_bounded(
        &self,
        rid: RepoId,
        bounds: &Bounds,
    ) -> Result<Vec<(cob::ObjectId, cob::patch::Patch)>, Error> {
        let mut meter = Meter::array(bounds)?;
        let loading = bounds.loading();
        let mut read = Meter::new(&loading);
        let repo = self.profile.storage.repository(rid)?;
        let patches = self.profile.patches(&repo)?;
        let mut result = Vec::new();
        self.each_cob(CobTable::Patches, rid, |id, size| {
            meter.check()?;
            read.charge(size)?;
            if let Some(patch) = patches.get(&id)? {
                meter.item(|w| json::patch(w, &id, &patch))?;
                result.push((id, patch));
            }
            Ok(())
        })?;
        Ok(result)
    }

    /// Read one patch from repository storage.
    pub fn patch(
        &self,
        rid: RepoId,
        patch_id: &str,
    ) -> Result<(cob::ObjectId, cob::patch::Patch), Error> {
        self.patch_bounded(rid, patch_id, &Bounds::unbounded())
    }

    /// [`Embedded::patch`], within `bounds`, as [`Embedded::issue_bounded`].
    pub fn patch_bounded(
        &self,
        rid: RepoId,
        patch_id: &str,
        bounds: &Bounds,
    ) -> Result<(cob::ObjectId, cob::patch::Patch), Error> {
        let not_found = || Error::NodeThread(format!("patch {patch_id} not found"));
        let mut meter = Meter::new(bounds);
        let repo = self.profile.storage.repository(rid)?;
        let id = Self::cob_id(&repo, patch_id)?.ok_or_else(not_found)?;
        let size = self
            .cob_size(CobTable::Patches, rid, &id)?
            .ok_or_else(not_found)?;
        Meter::new(&bounds.loading()).charge(size)?;
        let patch = self
            .profile
            .patches(&repo)?
            .get(&id)?
            .ok_or_else(not_found)?;
        meter.whole(|w| json::patch(w, &id, &patch))?;
        Ok((id, patch))
    }

    /// The COB `id` names, a full id or a unique prefix, resolved without
    /// loading the object; `None` when nothing matches.
    fn cob_id(
        repo: &radicle::storage::git::Repository,
        id: &str,
    ) -> Result<Option<cob::ObjectId>, Error> {
        validate_cob_id(id)?;
        let short = git2::Oid::from_str(id)?;
        match repo.backend.odb()?.exists_prefix(short, id.len()) {
            Ok(oid) => Ok(Some(oid.into())),
            Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// The COB cache, read-only, as heartwood opens it.
    fn cob_db(&self) -> Result<sqlite::Connection, Error> {
        let path = self.profile.cobs().join(cob::cache::COBS_DB_FILE);
        let mut db =
            sqlite::Connection::open_with_flags(path, sqlite::OpenFlags::new().with_read_only())?;
        db.set_busy_timeout(3000)?;
        Ok(db)
    }

    /// Calls `each` with every cached COB of `rid` in `table`, in the order
    /// heartwood's cache lists them, and its size in bytes: SQLite's
    /// `octet_length` reads a value's length from its record, so no COB is
    /// loaded, and the rows are read one at a time.
    fn each_cob(
        &self,
        table: CobTable,
        rid: RepoId,
        mut each: impl FnMut(cob::ObjectId, u64) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let db = self.cob_db()?;
        let mut stmt = db.prepare(table.sizes())?;
        stmt.bind((1, &rid))?;
        for row in stmt.into_iter() {
            let row = row?;
            let id = row.try_read::<&str, _>("id")?;
            let id = id
                .parse::<cob::ObjectId>()
                .map_err(|e| Error::NodeThread(format!("cached COB id {id:?}: {e}")))?;
            each(id, row.try_read::<i64, _>("size")?.max(0) as u64)?;
        }
        Ok(())
    }

    /// The size in bytes of the cached COB `id` of `rid` in `table`.
    fn cob_size(
        &self,
        table: CobTable,
        rid: RepoId,
        id: &cob::ObjectId,
    ) -> Result<Option<u64>, Error> {
        let db = self.cob_db()?;
        let mut stmt = db.prepare(table.size())?;
        stmt.bind((1, id.to_string().as_str()))?;
        stmt.bind((2, &rid))?;
        let size = match stmt.into_iter().next().transpose()? {
            None => None,
            Some(row) => Some(row.try_read::<i64, _>("size")?.max(0) as u64),
        };
        Ok(size)
    }

    /// [`Embedded::repo_info`], within `bounds`: the result is charged as
    /// [`json::repo_info`] writes it.
    pub fn repo_info_bounded(&self, rid: RepoId, bounds: &Bounds) -> Result<RepoInfo, Error> {
        let loading = bounds.loading();
        let mut read = Meter::new(&loading);
        let identity = if read.counts() {
            let repo = self.profile.storage.repository(rid)?;
            let objects = Objects::new(&repo.backend, bounds.loads)?;
            Some(Self::identity_fits(&repo, &objects, &mut read)?)
        } else {
            None
        };
        let info = self.repo_info_with(rid, identity, &mut read)?;
        Meter::new(bounds).whole(|w| json::repo_info(w, &info))?;
        Ok(info)
    }

    /// Read a repository's identity payload + head from local storage.
    pub fn repo_info(&self, rid: RepoId) -> Result<RepoInfo, Error> {
        let unbounded = Bounds::unbounded();
        self.repo_info_with(rid, None, &mut Meter::new(&unbounded))
    }

    /// [`Embedded::repo_info`], its identity document read at `identity`
    /// when a bounded read sized one there, and its head resolved within
    /// `read` ([`Embedded::head_bounded`]).
    fn repo_info_with(
        &self,
        rid: RepoId,
        identity: Option<git2::Oid>,
        read: &mut Meter,
    ) -> Result<RepoInfo, Error> {
        let repo = self.profile.storage.repository(rid)?;
        let objects = Objects::new(&repo.backend, read.bounds().budget)?;
        let doc = Self::identity_doc(&repo, identity)?;
        let proj = doc.project()?;
        let head = Self::head_bounded(&repo, &objects, read, identity)?;

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
        self.list_files_bounded(rid, &Bounds::unbounded())
    }

    /// [`Embedded::list_files`], within `bounds`: each path is charged as
    /// [`json::files`] writes it.
    ///
    /// The walk is depth-first and pre-order, as libgit2's `git_tree_walk`
    /// is, but iterative (review of libradicle #15): a bounded walk refuses
    /// to descend past [`MAX_TREE_DEPTH`] with [`Error::TooDeep`], and
    /// loads each tree only after its header, which the budget must have
    /// room for, counting every tree it has loaded so far: trees reached
    /// more than once are counted each time. A name that isn't UTF-8 reads
    /// `?`, a directory's too.
    pub fn list_files_bounded(&self, rid: RepoId, bounds: &Bounds) -> Result<Vec<String>, Error> {
        let mut meter = Meter::array(bounds)?;
        let loading = bounds.loading();
        let mut read = Meter::new(&loading);
        let bounded = meter.counts() || read.counts();
        let repo = self.profile.storage.repository(rid)?;
        let objects = Objects::new(&repo.backend, bounds.loads)?;
        let head = Self::head_bounded(&repo, &objects, &mut read, None)?;
        let commit = objects.commit(head, &mut read)?;
        let root = objects.tree(commit.tree_id(), &mut read)?;
        drop(commit);

        let mut files = Vec::new();
        // Each tree being walked, the index of its next entry, and the length
        // of `prefix` before its own name was added.
        let mut stack = vec![(root, 0usize, 0usize)];
        let mut prefix = String::new();
        loop {
            meter.check()?;
            let depth = stack.len();
            let Some((tree, index, mark)) = stack.last_mut() else {
                break;
            };
            let descend = match tree.get(*index) {
                None => {
                    prefix.truncate(*mark);
                    None
                }
                Some(entry) => {
                    *index += 1;
                    let name = entry.name().unwrap_or("?");
                    match entry.kind() {
                        Some(git2::ObjectType::Blob) => {
                            let path = format!("{prefix}{name}");
                            meter.item(|w| serde_json::to_writer(w, &path))?;
                            files.push(path);
                            continue;
                        }
                        Some(git2::ObjectType::Tree) => {
                            if bounded && depth > MAX_TREE_DEPTH {
                                return Err(Error::TooDeep {
                                    limit: MAX_TREE_DEPTH,
                                });
                            }
                            let mark = prefix.len();
                            prefix.push_str(name);
                            prefix.push('/');
                            Some((entry.id(), mark))
                        }
                        // A submodule: listed by nobody, as libgit2's walk
                        // doesn't descend into it either.
                        _ => continue,
                    }
                }
            };
            match descend {
                Some((id, mark)) => {
                    let subtree = objects.tree(id, &mut read)?;
                    stack.push((subtree, 0, mark));
                }
                None => {
                    stack.pop();
                }
            }
        }
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

    /// What heartwood's `identity_doc()` will load, sized first (review of
    /// libradicle #15): the document blob, `embeds/radicle.json` in the
    /// tree of the commit `refs/rad/id` names, and the commit and trees on
    /// the way, each checked and charged through `objects`. Without
    /// `refs/rad/id`, heartwood would search every remote's identity
    /// history instead, which can't be sized before it is read: refused
    /// with [`Error::NoIdentityHead`].
    ///
    /// Returns the head it sized: the caller reads the document there
    /// (`identity_doc_at`), never through `refs/rad/id` again, so a fetch
    /// moving the ref meanwhile changes only the next read. Objects are named
    /// by their content, so the head pins everything under it.
    fn identity_fits(
        repo: &radicle::storage::git::Repository,
        objects: &Objects,
        read: &mut Meter,
    ) -> Result<git2::Oid, Error> {
        let head = match repo.backend.refname_to_id("refs/rad/id") {
            Ok(head) => head,
            Err(e) if e.code() == git2::ErrorCode::NotFound => {
                return Err(Error::NoIdentityHead(repo.id))
            }
            Err(e) => return Err(e.into()),
        };
        let commit = objects.commit(head, read)?;
        Self::embedded_doc_fits(objects, &commit, read)?;
        Ok(head)
    }

    /// The head heartwood's `ReadRepository::head()` answers, within a loads
    /// budget (review of libradicle #15, rounds 4 and 5):
    /// - the local `HEAD`, when it resolves;
    /// - otherwise the canonical head of the default branch, computed from
    ///   the identity document at `identity` (sized there by
    ///   [`Embedded::identity_fits`] on `read` unless the caller already
    ///   did), never through `repo.head()` or `identity_doc()`. heartwood's
    ///   own canonical-ref rules and quorum compute it, over
    ///   [`objects::Tips`]: each delegate's tip is looked up once, sized and
    ///   charged, and only those pinned tips are used.
    ///
    /// Tips that disagree in any way are refused with
    /// [`Error::NoCanonicalHead`]: different commits would need a merge-base
    /// walk, unbounded and uncancellable, and tags or a commit and a tag
    /// are refused alike. Such a repository can't be browsed through a
    /// bounded read until its `HEAD` is fixed or its delegates agree.
    ///
    /// Unbounded (`read` counts nothing), it is heartwood's `head()`.
    fn head_bounded(
        repo: &radicle::storage::git::Repository,
        objects: &Objects,
        read: &mut Meter,
        identity: Option<git2::Oid>,
    ) -> Result<git2::Oid, Error> {
        if !read.counts() {
            return Ok(repo.head()?.1.into());
        }
        if let Ok(head) = repo.backend.head() {
            if let Ok((_, oid)) = radicle::git::refs::qualified_from(&head) {
                return Ok(oid.into());
            }
        }
        let identity = match identity {
            Some(identity) => identity,
            None => Self::identity_fits(repo, objects, read)?,
        };
        let doc = repo.identity_doc_at(identity.into())?;
        // The tips adapter charges the caller's meter through a cell, and
        // hands it back after.
        let placeholder = Meter::new(read.bounds());
        let cell = RefCell::new(std::mem::replace(read, placeholder));
        let tips = objects::Tips::new(repo, objects, &cell);
        let quorum = (|| -> Result<_, radicle::storage::RepositoryError> {
            Ok(doc
                .canonical_refs()?
                .rules()
                .canonical(doc.default_branch()?, &tips)
                .ok_or(radicle::storage::RepositoryError::MissingBranchRule)?
                .find_objects()?
                .quorum()?)
        })();
        let (refused, diverged) = (tips.refused(), tips.diverged());
        drop(tips);
        *read = cell.into_inner();
        if let Some(refused) = refused {
            return Err(refused);
        }
        if diverged {
            return Err(Error::NoCanonicalHead(repo.id));
        }
        Ok(quorum?.object.id().into())
    }

    /// The identity document: at `pinned`, a head [`Embedded::identity_fits`]
    /// sized, or, for an unbounded read, wherever heartwood finds it.
    fn identity_doc(
        repo: &radicle::storage::git::Repository,
        pinned: Option<git2::Oid>,
    ) -> Result<radicle::identity::DocAt, Error> {
        Ok(match pinned {
            Some(head) => repo.identity_doc_at(head.into())?,
            None => repo.identity_doc()?,
        })
    }

    /// The identity document embedded in `commit`'s tree, at
    /// `embeds/radicle.json`, sized and charged: what heartwood's
    /// `Doc::blob_at` loads. Missing, it is heartwood's to report.
    fn embedded_doc_fits(
        objects: &Objects,
        commit: &git2::Commit<'_>,
        read: &mut Meter,
    ) -> Result<(), Error> {
        let root = objects.tree(commit.tree_id(), read)?;
        let Some(embeds) = root.get_name("embeds") else {
            return Ok(());
        };
        if embeds.kind() != Some(git2::ObjectType::Tree) {
            return Ok(());
        }
        let embeds = objects.tree(embeds.id(), read)?;
        if let Some(doc) = embeds.get_name("radicle.json") {
            if doc.kind() == Some(git2::ObjectType::Blob) {
                objects.blob(doc.id(), read)?;
            }
        }
        Ok(())
    }

    /// `remote`'s signed refs, read by heartwood's own reader at the head
    /// its `rad/sigrefs` names now, through [`objects::Metered`] (review of
    /// libradicle #15, round 3): every commit and blob heartwood reads,
    /// current or old-format history and the identity-root documents named
    /// along it, is sized and charged to `read` before it is loaded. The
    /// head is resolved once and the read pinned to it (`load_at`).
    ///
    /// `Ok(Ok(None))` when the remote has no signed refs; `Ok(Err(_))` what
    /// heartwood's reader answers; `Err(_)` a refusal of the bounds.
    fn signed_refs_at(
        repo: &radicle::storage::git::Repository,
        remote: &radicle::prelude::PublicKey,
        objects: &Objects,
        read: &RefCell<Meter>,
    ) -> Result<
        Result<
            Option<radicle::storage::refs::SignedRefs>,
            radicle::storage::refs::sigrefs::read::error::Read,
        >,
        Error,
    > {
        let name = format!("refs/namespaces/{remote}/refs/rad/sigrefs");
        let head = match repo.backend.refname_to_id(&name) {
            Ok(head) => head,
            Err(e) if e.code() == git2::ErrorCode::NotFound => return Ok(Ok(None)),
            Err(e) => return Err(e.into()),
        };
        #[cfg(test)]
        seams::resolved();
        let reader = objects::Metered::new(repo, objects, read);
        let loaded = radicle::storage::refs::SignedRefs::load_at(head.into(), *remote, &reader);
        if let Some(refused) = reader.refused() {
            return Err(refused);
        }
        Ok(loaded)
    }

    /// Every signed remote of `repo`, loaded one at a time: within a loads
    /// budget through [`Embedded::signed_refs_at`], otherwise by heartwood's
    /// `remote()`.
    fn remotes_loaded(
        repo: &radicle::storage::git::Repository,
        objects: &Objects,
        read: &RefCell<Meter>,
        mut each: impl FnMut(radicle::prelude::PublicKey, radicle::storage::Remote) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let bounded = read.borrow().counts();
        for id in repo.remote_ids()? {
            read.borrow().check()?;
            let id = id?;
            let remote = if bounded {
                match Self::signed_refs_at(repo, &id, objects, read)? {
                    Ok(Some(refs)) => radicle::storage::Remote::new(refs),
                    Ok(None) => return Err(Error::NodeThread(format!("{id} has no signed refs"))),
                    Err(e) => return Err(radicle::storage::refs::Error::Read(e).into()),
                }
            } else {
                repo.remote(&id)?
            };
            each(id, remote)?;
        }
        Ok(())
    }

    fn surf_repo(&self, rid: RepoId) -> Result<radicle_surf::Repository, Error> {
        let repo = self.profile.storage.repository(rid)?;
        Ok(radicle_surf::Repository::open(repo.backend.path())?)
    }

    /// Paginated commit history reachable from `parent`.
    pub fn commits(
        &self,
        rid: RepoId,
        parent: &str,
        page: usize,
        per_page: usize,
    ) -> Result<Vec<radicle_surf::Commit>, Error> {
        self.commits_bounded(rid, parent, page, per_page, &Bounds::unbounded())
    }

    /// [`Embedded::commits`], within `bounds`: each commit is charged as
    /// [`json::commits`] writes it, and the token is checked at every commit
    /// walked, the skipped pages' included.
    ///
    /// The history is walked as radicle-surf's is (libgit2's revwalk order,
    /// see [`objects::Walk`]), but each commit's size is read from its
    /// header before it is loaded (review of libradicle #15): one over the
    /// budget is refused unloaded, and the commits walked, skipped ones
    /// included, may not add up to more than the budget either. A skipped
    /// commit is never converted, so one that radicle-surf would refuse
    /// (say, a message that isn't UTF-8) doesn't fail a later page.
    pub fn commits_bounded(
        &self,
        rid: RepoId,
        parent: &str,
        page: usize,
        per_page: usize,
        bounds: &Bounds,
    ) -> Result<Vec<radicle_surf::Commit>, Error> {
        let mut meter = Meter::array(bounds)?;
        let loading = bounds.loading();
        let mut read = Meter::new(&loading);
        let oid = parse_revision(parent)?;
        let repo = self.profile.storage.repository(rid)?;
        let objects = Objects::new(&repo.backend, bounds.loads)?;
        let mut walk = Walk::new(&objects, oid, &mut read)?;
        let skip = page.saturating_mul(per_page);
        let mut commits = Vec::new();
        if per_page == 0 {
            return Ok(commits);
        }
        let mut index = 0usize;
        while let Some(oid) = walk.next(&mut read)? {
            meter.check()?;
            index += 1;
            if index <= skip {
                continue;
            }
            let commit = radicle_surf::Commit::try_from(walk.load(oid)?)
                .map_err(radicle_surf::Error::from)?;
            meter.item(|w| serde_json::to_writer(w, &commit))?;
            commits.push(commit);
            if commits.len() == per_page {
                break;
            }
        }
        Ok(commits)
    }

    /// Commit metadata plus a structured diff against its first parent.
    ///
    /// Diff text and paths that are not UTF-8 (a Latin-1 file, a file name
    /// in another encoding) come back decoded lossily, with U+FFFD for each
    /// invalid sequence; see [`utf8_diff`].
    pub fn commit(&self, rid: RepoId, revision: &str) -> Result<CommitDetail, Error> {
        self.commit_bounded(rid, revision, &Bounds::unbounded())
    }

    /// [`Embedded::commit`], within `bounds`: the detail is charged as
    /// [`json::commit`] writes it, its diff as decoded ([`utf8_diff`]).
    ///
    /// The diff is checked before it is built, and is `TooLarge` before
    /// anything of it is built when:
    /// - its file records, a rename or copy counted as one, can't fit at the
    ///   least JSON each takes, with its paths: checked before renames and
    ///   copies are looked for, and again after;
    /// - the blobs it would load, sized from their headers, add up to more
    ///   than the budget, binary files' included;
    /// - its records, hunks and lines can't fit, each at the least JSON it
    ///   takes.
    ///
    /// The token is checked between those steps, and at every file record,
    /// hunk and line; finding renames and copies is one step libgit2 doesn't
    /// interrupt, bounded by the record count checked before it.
    pub fn commit_bounded(
        &self,
        rid: RepoId,
        revision: &str,
        bounds: &Bounds,
    ) -> Result<CommitDetail, Error> {
        let mut meter = Meter::new(bounds);
        let oid = parse_revision(revision)?;
        if meter.counts() {
            let storage = self.profile.storage.repository(rid)?;
            Self::diff_fits(&storage.backend, oid, &meter)?;
        }
        let repo = self.surf_repo(rid)?;
        let commit = repo.commit(radicle_surf::Oid::from(oid))?;
        meter.check()?;
        let diff = utf8_diff(repo.diff_commit(commit.id)?);
        let detail = CommitDetail { commit, diff };
        meter.whole(|w| json::commit(w, &detail))?;
        Ok(detail)
    }

    /// Whether `oid`'s diff against its first parent, as radicle-surf builds
    /// it, can fit what is left of `meter`'s budget, found without building
    /// it: see [`Embedded::commit_bounded`].
    fn diff_fits(repo: &git2::Repository, oid: git2::Oid, meter: &Meter) -> Result<(), Error> {
        use std::cell::{Cell, RefCell};

        let left = meter.left();
        let over = |size: u64| Error::TooLarge { size, limit: left };
        // The commit, its first parent and both root trees, each sized from
        // its header before it is loaded (review of libradicle #15): the
        // commit's message is part of the result.
        // The commit is part of the result: within what is left of the
        // budget too. Its parent and the trees only within the loads budget.
        let commits = Objects::new(repo, left.min(meter.bounds().loads))?;
        let objects = Objects::new(repo, meter.bounds().loads)?;
        let loading = meter.bounds().loading();
        let mut read_all = Meter::new(&loading);
        let commit = commits.commit(oid, &mut read_all)?;
        let new_tree = objects.tree(commit.tree_id(), &mut read_all)?;
        let old_tree = match commit.parent_ids().next() {
            Some(parent) => {
                let parent = objects.commit(parent, &mut read_all)?;
                Some(objects.tree(parent.tree_id(), &mut read_all)?)
            }
            None => None,
        };
        // Every tree the diff can descend into, sized before libgit2 loads
        // it (review of libradicle #15, round 2).
        Self::diff_trees_fit(&objects, old_tree.as_ref(), &new_tree, &mut read_all)?;
        let mut diff = repo.diff_tree_to_tree(old_tree.as_ref(), Some(&new_tree), None)?;

        // Before renames and copies are found, the records can't be fewer than
        // half the deltas: a rename or copy joins two at most.
        let floor = (diff.deltas().len() as u64 / 2).saturating_mul(FILE_JSON);
        if floor > left {
            return Err(over(floor));
        }
        // Every blob the diff will load, sized from its header, all of them
        // together: within what is left of the JSON budget (their lines are
        // the result), and charged to the same loads meter as the commits
        // and trees above, each within the per-object limit (review of
        // libradicle #15, round 3).
        let odb = repo.odb()?;
        let mut loaded = 0u64;
        for delta in diff.deltas() {
            meter.check()?;
            for file in [delta.old_file(), delta.new_file()] {
                if file.id().is_zero() || file.mode() == git2::FileMode::Commit {
                    continue;
                }
                let (size, _) = odb.read_header(file.id())?;
                let size = size as u64;
                if size > meter.bounds().loads {
                    return Err(Error::TooLarge {
                        size,
                        limit: meter.bounds().loads,
                    });
                }
                read_all.charge(size)?;
                loaded = loaded.saturating_add(size);
                if loaded > left {
                    return Err(over(loaded));
                }
            }
        }
        // As radicle-surf finds them: a rename or copy shows only its changes.
        meter.check()?;
        let mut similar = git2::DiffFindOptions::new();
        similar.renames(true);
        similar.copies(true);
        diff.find_similar(Some(&mut similar))?;
        meter.check()?;

        // The records as radicle-surf builds them, each with its paths; then
        // their hunks and lines.
        let needed = Cell::new(0u64);
        let stopped = RefCell::new(None);
        let charge = |bytes: u64| -> bool {
            needed.set(needed.get().saturating_add(bytes));
            let fits = meter.check().and_then(|()| {
                if needed.get() > left {
                    return Err(over(needed.get()));
                }
                Ok(())
            });
            match fits {
                Ok(()) => true,
                Err(e) => {
                    *stopped.borrow_mut() = Some(e);
                    false
                }
            }
        };
        let path_len =
            |file: git2::DiffFile<'_>| file.path().map_or(0, |path| path.as_os_str().len() as u64);
        let walked = diff.foreach(
            &mut |delta, _| {
                let paths = match delta.status() {
                    git2::Delta::Renamed | git2::Delta::Copied => {
                        path_len(delta.old_file()) + path_len(delta.new_file())
                    }
                    git2::Delta::Deleted => path_len(delta.old_file()),
                    _ => path_len(delta.new_file()),
                };
                charge(FILE_JSON + paths)
            },
            None,
            Some(&mut |_, hunk| charge(HUNK_JSON + hunk.header().len() as u64)),
            Some(&mut |_, _, line| {
                if !matches!(line.origin(), '+' | '-' | ' ') {
                    return true;
                }
                charge(LINE_JSON + line.content().len() as u64)
            }),
        );
        if let Some(e) = stopped.into_inner() {
            return Err(e);
        }
        walked?;
        Ok(())
    }

    /// Whether every tree a diff of `old` against `new` can make libgit2
    /// load fits `read`, found without loading any over it: the subtrees on
    /// one side only, and the pairs of subtrees under the same name that
    /// differ, all the way down. A pair that is the same tree on both sides
    /// is skipped, as libgit2's diff skips it. Each tree is sized from its
    /// header and charged before it is loaded (its entries and names are in
    /// those bytes), and a bounded walk refuses to descend past
    /// [`MAX_TREE_DEPTH`].
    fn diff_trees_fit(
        objects: &Objects,
        old: Option<&git2::Tree<'_>>,
        new: &git2::Tree<'_>,
        read: &mut Meter,
    ) -> Result<(), Error> {
        let mut pending = Vec::new();
        Self::differing_subtrees(old, Some(new), 1, &mut pending);
        while let Some((old, new, depth)) = pending.pop() {
            read.check()?;
            if depth > MAX_TREE_DEPTH {
                return Err(Error::TooDeep {
                    limit: MAX_TREE_DEPTH,
                });
            }
            let old = old.map(|id| objects.tree(id, read)).transpose()?;
            let new = new.map(|id| objects.tree(id, read)).transpose()?;
            Self::differing_subtrees(old.as_ref(), new.as_ref(), depth + 1, &mut pending);
        }
        Ok(())
    }

    /// The subtrees of `old` and `new`, paired by name, that a diff of the
    /// two descends into, queued at `depth`.
    fn differing_subtrees(
        old: Option<&git2::Tree<'_>>,
        new: Option<&git2::Tree<'_>>,
        depth: usize,
        pending: &mut Vec<(Option<git2::Oid>, Option<git2::Oid>, usize)>,
    ) {
        let subtrees = |tree: Option<&git2::Tree<'_>>| -> BTreeMap<Vec<u8>, git2::Oid> {
            tree.map(|tree| {
                tree.iter()
                    .filter(|e| e.kind() == Some(git2::ObjectType::Tree))
                    .map(|e| (e.name_bytes().to_vec(), e.id()))
                    .collect()
            })
            .unwrap_or_default()
        };
        let mut olds = subtrees(old);
        for (name, new) in subtrees(new) {
            match olds.remove(&name) {
                Some(old) if old == new => {}
                old => pending.push((old, Some(new), depth)),
            }
        }
        for old in olds.into_values() {
            pending.push((Some(old), None, depth));
        }
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
        self.tree_at_bounded(rid, revision, path, &Bounds::unbounded())
    }

    /// [`Embedded::tree_at`], within `bounds`: the entries are charged as
    /// they are listed, and the whole result as [`json::tree_at`] writes
    /// it. The commit and every tree on the way to `path` are sized from
    /// their headers before they are loaded (review of libradicle #15).
    pub fn tree_at_bounded(
        &self,
        rid: RepoId,
        revision: &str,
        path: &str,
        bounds: &Bounds,
    ) -> Result<(Vec<TreeEntry>, CommitInfo), Error> {
        let repo = self.profile.storage.repository(rid)?;
        let objects = Objects::new(&repo.backend, bounds.loads)?;
        let loading = bounds.loading();
        let mut read = Meter::new(&loading);
        let (entries, commit) =
            Self::entries_at(&objects, &mut read, parse_revision(revision)?, path, bounds)?;
        Meter::new(bounds).whole(|w| json::tree_at(w, &entries, &commit))?;
        Ok((entries, commit))
    }

    /// Entries of the tree at the head of the default branch, under
    /// `path` (empty = repository root).
    pub fn tree(&self, rid: RepoId, path: &str) -> Result<Vec<TreeEntry>, Error> {
        self.tree_bounded(rid, path, &Bounds::unbounded())
    }

    /// [`Embedded::tree`], within `bounds`, as [`Embedded::tree_at_bounded`];
    /// the whole result as [`json::tree`] writes it.
    pub fn tree_bounded(
        &self,
        rid: RepoId,
        path: &str,
        bounds: &Bounds,
    ) -> Result<Vec<TreeEntry>, Error> {
        let repo = self.profile.storage.repository(rid)?;
        let objects = Objects::new(&repo.backend, bounds.loads)?;
        let loading = bounds.loading();
        let mut read = Meter::new(&loading);
        let head = Self::head_bounded(&repo, &objects, &mut read, None)?;
        let (entries, _) = Self::entries_at(&objects, &mut read, head, path, bounds)?;
        Meter::new(bounds).whole(|w| json::tree(w, &entries))?;
        Ok(entries)
    }

    fn entries_at(
        objects: &Objects,
        read: &mut Meter,
        oid: git2::Oid,
        path: &str,
        bounds: &Bounds,
    ) -> Result<(Vec<TreeEntry>, CommitInfo), Error> {
        let mut meter = Meter::array(bounds)?;
        let commit = objects.commit(oid, read)?;
        let commit_info = Self::commit_info(&commit);
        let root = objects.tree(commit.tree_id(), read)?;
        let tree = objects.tree_at_path(root, path, read)?;
        let prefix = if path.is_empty() {
            String::new()
        } else {
            format!("{}/", path.trim_end_matches('/'))
        };
        let mut entries = Vec::new();
        for e in tree.iter() {
            meter.check()?;
            let name = e.name().unwrap_or("?").to_string();
            let kind = match e.kind() {
                Some(git2::ObjectType::Tree) => "tree",
                Some(git2::ObjectType::Commit) => "submodule",
                _ => "blob",
            };
            let entry = TreeEntry {
                path: format!("{prefix}{name}"),
                name,
                kind: kind.to_string(),
            };
            meter.item(|w| json::tree_entry(w, &entry))?;
            entries.push(entry);
        }
        Ok((entries, commit_info))
    }

    /// The object id and size of the file at `path` in a specific commit,
    /// from the object's header: none of its content is loaded.
    pub fn blob_header(
        &self,
        rid: RepoId,
        revision: &str,
        path: &str,
    ) -> Result<BlobHeader, Error> {
        self.blob_header_bounded(rid, revision, path, &Bounds::unbounded())
    }

    /// [`Embedded::blob_header`], within `bounds`' loads and token: the
    /// commit and every tree on the way to the file are sized from their
    /// headers before they are loaded, each at most `bounds.loads`, and
    /// what libgit2 holds of each once parsed ([`commit_held`],
    /// [`tree_held`]) is charged, all of them within `bounds.loads`. The
    /// walk stops with [`Error::Cancelled`] once the token is cancelled.
    /// `bounds.budget` isn't used: the result is no JSON.
    pub fn blob_header_bounded(
        &self,
        rid: RepoId,
        revision: &str,
        path: &str,
        bounds: &Bounds,
    ) -> Result<BlobHeader, Error> {
        let repo = self.profile.storage.repository(rid)?;
        let (oid, size) = Self::blob_header_in(&repo, revision, path, bounds)?;
        Ok(BlobHeader {
            oid: oid.to_string(),
            size,
        })
    }

    /// The id and size of the blob at `path` in `revision`. The commit and
    /// every tree on the way are sized from their headers first, each
    /// within `bounds.loads` (review of libradicle #15), and charged, all of
    /// them within it too (review of freedom-core #277, round 2); the
    /// blob's own size is read from its header.
    fn blob_header_in(
        repo: &radicle::storage::git::Repository,
        revision: &str,
        path: &str,
        bounds: &Bounds,
    ) -> Result<(git2::Oid, u64), Error> {
        let objects = Objects::new(&repo.backend, bounds.loads)?;
        let loading = bounds.loading();
        let mut read = Meter::new(&loading);
        let commit = objects.commit(parse_revision(revision)?, &mut read)?;
        let root = objects.tree(commit.tree_id(), &mut read)?;
        let trimmed = path.trim_end_matches('/');
        let (dir, file) = trimmed.rsplit_once('/').unwrap_or(("", trimmed));
        let missing = || {
            Error::Git(git2::Error::new(
                git2::ErrorCode::NotFound,
                git2::ErrorClass::Tree,
                format!("the path '{path}' does not exist in the given tree"),
            ))
        };
        let tree = match objects.tree_at_path(root, dir, &mut read) {
            Err(Error::NotATree(_)) => return Err(missing()),
            other => other?,
        };
        let entry = tree.get_name(file).ok_or_else(missing)?;
        let not_a_blob = || Error::NotABlob(path.to_string());
        if entry.kind() != Some(git2::ObjectType::Blob) {
            return Err(not_a_blob());
        }
        let (size, kind) = repo.backend.odb()?.read_header(entry.id())?;
        if kind != git2::ObjectType::Blob {
            return Err(not_a_blob());
        }
        Ok((entry.id(), size as u64))
    }

    /// Blob content at a specific commit.
    pub fn read_blob_at(&self, rid: RepoId, revision: &str, path: &str) -> Result<Blob, Error> {
        self.read_blob_at_capped(rid, revision, path, u64::MAX)
    }

    /// [`Embedded::read_blob_at`], for a file of at most `limit` bytes. Its
    /// size is read from the object's header first, so a larger file is
    /// refused with [`Error::TooLarge`] before any of it is loaded. The
    /// commit and the trees on the way to it are sized from their headers
    /// too, each at most the larger of `limit` and [`BINDING_READ_BUDGET`],
    /// and what libgit2 holds of them is charged, all of them within it
    /// ([`Embedded::read_blob_at_bounded`]).
    pub fn read_blob_at_capped(
        &self,
        rid: RepoId,
        revision: &str,
        path: &str,
        limit: u64,
    ) -> Result<Blob, Error> {
        // The commit and the trees on the way: within the larger of the
        // blob's cap and a binding's loads budget, each and in all.
        let loads = limit.max(BINDING_READ_BUDGET);
        let bounds = Bounds::unbounded().with_loads(loads);
        self.read_blob_at_bounded(rid, revision, path, limit, &bounds)
    }

    /// [`Embedded::read_blob_at`], for a file of at most `limit` bytes, as
    /// [`Embedded::read_blob_at_capped`], with the walk to it within
    /// `bounds`' loads and token, as [`Embedded::blob_header_bounded`]'s.
    /// The walk's own handles to the commit and the trees are dropped
    /// before the file loads. With libgit2's object cache off, that is all
    /// of the walk, and nothing of it is held while the file loads. With
    /// the cache on (libgit2's default), the repository's cache keeps the
    /// commits and trees under 4 KiB it parsed, so up to the walk's charge
    /// can stay held beside the file until the repository handle closes,
    /// when this returns.
    pub fn read_blob_at_bounded(
        &self,
        rid: RepoId,
        revision: &str,
        path: &str,
        limit: u64,
        bounds: &Bounds,
    ) -> Result<Blob, Error> {
        let repo = self.profile.storage.repository(rid)?;
        let (oid, size) = Self::blob_header_in(&repo, revision, path, bounds)?;
        if size > limit {
            return Err(Error::TooLarge { size, limit });
        }
        if bounds.cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let blob = repo.backend.find_blob(oid)?;
        let content = blob.content();
        if content.len() as u64 > limit {
            return Err(Error::TooLarge {
                size: content.len() as u64,
                limit,
            });
        }
        Ok(Blob {
            binary: blob.is_binary(),
            content: content.to_vec(),
        })
    }

    /// Blob content at the head of the default branch.
    pub fn read_blob(&self, rid: RepoId, path: &str) -> Result<Blob, Error> {
        self.read_blob_capped(rid, path, u64::MAX)
    }

    /// [`Embedded::read_blob`], for a file of at most `limit` bytes, as
    /// [`Embedded::read_blob_at_capped`].
    pub fn read_blob_capped(&self, rid: RepoId, path: &str, limit: u64) -> Result<Blob, Error> {
        let repo = self.profile.storage.repository(rid)?;
        // The head within the loads its commit and trees get (see
        // `read_blob_at_capped`); an uncapped read resolves it as heartwood
        // does.
        let loads = if limit == u64::MAX {
            u64::MAX
        } else {
            limit.max(BINDING_READ_BUDGET)
        };
        let objects = Objects::new(&repo.backend, loads)?;
        let loading = Bounds::new(CancelToken::new(), loads);
        let head = Self::head_bounded(&repo, &objects, &mut Meter::new(&loading), None)?;
        self.read_blob_at_capped(rid, &head.to_string(), path, limit)
    }

    /// Signed repository remotes and their published branch heads.
    pub fn remotes(&self, rid: RepoId) -> Result<Vec<RemoteInfo>, Error> {
        self.remotes_bounded(rid, &Bounds::unbounded())
    }

    /// [`Embedded::remotes`], within `bounds`: each remote is charged as
    /// [`json::remotes`] writes it, before the next is read.
    pub fn remotes_bounded(&self, rid: RepoId, bounds: &Bounds) -> Result<Vec<RemoteInfo>, Error> {
        let mut meter = Meter::array(bounds)?;
        let loading = bounds.loading();
        let read = RefCell::new(Meter::new(&loading));
        let repo = self.profile.storage.repository(rid)?;
        let objects = Objects::new(&repo.backend, bounds.loads)?;
        let identity = if read.borrow().counts() {
            Some(Self::identity_fits(
                &repo,
                &objects,
                &mut read.borrow_mut(),
            )?)
        } else {
            None
        };
        let doc = Self::identity_doc(&repo, identity)?;
        let mut result = Vec::new();
        Self::remotes_loaded(&repo, &objects, &read, |id, remote| {
            meter.check()?;
            let heads = remote
                .iter()
                .filter_map(|(name, oid)| {
                    name.as_str()
                        .strip_prefix("refs/heads/")
                        .map(|branch| (branch.to_string(), oid.to_string()))
                })
                .collect();
            let info = RemoteInfo {
                id: id.to_string(),
                did: radicle::identity::Did::from(id).to_string(),
                delegate: doc.is_delegate(&id.into()),
                heads,
            };
            meter.item(|w| json::remote(w, &info))?;
            result.push(info);
            Ok(())
        })?;
        result.sort_by(|a, b| b.delegate.cmp(&a.delegate).then_with(|| a.id.cmp(&b.id)));
        Ok(result)
    }

    /// Commit, branch, and contributor counts reachable from `revision`.
    pub fn repo_stats(&self, rid: RepoId, revision: &str) -> Result<RepoStats, Error> {
        self.repo_stats_bounded(rid, revision, &Bounds::unbounded())
    }

    /// [`Embedded::repo_stats`], within `bounds`. Its result is three
    /// numbers, but it holds a set of contributors and one of branches while
    /// it counts: each new entry is charged its bytes and [`ENTRY_COST`], and
    /// the result its JSON, as [`json::repo_stats`] writes it. The token is
    /// checked at every commit walked, and at every remote.
    ///
    /// Every commit is sized from its header before it is loaded (review of
    /// libradicle #15): one over the budget is refused unloaded. The walk
    /// itself, all of history from `revision`, is the count it returns.
    pub fn repo_stats_bounded(
        &self,
        rid: RepoId,
        revision: &str,
        bounds: &Bounds,
    ) -> Result<RepoStats, Error> {
        let mut meter = Meter::new(bounds);
        let repo = self.profile.storage.repository(rid)?;
        let head = parse_revision(revision)?;
        // All of history, within the loads budget: what doesn't fit is
        // TooLarge, as for any other read.
        let objects = Objects::new(&repo.backend, bounds.loads)?;
        let loading = bounds.loading();
        let mut read = Meter::new(&loading);
        let mut walk = Walk::new(&objects, head, &mut read)?;

        let mut commits = 0;
        let mut contributors = HashSet::new();
        while let Some(oid) = walk.next(&mut read)? {
            #[cfg(test)]
            seams::at(seams::Step::Commit, &bounds.cancel);
            meter.check()?;
            #[cfg(test)]
            seams::bump(&seams::COMMITS);
            let commit = walk.load(oid)?;
            let author = commit.author();
            let name = author.name().unwrap_or_default().to_string();
            let email = author.email().unwrap_or_default().to_string();
            let bytes = (name.len() + email.len()) as u64;
            if contributors.insert((name, email)) {
                meter.charge(bytes + ENTRY_COST)?;
            }
            commits += 1;
        }

        // The branches every signed remote publishes, as `remotes` reads them.
        let identity = if read.counts() {
            Some(Self::identity_fits(&repo, &objects, &mut read)?)
        } else {
            None
        };
        Self::identity_doc(&repo, identity)?;
        let read = RefCell::new(read);
        let mut branches = HashSet::new();
        Self::remotes_loaded(&repo, &objects, &read, |_, remote| {
            #[cfg(test)]
            seams::at(seams::Step::Remote, &bounds.cancel);
            meter.check()?;
            #[cfg(test)]
            seams::bump(&seams::REMOTES);
            for (name, _) in remote.iter() {
                if let Some(branch) = name.as_str().strip_prefix("refs/heads/") {
                    if !branches.contains(branch) {
                        meter.charge(branch.len() as u64 + ENTRY_COST)?;
                        branches.insert(branch.to_owned());
                    }
                }
            }
            Ok(())
        })?;
        let stats = RepoStats {
            commits,
            branches: branches.len(),
            contributors: contributors.len(),
        };
        meter.whole(|w| json::repo_stats(w, &stats))?;
        Ok(stats)
    }

    /// Gracefully shut the node down, join its thread, and wait for it to
    /// let go of its home, so that a node can start on it again at once.
    ///
    /// heartwood's runtime holds the home until every name lookup of the
    /// node has returned (Security R-16), and its stop doesn't wait for one
    /// past a second. So the home may be held a while after the thread ends:
    /// this waits for it up to [`HOME_RELEASE_LIMIT`], and is
    /// [`Error::ShutdownTimeout`] past that.
    ///
    /// A thread that ended with an error, or panicked, is
    /// [`Error::NodeThread`], whether this call joins it or an earlier
    /// [`Embedded::shutdown_within`] did, and whether or not the home is let
    /// go of in time: this call is the last.
    ///
    /// A host that must tell whether its own call asked the node, or how the
    /// thread ended apart from the home, stops it with
    /// [`Embedded::shutdown_within`] instead, whose [`Shutdown`] says.
    pub fn shutdown(mut self) -> Result<(), Error> {
        if self.running() {
            self.asked = true;
            if self.handle.clone().shutdown().is_err() {
                // The node's control is gone, though its thread runs: its
                // reactor is asked directly, which can't fail.
                (self.stop)();
            }
        }
        self.join_within(None);
        // A thread that ended with an error still holds the home a while.
        let home = self.await_release(std::time::Instant::now().checked_add(HOME_RELEASE_LIMIT));
        // How the thread ended, if with an error, before the release's wait:
        // this call is the last.
        match (&self.end, home) {
            (ThreadEnd::Failed(why), _) => Err(Error::NodeThread(why.clone())),
            (_, HomeLock::Released) => Ok(()),
            (_, HomeLock::Held) => Err(Error::ShutdownTimeout(HOME_RELEASE_LIMIT)),
            (_, HomeLock::Unknown(e)) => Err(e.into()),
        }
    }

    /// Whether the node's thread is still running, and not yet joined.
    fn running(&self) -> bool {
        self.node_thread
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
    }

    /// Waits, until `deadline` if any, for the node's thread to end, and
    /// joins it, keeping how it ended in `self.end` for every later stop
    /// call: `false` if it still runs at the deadline. A thread that has
    /// ended is joined at once, even with no time left.
    fn join_within(&mut self, deadline: Option<std::time::Instant>) -> bool {
        let Some(thread) = self.node_thread.as_ref() else {
            return true;
        };
        // With no deadline, the join waits.
        if let Some(deadline) = deadline {
            while !thread.is_finished() {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    return false;
                }
                std::thread::sleep(left.min(std::time::Duration::from_millis(10)));
            }
        }
        if let Some(thread) = self.node_thread.take() {
            self.end = match thread.join() {
                Err(_) => ThreadEnd::Failed("node thread panicked".into()),
                Ok(Err(e)) => ThreadEnd::Failed(e.to_string()),
                Ok(Ok(())) => ThreadEnd::Stopped,
            };
        }
        true
    }

    /// Waits, until `deadline` if any, for the node, whose thread has ended,
    /// to let go of its home: [`HomeLock::Held`] past it. Once the home has
    /// been seen let go of, it is [`HomeLock::Released`] at once: another
    /// node may have taken it since.
    fn await_release(&mut self, deadline: Option<std::time::Instant>) -> HomeLock {
        while !self.released {
            match self.profile.home.lock_node() {
                Ok(lock) => {
                    drop(lock);
                    self.released = true;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    let poll = std::time::Duration::from_millis(10);
                    let pause = match deadline {
                        Some(deadline) => {
                            let left =
                                deadline.saturating_duration_since(std::time::Instant::now());
                            if left.is_zero() {
                                return HomeLock::Held;
                            }
                            left.min(poll)
                        }
                        None => poll,
                    };
                    std::thread::sleep(pause);
                }
                Err(e) => return HomeLock::Unknown(e),
            }
        }
        HomeLock::Released
    }

    /// The online repair of `rid` after an uncertain write by this node: its
    /// own namespace, then its COB cache (heartwood's
    /// `reconcile::driver::Repair::online`, with the node's own ID). Run it a
    /// unit at a time with [`Embedded::repair_step`].
    pub fn online_repair(&self, rid: RepoId) -> reconcile::driver::Repair {
        reconcile::driver::Repair::online(rid, *self.profile.id())
    }

    /// Runs the next unit of `repair` on this node's storage, signed with
    /// the key the node runs as (freedom-core design 0008, "Writes": the
    /// online repair after an uncertain write). A host that started the
    /// node with [`Embedded::start_with_key`] so keeps no copy of the key
    /// for it.
    ///
    /// The host holds the repository closed at the node's gate
    /// ([`Options::repositories`]) as being repaired, for the whole repair,
    /// and passes the progress state and the quarantine record it keeps. Its
    /// watchdog is told of each unit through `watch`, and `stop` stops the
    /// repair between units. Only the node's own namespace is ever signed.
    ///
    /// A failure outside the repository is [`Error::Reconcile`], and
    /// quarantines nothing; the host's environment among them, which would
    /// fail every repository alike: libgit2's configuration outside any
    /// repository doesn't load, or the repository isn't the process's to
    /// open. The host keeps the repository shut, and runs the repair again
    /// once that is fixed.
    pub fn repair_step(
        &self,
        repair: &mut reconcile::driver::Repair,
        state: &mut reconcile::state::State,
        quarantine: &mut reconcile::quarantine::Quarantine,
        stop: &reconcile::driver::Stop,
        watch: &dyn reconcile::driver::Watch,
    ) -> Result<reconcile::driver::Progress, Error> {
        let signer = self.signer()?;
        let mut cx = reconcile::driver::Context {
            storage: &self.profile.storage,
            state,
            quarantine,
            signer: Some(&signer),
            stop,
            watch,
        };
        Ok(repair.step(&mut cx)?)
    }

    /// Removes the quarantined repository `rid`, if its quarantine record
    /// has `qid`: the only way out of quarantine (freedom-core design 0008,
    /// "The escape"). With no record of `rid`, or one with another `qid`,
    /// nothing is touched, and the answer is `NotQuarantined`.
    ///
    /// Otherwise the node unseeds it first, so that it stops asking for it.
    /// Then heartwood's `remove_repo` runs its steps, each durable before
    /// the next: it stops seeding the repository, whatever the node's
    /// default seeding policy; it deletes `storage/<rid>`, then the
    /// repository's COB cache rows, and last the record. Cut off at any
    /// point, the record stays, and the removal completes when run again
    /// with the same `qid`.
    ///
    /// Under a blocking default, the repository's policy goes, and a seed or
    /// clone after the removal seeds it anew. Under a permissive one, which
    /// would seed it with no policy of its own, it is blocked instead: only
    /// an unblock lets the default seed it again, and a seed or clone of it
    /// doesn't lift that block.
    ///
    /// It runs only with the repository shut at the node's gate, as a
    /// quarantined repository is, and refuses with [`Error::Open`]
    /// otherwise.
    pub fn remove_quarantined(
        &self,
        rid: RepoId,
        qid: reconcile::quarantine::Qid,
        quarantine: &mut reconcile::quarantine::Quarantine,
    ) -> Result<reconcile::quarantine::Removal, Error> {
        // Shut, nothing seeds it while it is removed: neither this crate's
        // clone and import ([`seed`]) nor the node's own seed and unblock
        // commands, which the gate refuses too. So no policy written after
        // the removal's unseed outlives it.
        let state = self.repositories.state(&rid);
        if state.is_open() {
            return Err(Error::Open(rid));
        }
        if quarantine.get(&rid)?.is_none_or(|record| record.qid != qid) {
            return Ok(reconcile::quarantine::Removal::NotQuarantined);
        }
        self.handle.clone().unseed(rid)?;
        // The node's default, the one it seeds by: `Options::seeding_policy`
        // is set on this profile too.
        let mut policies = radicle::node::policy::config::Config::new(
            self.profile.config.node.seeding_policy.into(),
            self.profile.home.policies_mut()?,
        );
        let mut cache = self.profile.home.cobs_db_mut()?;
        Ok(reconcile::quarantine::remove_repo(
            quarantine,
            &mut policies,
            self.profile.storage.path(),
            &mut cache,
            rid,
            qid,
        )?)
    }

    /// Whether the node's thread is still running. `false` once the node has
    /// stopped, whether it was asked to or ended by itself (an error in its
    /// runtime): a host can then restart it, or report why.
    ///
    /// A host deciding how a stop left the node takes it from the stop's
    /// [`Shutdown`] instead: the node can end between this call and the
    /// stop.
    pub fn node_alive(&self) -> bool {
        self.running()
    }

    /// Asks the node to stop, and waits at most `timeout` for its thread to
    /// end and for it to let go of its home. A `timeout` too large to add to
    /// the current time waits for as long as the node takes.
    ///
    /// The [`Shutdown`] it returns says, each apart, from what this one call
    /// saw:
    /// - **[`Shutdown::asked`]:** [`Asked::ThisCall`] if its thread still ran
    ///   when this call asked it to stop, and no stop call of this
    ///   `Embedded` had asked before; [`Asked::Earlier`] if one had;
    ///   [`Asked::Never`] if its thread had ended before any asked.
    /// - **[`Shutdown::thread`]:** how its thread ended,
    ///   [`ThreadEnd::Stopped`] or [`ThreadEnd::Failed`], once it has, and
    ///   [`ThreadEnd::Running`] if it still ran at the deadline. The node is
    ///   joined as soon as its thread has ended, even with no time to wait.
    ///   How it ended is kept, and every later call reports it again,
    ///   whatever the home.
    /// - **[`Shutdown::home`]:** once the thread has ended, it waits, within
    ///   the same deadline, for the node to let go of its home, as
    ///   [`Embedded::shutdown`] does: [`HomeLock::Released`], or
    ///   [`HomeLock::Held`] past the deadline. A name lookup the node started
    ///   holds the home until it returns, however its thread ended.
    ///
    /// Past the deadline the node stays in `self`, asked to stop, and
    /// another call waits again. A host decides from the one `Shutdown`,
    /// with no [`Embedded::node_alive`] check before it: the node can end
    /// between the two.
    ///
    /// The stop goes to the node's reactor directly, never through its
    /// control socket, so asking can't wait on anything.
    pub fn shutdown_within(&mut self, timeout: std::time::Duration) -> Shutdown {
        let deadline = std::time::Instant::now().checked_add(timeout);
        let asked = self.ask();
        let (thread, home) = if self.join_within(deadline) {
            let home = self.await_release(deadline);
            (self.end.clone(), home)
        } else {
            // Its thread runs, so the node holds its home.
            (ThreadEnd::Running, HomeLock::Held)
        };
        Shutdown {
            asked,
            thread,
            home,
            within: timeout,
        }
    }

    /// Asks the node to stop, through its reactor, if its thread still
    /// runs, and records that a stop call has: who asked it first, as this
    /// call finds it ([`Shutdown::asked`]).
    fn ask(&mut self) -> Asked {
        let earlier = self.asked;
        let running = self.running();
        if running {
            self.asked = true;
            (self.stop)();
        }
        match (earlier, running) {
            (true, _) => Asked::Earlier,
            (false, true) => Asked::ThisCall,
            (false, false) => Asked::Never,
        }
    }
}

/// How long [`Embedded::shutdown`] waits, at most, for a stopped node to let
/// go of its home: a name lookup the stop didn't wait for holds it until it
/// returns, as the system's resolver does within its own timeouts.
pub const HOME_RELEASE_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);

/// The deadline of [`shutdown_slot`] for a host that passes none: its
/// stop, its thread's join and its home's release, all together.
pub const SHUTDOWN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// What a stop of the node found ([`Embedded::shutdown_within`]), from one
/// call: who asked the node to stop, how its thread ended, and whether it
/// let go of its home, each apart. A host decides from it alone.
///
/// - **The node stopped because this call asked it** when it is
///   [`Asked::ThisCall`] and [`ThreadEnd::Stopped`]
///   ([`Shutdown::stopped_by_this_call`]). heartwood's run returns `Ok` only
///   once the node was asked to stop, so a node that ends by itself is
///   [`ThreadEnd::Failed`], before this call's request or after it. For a node
///   with no control socket, nothing but this crate's stop calls can ask
///   it.
/// - **A failed thread is reported whatever the home:** a name lookup that
///   still holds the home past the deadline makes it [`HomeLock::Held`], not
///   the thread's end unknown.
/// - **Nothing is left to wait for** once the thread has ended and the home
///   is let go of ([`Shutdown::is_done`]). Until then, the node stays the
///   caller's, asked to stop, and another call waits again.
#[derive(Debug)]
#[must_use]
pub struct Shutdown {
    /// Who asked the node to stop.
    pub asked: Asked,
    /// How the node's thread ended, or that it still ran at the deadline.
    pub thread: ThreadEnd,
    /// Whether the node let go of its home within the deadline.
    pub home: HomeLock,
    /// The call's `timeout`, as it was passed.
    pub within: std::time::Duration,
}

/// Who asked the node to stop, as a stop call found it ([`Shutdown::asked`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asked {
    /// This call: the node's thread still ran when it asked, and no stop
    /// call of this [`Embedded`] had asked before.
    ThisCall,
    /// An earlier stop call of this [`Embedded`] had asked, one that
    /// returned past its deadline or before this one: this call waited
    /// again, or found the node stopped.
    Earlier,
    /// No stop call of this [`Embedded`]: its thread had ended before any
    /// asked. It ended by itself, or was stopped through the node's control
    /// socket (`rad node stop`), which this crate doesn't see.
    Never,
}

/// How the node's thread ended ([`Shutdown::thread`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadEnd {
    /// It still ran at the deadline. It has been asked to stop, and another
    /// call waits for it again.
    Running,
    /// Its run returned `Ok`. heartwood's run returns `Ok` only once the
    /// node was asked to stop: by this crate's stop calls, or through its
    /// control socket.
    Stopped,
    /// Its run returned an error, a fatal one of heartwood's service among
    /// them, or the thread panicked: why, as [`Error::NodeThread`] gives it.
    /// The text can name a path in the node's home.
    Failed(String),
}

/// Whether the node let go of its home's lock ([`Shutdown::home`]).
#[derive(Debug)]
pub enum HomeLock {
    /// It has, as this call or an earlier one found: a node can start on
    /// the home at once. Another may have taken it since.
    Released,
    /// It still held the home at the deadline. Its thread ran, or a name
    /// lookup it started still did: heartwood's runtime holds the home until
    /// every lookup has returned (Security R-16). Another call waits again.
    Held,
    /// Taking the home's node lock failed otherwise, with this error, so
    /// whether the node let go of it isn't known.
    Unknown(std::io::Error),
}

impl Shutdown {
    /// The node stopped because this call asked it: this call was the first
    /// to ask, while its thread ran, and its run returned `Ok`. Whether it
    /// has let go of its home is [`Shutdown::home`]'s to say.
    pub fn stopped_by_this_call(&self) -> bool {
        self.asked == Asked::ThisCall && self.thread == ThreadEnd::Stopped
    }

    /// Nothing is left to wait for: the node's thread has ended, either way,
    /// and it has let go of its home.
    pub fn is_done(&self) -> bool {
        self.thread != ThreadEnd::Running && matches!(self.home, HomeLock::Released)
    }

    /// The outcome as a `Result`, for a host that needs only to know whether
    /// to call again:
    /// - [`Error::ShutdownTimeout`] while the thread runs or the home is
    ///   held: the node is still the caller's;
    /// - then [`Error::NodeThread`] if the thread failed;
    /// - then the error the home's lock failed with, if it did;
    /// - `Ok` once the node stopped cleanly and let go of its home.
    ///
    /// It doesn't say who asked the node to stop.
    pub fn result(self) -> Result<(), Error> {
        match (self.thread, self.home) {
            (ThreadEnd::Running, _) | (_, HomeLock::Held) => {
                Err(Error::ShutdownTimeout(self.within))
            }
            (ThreadEnd::Failed(why), _) => Err(Error::NodeThread(why)),
            (ThreadEnd::Stopped, HomeLock::Unknown(e)) => Err(e.into()),
            (ThreadEnd::Stopped, HomeLock::Released) => Ok(()),
        }
    }
}

/// What [`shutdown_slot`] found.
#[derive(Debug, PartialEq, Eq)]
pub enum Slot {
    /// The node stopped, and let go of its home: the slot is empty.
    Stopped,
    /// The slot was empty: no node was started.
    Empty,
}

/// Proof that [`isolate_git_config`] has run, and succeeded, in this process:
/// only it makes one. A function that must run after it can take one as a
/// parameter, as freedom-radicled's reconciliation can.
#[derive(Debug, Clone, Copy)]
pub struct GitConfigIsolated(());

/// The environment variables an environment-driven libgit2 open reads for its
/// configuration, around the search paths [`isolate_git_config`] empties.
/// libgit2 1.9 reads no others: it has no `GIT_CONFIG_COUNT` or
/// `GIT_CONFIG_PARAMETERS`.
pub const GIT_CONFIG_ENV: [&str; 3] = [
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_NOSYSTEM",
];

/// Empties libgit2's default configuration search paths, the System, XDG,
/// Global and ProgramData levels (freedom-core design 0008, libradicle item
/// 9). From then on, a repository libgit2 opens by its path reads no
/// `/etc/gitconfig`, no `~/.gitconfig` and no `$XDG_CONFIG_HOME/git/config`,
/// and [`git2::Config::open_default`] finds none either. A confined process
/// then opens its repositories: libgit2 fails an open whose system
/// configuration exists but can't be read.
///
/// That is all it does. It leaves two things:
///
/// - **The environment.** An environment-driven open
///   (`Repository::open_from_env`, or `open_ext` with `FROM_ENV`) also reads
///   [`GIT_CONFIG_ENV`], whatever the search paths are. libradicle never opens
///   a repository that way, and nor does any heartwood code it runs: heartwood's
///   storage opens by path with `BARE | NO_DOTGIT | NO_SEARCH`, and its one
///   `FROM_ENV` open, `rad::repo`, is the `rad` CLI's. A host that might
///   clears the variables first ([`remove_git_config_env`]), or refuses to run
///   with them set, as freedom-radicled refuses every `GIT_*` variable.
/// - **A repository's own configuration,** includes and all: an
///   `include.path` or `includeIf.*.path` in it applies wherever it points,
///   and one a confined process can't read fails that repository's open. Only
///   the local owner writes that configuration, not a peer. A storage
///   repository's `config` is written when it is created, and by `set_user`,
///   with the local profile's `user.name` and `user.email` (heartwood's
///   `storage::git::Repository::create`). A fetch writes a peer's objects
///   into `objects/pack` and its refs under its namespace, and nothing else.
///   A working repository [`Embedded::import_repo`] reads is the host's own.
///
/// It doesn't reach a `git` process the node spawns either, in a build
/// without the `no-spawn` feature: git reads its own configuration.
///
/// A later call returns the first one's result and leaves libgit2's options
/// alone. [`Embedded::start`] and [`Embedded::start_with_key`] don't call it:
/// the N-API and UniFFI bindings run in a host's process, where libradicle
/// can't know that the contract below holds.
///
/// # Safety
///
/// Call it before any thread in the process can use libgit2: first thing in
/// `main`, before any other thread is spawned. freedom-radicled does. It
/// writes libgit2's process-wide options
/// (`git_libgit2_opts(GIT_OPT_SET_SEARCH_PATH, …)`), which every
/// configuration open reads without synchronization, so a libgit2 call on
/// another thread at the same time is a data race. A use that has ended
/// before it is harmless; a concurrent one isn't, and libradicle can neither
/// see nor lock another caller's use of libgit2.
pub unsafe fn isolate_git_config() -> Result<GitConfigIsolated, Error> {
    static ISOLATED: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    ISOLATED
        .get_or_init(|| {
            for level in [
                git2::ConfigLevel::System,
                git2::ConfigLevel::XDG,
                git2::ConfigLevel::Global,
                git2::ConfigLevel::ProgramData,
            ] {
                // SAFETY: `set_search_path` writes libgit2's process-wide
                // options, which other libgit2 calls read unsynchronized.
                // This function's caller has no other thread able to use
                // libgit2 (its `# Safety`), and the `OnceLock` runs it once.
                unsafe { git2::opts::set_search_path(level, "") }
                    .map_err(|e| e.message().to_owned())?;
            }
            Ok(())
        })
        .clone()
        .map(|()| GitConfigIsolated(()))
        .map_err(Error::GitConfig)
}

/// Removes [`GIT_CONFIG_ENV`] from the process's environment: what an
/// environment-driven libgit2 open would read around
/// [`isolate_git_config`]'s empty search paths. For a host that might open a
/// repository from the environment, or links code that does; libradicle
/// doesn't. A host can instead refuse to run with them set, as
/// freedom-radicled does.
///
/// # Safety
///
/// [`isolate_git_config`]'s: call it before any thread in the process can use
/// libgit2, or read the environment at all; first thing in `main`, before
/// any other thread is spawned. Changing the environment races with a
/// `getenv` on another thread, libgit2's among them; edition 2024 makes
/// `std::env::remove_var` `unsafe` for that reason.
pub unsafe fn remove_git_config_env() {
    for name in GIT_CONFIG_ENV {
        // SAFETY: no other thread reads the environment (the `# Safety`
        // above).
        std::env::remove_var(name);
    }
}

/// Shuts down the node a binding keeps in `slot`, within `timeout` for all
/// of it: its stop, its thread's join and its home's release
/// ([`Embedded::shutdown_within`], as [`Shutdown::result`] puts it).
///
/// - **Stopped,** and its home let go of: the node is taken out of the slot,
///   [`Slot::Stopped`].
/// - **Past the deadline:** [`Error::ShutdownTimeout`], while its thread
///   runs or its home is held, however its thread ended. The node stays in
///   the slot, asked to stop, so that another call waits again. A host
///   retries until it is stopped before it starts a node on the home again,
///   which is `HomeLocked` until then.
/// - **Any other error is final:** the node is taken out, and the error
///   returned: [`Error::NodeThread`] for a thread that failed, once the home
///   is let go of.
///
/// A host passes a `timeout` below its own watchdog's, so that the call
/// returns before the watchdog fires.
pub fn shutdown_slot(
    slot: &mut Option<Embedded>,
    timeout: std::time::Duration,
) -> Result<Slot, Error> {
    let Some(node) = slot.as_mut() else {
        return Ok(Slot::Empty);
    };
    match node.shutdown_within(timeout).result() {
        Ok(()) => {
            slot.take();
            Ok(Slot::Stopped)
        }
        Err(e @ Error::ShutdownTimeout(_)) => Err(e),
        Err(e) => {
            slot.take();
            Err(e)
        }
    }
}

/// How long dropping an [`Embedded`] waits for a node that still runs.
pub const DROP_SHUTDOWN_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Dropping an [`Embedded`] whose node's thread hasn't been joined stops
/// it, as [`Embedded::shutdown_within`] does, and waits up to
/// [`DROP_SHUTDOWN_LIMIT`] for its thread, joining it. A node that hasn't
/// stopped by then is logged, and its thread is left to finish by itself: it
/// has been asked to stop, and holds the home until it does. A node that
/// stopped with an error is logged too. A thread an earlier stop call
/// joined has been reported by it, and isn't waited for again.
impl Drop for Embedded {
    fn drop(&mut self) {
        if self.node_thread.is_none() {
            return;
        }
        let stopped = self.shutdown_within(DROP_SHUTDOWN_LIMIT);
        if let ThreadEnd::Failed(why) = &stopped.thread {
            log::warn!(
                target: "libradicle",
                "Dropped a node whose thread ended with an error: {why}"
            );
        } else if let Err(e) = stopped.result() {
            log::warn!(target: "libradicle", "Dropped a node that didn't stop cleanly: {e}");
        }
    }
}

/// The repository stored at storage entry `entry`, as heartwood's
/// `Storage::repositories` finds them: a directory named by its RID, neither
/// hidden nor a temporary clone (`.tmp`, or the older `.lock`). Anything else
/// there is `None`; a directory whose name isn't a RID is an error, as there.
fn stored_repository(entry: &std::fs::DirEntry) -> Result<Option<RepoId>, Error> {
    if !entry.file_type()?.is_dir() {
        return Ok(None);
    }
    let name = entry.file_name();
    if name.to_string_lossy().starts_with('.') {
        return Ok(None);
    }
    let path = entry.path();
    if let Some(ext) = path.extension().and_then(|ext| ext.to_str()) {
        if ext == "tmp" || ext == "lock" {
            return Ok(None);
        }
    }
    match RepoId::try_from(name.clone()) {
        Ok(rid) => Ok(Some(rid)),
        Err(_) => Err(radicle::storage::Error::InvalidId(name).into()),
    }
}

/// Seeds `rid` through the node, holding the repository gate's pass for a
/// mutation across the policy write: a repository being repaired or
/// quarantined isn't seeded (`Error::Gated`), and a close of it that has
/// returned has this seed written already, or refuses it. The node's own
/// seed command takes the same pass.
fn seed(
    repositories: &radicle::node::gate::RepoGate,
    handle: &mut NodeHandle,
    rid: RepoId,
) -> Result<(), Error> {
    let Some(_pass) = repositories.admit(rid, radicle::node::gate::Purpose::Mutate) else {
        return Err(Error::Gated {
            rid,
            state: repositories.state(&rid),
        });
    };
    handle.seed(rid, Scope::All)?;
    Ok(())
}

/// The node's repository gate when the host passes none
/// ([`Options::repositories`]): one that admits every repository, for every
/// purpose, as the node did before heartwood had one.
fn repositories() -> radicle::node::gate::RepoGate {
    radicle::node::gate::RepoGate::new()
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

/// `diff` with every line, hunk header and path valid UTF-8, each invalid
/// sequence replaced by U+FFFD.
///
/// radicle-surf keeps diff lines as bytes, and its `Serialize` for a line or
/// a path returns an error when they are not UTF-8. The bindings serialize
/// the diff to JSON, so such a commit (one adding a Latin-1 text file, say)
/// would fail there; JSON text has no way to carry the raw bytes anyway.
/// The diff is rebuilt through `Diff`'s own `insert_*`, which count the same
/// stats radicle-surf counted.
///
/// A bounded read charges the decoded diff ([`Embedded::commit_bounded`]).
/// Decoding never shortens the text: an invalid sequence is at most three
/// bytes, and its U+FFFD is three. So the floors [`Embedded::diff_fits`]
/// takes from the raw bytes stay floors of the decoded diff's JSON.
fn utf8_diff(diff: radicle_surf::diff::Diff) -> radicle_surf::diff::Diff {
    use radicle_surf::diff::{Diff, FileDiff};

    let mut out = Diff::default();
    for file in diff.into_files() {
        match file {
            FileDiff::Added(f) => out.insert_added(utf8_path(f.path), utf8_content(f.diff), f.new),
            FileDiff::Deleted(f) => {
                out.insert_deleted(utf8_path(f.path), utf8_content(f.diff), f.old)
            }
            FileDiff::Modified(f) => {
                out.insert_modified(utf8_path(f.path), utf8_content(f.diff), f.old, f.new)
            }
            FileDiff::Moved(f) => out.insert_moved(
                utf8_path(f.old_path),
                utf8_path(f.new_path),
                f.old,
                f.new,
                utf8_content(f.diff),
            ),
            FileDiff::Copied(f) => out.insert_copied(
                utf8_path(f.old_path),
                utf8_path(f.new_path),
                f.old,
                f.new,
                utf8_content(f.diff),
            ),
        }
    }
    out
}

fn utf8_path(path: PathBuf) -> PathBuf {
    match path.into_os_string().into_string() {
        Ok(text) => PathBuf::from(text),
        Err(raw) => PathBuf::from(raw.to_string_lossy().into_owned()),
    }
}

fn utf8_line(line: radicle_surf::diff::Line) -> radicle_surf::diff::Line {
    match line.from_utf8() {
        Ok(text) => text.into(),
        Err(err) => String::from_utf8_lossy(err.as_bytes()).into_owned().into(),
    }
}

fn utf8_content(content: radicle_surf::diff::DiffContent) -> radicle_surf::diff::DiffContent {
    use radicle_surf::diff::{Addition, Deletion, DiffContent, Hunk, Hunks, Modification};

    let DiffContent::Plain { hunks, stats, eof } = content else {
        return content;
    };
    let hunks = hunks
        .0
        .into_iter()
        .map(|hunk| Hunk {
            header: utf8_line(hunk.header),
            lines: hunk
                .lines
                .into_iter()
                .map(|line| match line {
                    Modification::Addition(Addition { line, line_no }) => {
                        Modification::Addition(Addition {
                            line: utf8_line(line),
                            line_no,
                        })
                    }
                    Modification::Deletion(Deletion { line, line_no }) => {
                        Modification::Deletion(Deletion {
                            line: utf8_line(line),
                            line_no,
                        })
                    }
                    Modification::Context {
                        line,
                        line_no_old,
                        line_no_new,
                    } => Modification::Context {
                        line: utf8_line(line),
                        line_no_old,
                        line_no_new,
                    },
                })
                .collect(),
            old: hunk.old,
            new: hunk.new,
        })
        .collect::<Vec<_>>();
    DiffContent::Plain {
        hunks: Hunks(hunks),
        stats,
        eof,
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_revision, repositories, validate_cob_id};
    use super::{CancelToken, FetchPolicy, Progress, FILE_JSON, HUNK_JSON, LINE_JSON};
    use std::time::{Duration, Instant};

    /// A two-commit git repository whose second commit changes `ok.txt` and
    /// adds `latin1.txt`, both with Latin-1 bytes (and, on Unix, a file whose
    /// name is not UTF-8). Returns the directory and the second commit.
    fn non_utf8_repo(name: &str) -> (std::path::PathBuf, git2::Oid) {
        let dir = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = git2::Repository::init(&dir).expect("init");
        let sig = git2::Signature::new("t", "t@example.com", &git2::Time::new(0, 0)).unwrap();
        let commit = |files: &[(&std::path::Path, &[u8])], parent: Option<git2::Oid>| {
            let mut tree = repo.treebuilder(None).unwrap();
            for (path, bytes) in files {
                let blob = repo.blob(bytes).unwrap();
                tree.insert(path, blob, 0o100644).unwrap();
            }
            let tree = repo.find_tree(tree.write().unwrap()).unwrap();
            let parents = parent.map(|oid| repo.find_commit(oid).unwrap());
            let parents: Vec<_> = parents.iter().collect();
            repo.commit(Some("HEAD"), &sig, &sig, "change", &tree, &parents)
                .unwrap()
        };
        let ok = std::path::Path::new("ok.txt");
        let latin1 = std::path::Path::new("latin1.txt");
        let first = commit(&[(ok, &b"plain\n"[..])], None);
        #[cfg(unix)]
        let odd_name = {
            use std::os::unix::ffi::OsStrExt;
            std::path::PathBuf::from(std::ffi::OsStr::from_bytes(b"na\xefve.txt"))
        };
        let mut files: Vec<(&std::path::Path, &[u8])> = vec![
            (ok, &b"plain\ncaf\xe9\n"[..]),
            (latin1, &b"\xe0 bient\xf4t\n"[..]),
        ];
        #[cfg(unix)]
        files.push((odd_name.as_path(), &b"text\n"[..]));
        let second = commit(&files, Some(first));
        (dir, second)
    }

    #[test]
    fn utf8_diff_makes_a_non_utf8_commit_serializable() {
        let (dir, oid) = non_utf8_repo("libradicle-utf8-diff");
        let surf = radicle_surf::Repository::open(&dir).expect("open");
        let raw = surf
            .diff_commit(radicle_surf::Oid::from(oid))
            .expect("diff");

        // radicle-surf's own serializer refuses the raw diff: the bindings'
        // `json!` unwrapped exactly this error.
        assert!(serde_json::to_string(&raw).is_err());

        let diff = super::utf8_diff(raw.clone());
        let json = serde_json::to_value(&diff).expect("the converted diff serializes");
        let text = json.to_string();
        assert!(text.contains("caf\u{FFFD}"), "{text}");
        assert!(text.contains("\u{FFFD} bient\u{FFFD}t"), "{text}");
        assert!(text.contains("plain"), "{text}");
        #[cfg(unix)]
        assert!(text.contains("na\u{FFFD}ve.txt"), "{text}");

        // Same files, same stats, same shape.
        assert_eq!(diff.stats(), raw.stats());
        assert_eq!(diff.files().count(), raw.files().count());
        let files = json["files"].as_array().unwrap();
        assert!(files.iter().all(|f| f["diff"]["type"] == "plain"), "{text}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn utf8_diff_leaves_a_utf8_diff_unchanged() {
        let (dir, oid) = non_utf8_repo("libradicle-utf8-diff-same");
        let surf = radicle_surf::Repository::open(&dir).expect("open");
        let first = surf
            .commit(radicle_surf::Oid::from(oid))
            .expect("commit")
            .parents[0];
        let raw = surf.diff_commit(first).expect("diff");
        assert_eq!(super::utf8_diff(raw.clone()), raw);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `json::commit` returns radicle-surf's error for a diff it can't
    /// serialize, where the `json!` it replaced unwrapped it and panicked;
    /// and writes what that `json!` did, byte for byte, for one it can.
    #[test]
    fn commit_json_is_fallible_and_unchanged() {
        let (dir, oid) = non_utf8_repo("libradicle-utf8-commit-json");
        let surf = radicle_surf::Repository::open(&dir).expect("open");
        let commit = surf.commit(radicle_surf::Oid::from(oid)).expect("commit");
        let raw = surf.diff_commit(commit.id).expect("diff");
        let decoded = super::utf8_diff(raw.clone());

        let refused = super::CommitDetail {
            commit: commit.clone(),
            diff: raw,
        };
        assert!(super::json::commit(&mut Vec::new(), &refused).is_err());
        let failed = super::json::to_string(|w| super::json::commit(w, &refused));
        assert!(failed.starts_with(r#"{"error":"#), "{failed}");

        let first = surf.commit(commit.parents[0]).expect("first commit");
        let first_diff = surf.diff_commit(first.id).expect("first diff");
        for detail in [
            super::CommitDetail {
                commit,
                diff: decoded,
            },
            super::CommitDetail {
                commit: first,
                diff: first_diff,
            },
        ] {
            let before = serde_json::json!({
                "commit": detail.commit,
                "diff": detail.diff,
            })
            .to_string();
            assert_eq!(
                super::json::to_string(|w| super::json::commit(w, &detail)),
                before
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The bounded diff's line count is a floor of radicle-surf's JSON: no
    /// line's JSON is shorter than `LINE_JSON`, an empty addition's.
    #[test]
    fn no_diff_line_takes_less_json_than_line_json() {
        use radicle_surf::diff::Modification;
        let len = |line: Modification| serde_json::to_string(&line).unwrap().len() as u64;
        assert_eq!(len(Modification::addition(String::new(), 1)), LINE_JSON);
        assert_eq!(len(Modification::deletion(String::new(), 1)), LINE_JSON);
        assert!(len(Modification::context(String::new(), 1, 1)) > LINE_JSON);
    }

    /// The bounded diff's file and hunk counts are a floor of radicle-surf's
    /// JSON too: no file record is shorter than `FILE_JSON`, a rename's with
    /// empty paths, and no hunk than `HUNK_JSON`, an empty one's.
    #[test]
    fn no_diff_record_takes_less_json_than_its_floor() {
        use radicle_surf::diff::*;
        use std::path::PathBuf;
        let file = DiffFile {
            oid: radicle_surf::Oid::from(git2::Oid::ZERO_SHA1),
            mode: FileMode::Blob,
        };
        let records = [
            FileDiff::Added(Added {
                path: PathBuf::new(),
                diff: DiffContent::Empty,
                new: file.clone(),
            }),
            FileDiff::Deleted(Deleted {
                path: PathBuf::new(),
                diff: DiffContent::Empty,
                old: file.clone(),
            }),
            FileDiff::Modified(Modified {
                path: PathBuf::new(),
                diff: DiffContent::Empty,
                old: file.clone(),
                new: file.clone(),
            }),
            FileDiff::Moved(Moved {
                old_path: PathBuf::new(),
                old: file.clone(),
                new_path: PathBuf::new(),
                new: file.clone(),
                diff: DiffContent::Empty,
            }),
            FileDiff::Copied(Copied {
                old_path: PathBuf::new(),
                old: file.clone(),
                new_path: PathBuf::new(),
                new: file.clone(),
                diff: DiffContent::Binary,
            }),
        ];
        let len = |json: String| json.len() as u64;
        let shortest = records
            .iter()
            .map(|record| len(serde_json::to_string(record).unwrap()))
            .min();
        assert_eq!(shortest, Some(FILE_JSON));
        let hunk: Hunk<Modification> = Hunk {
            header: Line::from(String::new()),
            lines: vec![],
            old: 0..0,
            new: 0..0,
        };
        assert_eq!(len(serde_json::to_string(&hunk).unwrap()), HUNK_JSON);
    }

    /// No listed repository takes less JSON than `REPO_JSON`: one with an
    /// empty name and description, and the RID of 20 zero bytes, the
    /// shortest, takes exactly that.
    #[test]
    fn no_repository_takes_less_json_than_repo_json() {
        use super::{json, RepoId, RepoInfo, REPO_JSON};
        let shortest = "rad:z11111111111111111111";
        let info = |rid: RepoId| RepoInfo {
            rid,
            name: String::new(),
            description: String::new(),
            default_branch: String::new(),
            head: String::new(),
            delegates: vec![],
            threshold: 0,
            visibility: Default::default(),
            issues_open: 0,
            patches_open: 0,
        };
        let len = |info: &RepoInfo| json::to_string(|w| json::repo(w, info)).len() as u64;
        let floor = info(shortest.parse().expect("a RID"));
        assert_eq!(floor.rid.to_string(), shortest);
        assert_eq!(len(&floor), REPO_JSON);
        let ones =
            radicle::git::Oid::from(radicle::git::raw::Oid::from_bytes(&[0xff; 20]).unwrap());
        assert!(len(&info(RepoId::from(ones))) > REPO_JSON);
    }

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

    #[test]
    fn the_node_s_repository_gate_admits_everything() {
        use radicle::node::gate::Purpose;

        let gate = repositories();
        let rids = [[1u8; 20], [2u8; 20]].map(|bytes| {
            super::RepoId::from(radicle::git::Oid::from(
                radicle::git::raw::Oid::from_bytes(&bytes).unwrap(),
            ))
        });
        for rid in rids {
            assert!(gate.state(&rid).is_open());
            for purpose in [
                Purpose::Mutate,
                Purpose::Serve,
                Purpose::Announce,
                Purpose::Inventory,
            ] {
                assert!(gate.admit(rid, purpose).is_some(), "{rid} for {purpose:?}");
            }
        }
        for purpose in [
            Purpose::Mutate,
            Purpose::Serve,
            Purpose::Announce,
            Purpose::Inventory,
        ] {
            assert_eq!(gate.admit_all(rids, purpose).rids(), rids);
        }
    }

    /// A gate whose name lookups wait until it is released.
    #[derive(Debug, Default)]
    struct Held {
        state: std::sync::Mutex<(usize, bool)>,
        changed: std::sync::Condvar,
    }

    impl Held {
        fn wait_for_a_lookup(&self) {
            let mut state = self.state.lock().unwrap();
            while state.0 == 0 {
                state = self.changed.wait(state).unwrap();
            }
        }

        fn release(&self) {
            self.state.lock().unwrap().1 = true;
            self.changed.notify_all();
        }
    }

    impl radicle::node::dial::Gate for Held {
        fn allows(&self, _: &radicle::node::Address, _: &std::net::SocketAddr) -> bool {
            false
        }

        fn resolve(&self, _: &str, _: u16) -> std::io::Result<Vec<std::net::SocketAddr>> {
            let mut state = self.state.lock().unwrap();
            state.0 += 1;
            self.changed.notify_all();
            while !state.1 {
                state = self.changed.wait(state).unwrap();
            }
            Ok(vec![])
        }
    }

    /// A node of a fresh home under /tmp, whose one name lookup `gate`
    /// holds, its thread stopped and joined: the home stays held until the
    /// lookup is released.
    fn stopped_with_a_held_home(
        name: &str,
        gate: &std::sync::Arc<Held>,
    ) -> (super::Embedded, std::path::PathBuf) {
        use radicle::crypto::Signer as _;

        let home =
            std::path::Path::new("/tmp").join(format!("radunit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let peer = *radicle::crypto::SigningKey::from_seed(radicle::crypto::Seed::new([7; 32]))
            .public_key();
        std::fs::write(
            home.join("config.json"),
            format!(
                r#"{{"preferredSeeds":[],"node":{{"alias":"unit","network":"test","peers":{{"type":"static"}},"connect":["{peer}@held.test:8776"]}}}}"#
            ),
        )
        .unwrap();
        let opts = super::Options {
            home: home.to_str().unwrap().into(),
            alias: "unit".into(),
            listen: vec![],
            control_socket: true,
            routing: Some(super::Routing {
                proxy: None,
                onion: radicle::node::config::AddressConfig::Drop,
                gate: super::hermetic::wrapping(gate.clone()),
            }),
            seeding_policy: None,
            isolated: false,
            repositories: None,
        };
        super::hermetic::check(
            &opts.home,
            opts.isolated,
            &opts.listen,
            Some((None, &radicle::node::config::AddressConfig::Drop)),
        );
        let mut node = super::Embedded::start(opts).unwrap();
        gate.wait_for_a_lookup();
        (node.stop)();
        node.node_thread.take().unwrap().join().unwrap().unwrap();
        // As a stop call's join keeps it.
        node.end = super::ThreadEnd::Stopped;
        (node, home)
    }

    /// Whether `stopped` says the thread failed with a reason holding
    /// `why`.
    fn failed(stopped: &super::Shutdown, why: &str) -> bool {
        matches!(&stopped.thread, super::ThreadEnd::Failed(e) if e.contains(why))
    }

    #[test]
    // Review of libradicle #19, round 2, and freedom-core #249, round 1: a
    // node whose thread ended with an error by itself, while a lookup still
    // holds its home past the deadline, is `Failed` at once, apart from the
    // held home, and so is every later call. Before, the call was
    // `ShutdownTimeout` alone, and the error came only once the home was let
    // go of.
    fn a_thread_that_ended_with_an_error_is_reported_while_a_lookup_holds_the_home() {
        use super::{Asked, HomeLock};

        let gate = std::sync::Arc::new(Held::default());
        let (mut node, home) = stopped_with_a_held_home("terminal", &gate);
        // The thread ended with an error of its runtime, before any stop
        // call asked it.
        node.node_thread = Some(std::thread::spawn(|| {
            Err(radicle_node::runtime::Error::Io(std::io::Error::other(
                "the runtime failed",
            )))
        }));
        while !node.node_thread.as_ref().unwrap().is_finished() {
            std::thread::sleep(Duration::from_millis(1));
        }

        for within in [Duration::ZERO, Duration::from_millis(300)] {
            let started = Instant::now();
            let stopped = node.shutdown_within(within);
            assert!(started.elapsed() >= within, "{:?}", started.elapsed());
            assert_eq!(stopped.asked, Asked::Never, "{stopped:?}");
            assert!(failed(&stopped, "the runtime failed"), "{stopped:?}");
            assert!(matches!(stopped.home, HomeLock::Held), "{stopped:?}");
            assert!(!stopped.stopped_by_this_call() && !stopped.is_done());
            // As a `Result`, still a timeout: a binding keeps the node in
            // its slot until the home is let go of.
            let result = stopped.result();
            assert!(
                matches!(result, Err(super::Error::ShutdownTimeout(d)) if d == within),
                "{result:?}"
            );
        }
        gate.release();
        let stopped = node.shutdown_within(Duration::from_secs(10));
        assert_eq!(stopped.asked, Asked::Never, "{stopped:?}");
        assert!(failed(&stopped, "the runtime failed"), "{stopped:?}");
        assert!(matches!(stopped.home, HomeLock::Released), "{stopped:?}");
        assert!(stopped.is_done() && !stopped.stopped_by_this_call());
        let result = stopped.result();
        assert!(
            matches!(&result, Err(super::Error::NodeThread(e)) if e.contains("the runtime failed")),
            "{result:?}"
        );
        // Every later call reports it again.
        let again = node.shutdown_within(Duration::ZERO);
        assert!(failed(&again, "the runtime failed"), "{again:?}");
        assert!(again.is_done(), "{again:?}");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    // freedom-core #249, round 1: a node this call asked to stop, whose
    // thread then ended with an error, while a lookup holds its home past
    // the deadline: this call asked it, and it failed, whatever the home.
    fn a_thread_asked_by_this_call_that_failed_is_reported_while_a_lookup_holds_the_home() {
        use super::{Asked, HomeLock};
        use std::sync::atomic::{AtomicBool, Ordering};

        let gate = std::sync::Arc::new(Held::default());
        let (mut node, home) = stopped_with_a_held_home("asked", &gate);
        // A thread that runs until it is asked to stop, and then fails.
        let asked = std::sync::Arc::new(AtomicBool::new(false));
        node.stop = Box::new({
            let asked = asked.clone();
            move || asked.store(true, Ordering::SeqCst)
        });
        node.node_thread = Some(std::thread::spawn({
            let asked = asked.clone();
            move || {
                while !asked.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(radicle_node::runtime::Error::Io(std::io::Error::other(
                    "failed while stopping",
                )))
            }
        }));
        assert!(node.node_alive());

        let stopped = node.shutdown_within(Duration::from_millis(500));
        assert!(asked.load(Ordering::SeqCst));
        assert_eq!(stopped.asked, Asked::ThisCall, "{stopped:?}");
        assert!(failed(&stopped, "failed while stopping"), "{stopped:?}");
        assert!(matches!(stopped.home, HomeLock::Held), "{stopped:?}");
        assert!(!stopped.stopped_by_this_call() && !stopped.is_done());

        gate.release();
        let stopped = node.shutdown_within(Duration::from_secs(10));
        assert_eq!(stopped.asked, Asked::Earlier, "{stopped:?}");
        assert!(failed(&stopped, "failed while stopping"), "{stopped:?}");
        assert!(matches!(stopped.home, HomeLock::Released), "{stopped:?}");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    // The outcome as a `Result`: a timeout while the thread runs or the home
    // is held, then the thread's failure, then the home lock's error.
    fn the_outcome_as_a_result_keeps_the_node_while_it_holds_its_home() {
        use super::{Asked, HomeLock, Shutdown, ThreadEnd};

        let within = Duration::from_millis(7);
        let outcome = |thread: ThreadEnd, home: HomeLock| Shutdown {
            asked: Asked::ThisCall,
            thread,
            home,
            within,
        };
        let failed = || ThreadEnd::Failed("ended".into());
        let unknown = || HomeLock::Unknown(std::io::Error::other("lock"));
        for (stopped, timeout) in [
            (outcome(ThreadEnd::Running, HomeLock::Held), true),
            (outcome(ThreadEnd::Stopped, HomeLock::Held), true),
            (outcome(failed(), HomeLock::Held), true),
            (outcome(failed(), HomeLock::Released), false),
            (outcome(failed(), unknown()), false),
            (outcome(ThreadEnd::Stopped, unknown()), false),
            (outcome(ThreadEnd::Stopped, HomeLock::Released), false),
        ] {
            let what = format!("{stopped:?}");
            let thread = stopped.thread.clone();
            let io = matches!(stopped.home, HomeLock::Unknown(_));
            match (stopped.result(), thread) {
                (Err(super::Error::ShutdownTimeout(d)), _) => {
                    assert!(timeout && d == within, "{what}")
                }
                (Err(super::Error::NodeThread(e)), ThreadEnd::Failed(_)) => {
                    assert!(!timeout && e == "ended", "{what}")
                }
                (Err(super::Error::Io(e)), ThreadEnd::Stopped) => {
                    assert!(!timeout && io && e.to_string() == "lock", "{what}")
                }
                (Ok(()), ThreadEnd::Stopped) => assert!(!timeout && !io, "{what}"),
                (other, _) => panic!("{what}: {other:?}"),
            }
        }
        // Who asked, and whether anything is left to wait for.
        for (asked, thread, home, by_this_call, done) in [
            (
                Asked::ThisCall,
                ThreadEnd::Stopped,
                HomeLock::Released,
                true,
                true,
            ),
            (
                Asked::ThisCall,
                ThreadEnd::Stopped,
                HomeLock::Held,
                true,
                false,
            ),
            (Asked::ThisCall, failed(), HomeLock::Released, false, true),
            (
                Asked::ThisCall,
                ThreadEnd::Running,
                HomeLock::Held,
                false,
                false,
            ),
            (
                Asked::Earlier,
                ThreadEnd::Stopped,
                HomeLock::Released,
                false,
                true,
            ),
            (
                Asked::Never,
                ThreadEnd::Stopped,
                HomeLock::Released,
                false,
                true,
            ),
            (Asked::ThisCall, ThreadEnd::Stopped, unknown(), true, false),
        ] {
            let stopped = Shutdown {
                asked,
                thread,
                home,
                within,
            };
            assert_eq!(
                (stopped.stopped_by_this_call(), stopped.is_done()),
                (by_this_call, done),
                "{stopped:?}"
            );
        }
    }

    #[test]
    // Review of libradicle #19, round 3: the consuming `shutdown` returns a
    // terminal error that a bounded call past its deadline kept, once the
    // home is let go of.
    fn a_terminal_error_kept_by_a_bounded_call_reaches_the_consuming_shutdown() {
        let gate = std::sync::Arc::new(Held::default());
        let (mut node, home) = stopped_with_a_held_home("consuming", &gate);
        node.node_thread = Some(std::thread::spawn(|| {
            Err(radicle_node::runtime::Error::Io(std::io::Error::other(
                "the saved runtime failure",
            )))
        }));
        while !node.node_thread.as_ref().unwrap().is_finished() {
            std::thread::sleep(Duration::from_millis(1));
        }
        let stopped = node.shutdown_within(Duration::ZERO);
        assert!(failed(&stopped, "the saved runtime failure"), "{stopped:?}");
        assert!(matches!(stopped.home, super::HomeLock::Held), "{stopped:?}");
        let release = {
            let gate = gate.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                gate.release();
            })
        };
        let result = node.shutdown();
        assert!(
            matches!(&result, Err(super::Error::NodeThread(e)) if e.contains("the saved runtime failure")),
            "{result:?}"
        );
        release.join().unwrap();
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    // Review of libradicle #19, round 3: the consuming `shutdown` of a node
    // whose thread has ended with an error returns that error, though its
    // control is gone.
    fn the_consuming_shutdown_of_an_ended_node_returns_how_it_ended() {
        let gate = std::sync::Arc::new(Held::default());
        let (mut node, home) = stopped_with_a_held_home("ended", &gate);
        node.node_thread = Some(std::thread::spawn(|| {
            Err(radicle_node::runtime::Error::Io(std::io::Error::other(
                "the runtime ended",
            )))
        }));
        while !node.node_thread.as_ref().unwrap().is_finished() {
            std::thread::sleep(Duration::from_millis(1));
        }
        gate.release();
        let result = node.shutdown();
        assert!(
            matches!(&result, Err(super::Error::NodeThread(e)) if e.contains("the runtime ended")),
            "{result:?}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    // Review of libradicle #19, round 2: a short deadline bounds the wait for
    // the home's release too, rather than the 10 ms it polls at.
    fn a_short_deadline_bounds_the_wait_for_the_home() {
        let gate = std::sync::Arc::new(Held::default());
        let (mut node, home) = stopped_with_a_held_home("short", &gate);
        // The quickest of ten calls: a loaded scheduler can hold up any one
        // sleep, but the full 10 ms poll would hold up every one.
        let mut quickest = Duration::MAX;
        for _ in 0..10 {
            let started = Instant::now();
            let stopped = node.shutdown_within(Duration::from_millis(1));
            quickest = quickest.min(started.elapsed());
            assert!(matches!(stopped.home, super::HomeLock::Held), "{stopped:?}");
        }
        // Sleeps on Windows round up to its timer's tick.
        if cfg!(not(windows)) {
            assert!(
                quickest < Duration::from_millis(8),
                "the quickest call took {quickest:?}"
            );
        }
        gate.release();
        node.shutdown_within(Duration::from_secs(10))
            .result()
            .expect("the home is let go of");
        let _ = std::fs::remove_dir_all(&home);
    }
}
