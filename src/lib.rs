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

use radicle::crypto::ssh::Keystore;
use radicle::identity::RepoId;
use radicle::issue::cache::Issues as _;
use radicle::node::policy::Scope;
use radicle::node::{ConnectOptions, ConnectResult, FetchResult, Handle as _};
use radicle::patch::cache::Patches as _;
use radicle::prelude::*;
use radicle::profile::{env, Home, Profile};
use radicle::storage::{ReadRepository, ReadStorage};
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
            // TODO(fork): add `Profile::load_from(home)` upstream; `load()`
            // only reads $RAD_HOME. One profile per process is fine for now.
            env::set_var(env::RAD_HOME, opts.home.as_os_str());
            Profile::load()?
        } else {
            let alias = Alias::new(&opts.alias);
            let seed = env::seed().unwrap_or_else(|| {
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
