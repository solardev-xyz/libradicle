//! The calls that wait on the node's reactor for an answer wait within a
//! bound: `connected_peers` and `unseed_repo` (freedom-core #340), and
//! `seeders`, the announcement of each COB change (`create_issue`,
//! `comment_issue`, `edit_issue_state`, `comment_patch`) and
//! `remove_quarantined`'s unseed (freedom-core #363). A wedged reactor
//! answers none: each call returns `NodeDeadline` at its bound, and
//! `waiting_on_node` names it. A COB change is committed before it's
//! announced, so it stands either way, and its error is an announcement's;
//! a failure of the change itself isn't. A removal cut off at its unseed
//! keeps its record, and completes when run again.
//!
//! At most `NODE_WAIT_LIMIT` calls wait at once, within their bounds or past
//! them, even for callers that all arrive together: a call past those isn't
//! sent (`NodeBusy`). When the reactor comes back, it answers the calls left
//! waiting, and they end.
//!
//! The reactor is wedged through the node's dial gate, which it asks on its
//! own thread before every dial: this test's gate doesn't return until the
//! test lets it. The tests run one at a time, as they count the process's
//! waiting threads. Their time bounds are generous, and only check that a
//! call returns: what a call did is read from its error.

mod common;

use std::path::{Path, PathBuf};
use std::str::FromStr as _;
use std::sync::{Arc, Barrier, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use libradicle::{Embedded, Error, HostKey, Options, NODE_WAIT_LIMIT};
use radicle::cob::issue::State as IssueState;
use radicle::crypto::Signer as _;
use radicle::git::raw as git2;
use radicle::identity::RepoId;
use radicle::node::gate::{Quarantine as Shut, RepoGate, RepoState};
use radicle::storage::git::reconcile::quarantine::{Kind, Quarantine, Removal, QUARANTINE_DB_FILE};
use radicle::storage::WriteStorage as _;

/// The node's key: the seed of its signing key.
const KEY: [u8; 32] = [71; 32];

/// How long a test waits, at most, for something it expects soon: a bound
/// that only catches a hang.
const SOON: Duration = Duration::from_secs(60);

/// The tests' turns: one at a time.
fn turn() -> MutexGuard<'static, ()> {
    static TURN: Mutex<()> = Mutex::new(());
    TURN.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A dial gate that holds the reactor in its first ask until it's let go,
/// then refuses every dial.
#[derive(Debug, Default)]
struct Wedge {
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}

impl Wedge {
    /// Waits until the reactor is held.
    fn wait_held(&self, within: Duration) {
        let (state, timeout) = self
            .changed
            .wait_timeout_while(self.state.lock().unwrap(), within, |(held, _)| !*held)
            .unwrap();
        assert!(
            state.0,
            "the reactor wasn't held within {within:?}: {timeout:?}"
        );
    }

    fn let_go(&self) {
        self.state.lock().unwrap().1 = true;
        self.changed.notify_all();
    }
}

impl radicle::node::dial::Gate for Wedge {
    fn allows(&self, _: &radicle::node::Address, _: &std::net::SocketAddr) -> bool {
        let mut state = self.state.lock().unwrap();
        state.0 = true;
        self.changed.notify_all();
        let _released = self
            .changed
            .wait_while(state, |(_, let_go)| !*let_go)
            .unwrap();
        false
    }

    fn resolve(&self, _: &str, _: u16) -> std::io::Result<Vec<std::net::SocketAddr>> {
        Err(std::io::ErrorKind::PermissionDenied.into())
    }
}

/// A home named `name` on the test network, with a static peer set and one
/// preferred seed at a loopback address, for the gate to be asked about.
fn home(name: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!("lr-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.json"),
        r#"{"preferredSeeds":["z6MktwupdmLXVVqTzCw4i46r4uGyosGXRnR3XjN4Zq7oMMsw@127.0.0.1:1"],"node":{"alias":"waits","network":"test","connect":[],"peers":{"type":"static"}}}"#,
    )
    .unwrap();
    home
}

fn start(home: &Path, gate: Arc<Wedge>, repositories: RepoGate) -> Embedded {
    let mut bytes = KEY.to_vec();
    common::start_with_key(
        Options {
            home: home.to_path_buf(),
            alias: "waits".into(),
            listen: vec![],
            control_socket: false,
            routing: Some(common::routing_with(Some(gate))),
            seeding_policy: None,
            isolated: false,
            repositories: Some(repositories),
        },
        HostKey::from_bytes(&mut bytes).expect("32 bytes"),
    )
    .expect("start")
}

/// A repository named `name` imported into `node`'s storage, from a working
/// copy with one commit on `master`; and that commit.
fn import(node: &mut Embedded, name: &str) -> (RepoId, git2::Oid) {
    let work = std::env::temp_dir().join(format!("lr-waits-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).unwrap();
    let repo = git2::Repository::init(&work).unwrap();
    std::fs::write(work.join("README.md"), "# Waits\n").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("README.md")).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let signature = git2::Signature::now("Waits", "waits@example.invalid").unwrap();
    let commit = repo
        .commit(
            Some("refs/heads/master"),
            &signature,
            &signature,
            "First",
            &tree,
            &[],
        )
        .unwrap();
    let rid = node
        .import_repo(&work, name, "A waits test.", "master")
        .expect("import");
    let _ = std::fs::remove_dir_all(&work);
    (rid, commit)
}

/// A repository with an issue and a patch, each COB change a test can make
/// waiting on its announcement.
struct Writes {
    rid: RepoId,
    issue: String,
    revision: String,
}

/// [`Writes`], made in `node`, whose home is `home`, while its reactor
/// runs. libradicle opens no patch: this one is opened with heartwood's
/// store, as the node's key.
fn writes(node: &mut Embedded, home: &Path) -> Writes {
    let (rid, commit) = import(node, "written");
    let issue = node
        .create_issue(rid, "Written", "Its description.", vec![])
        .expect("an issue");

    let key = radicle::crypto::SigningKey::from_seed(radicle::crypto::Seed::new(KEY));
    let public_key = *key.public_key();
    let home = radicle::profile::Home::new(home).unwrap();
    let config = radicle::profile::Config::load(home.config().as_path()).unwrap();
    let storage = radicle::Storage::open(
        home.storage(),
        radicle::git::UserInfo {
            alias: config.alias().clone(),
            key: public_key,
        },
    )
    .unwrap();
    let profile = radicle::Profile {
        keystore: radicle::crypto::ssh::Keystore::new(&home.keys()),
        home,
        storage,
        public_key,
        config,
    };
    let repo = profile.storage.repository_mut(rid).unwrap();
    let mut patches = profile.patches_mut(&repo, &key).unwrap();
    let commit = radicle::git::Oid::from_str(&commit.to_string()).unwrap();
    patches
        .create(
            radicle::cob::Title::new("Proposed").unwrap(),
            "Its description.",
            radicle::cob::patch::MergeTarget::Delegates,
            commit,
            commit,
            &[],
        )
        .expect("a patch");
    let (_, patch) = node
        .patches(rid)
        .expect("the patches")
        .into_iter()
        .next()
        .expect("the patch");
    Writes {
        rid,
        issue,
        revision: patch.root().0.to_string(),
    }
}

/// What each COB change left in storage: the issues' titles, the comments
/// on the issue, whether it's closed, and the comments on the patch's
/// revision.
#[derive(Debug, PartialEq, Eq)]
struct Stands {
    titles: Vec<String>,
    comments: usize,
    closed: bool,
    patch_comments: usize,
}

fn stands(node: &Embedded, writes: &Writes) -> Stands {
    let issues = node.issues(writes.rid).expect("the issues");
    let mut titles = issues
        .iter()
        .map(|(_, issue)| issue.title().to_string())
        .collect::<Vec<_>>();
    titles.sort();
    let (_, issue) = issues
        .iter()
        .find(|(id, _)| id.to_string() == writes.issue)
        .expect("the issue");
    let (_, patch) = node
        .patches(writes.rid)
        .expect("the patches")
        .into_iter()
        .next()
        .expect("the patch");
    Stands {
        titles,
        comments: issue.comments().count(),
        closed: matches!(issue.state(), IssueState::Closed { .. }),
        patch_comments: patch.root().1.discussion().len(),
    }
}

/// Each COB change, `tag` in what it writes, closing the issue if `close`
/// and reopening it otherwise, each waiting on its announcement up to
/// `within`.
fn write_each(
    node: &Embedded,
    writes: &Writes,
    tag: &str,
    close: bool,
    within: Duration,
) -> [Result<String, Error>; 4] {
    let state = if close { "closed" } else { "open" };
    [
        node.create_issue_within(writes.rid, tag, "Its description.", vec![], within),
        node.comment_issue_within(writes.rid, &writes.issue, tag, None, within),
        node.edit_issue_state_within(writes.rid, &writes.issue, state, within),
        node.comment_patch_within(writes.rid, &writes.revision, tag, within),
    ]
}

/// Holds `node`'s reactor in `gate`: a dial of the seed reaches the gate on
/// the reactor's thread, which it holds from then on. The dial's thread, to
/// join once the gate lets go.
fn wedge(
    node: &Embedded,
    gate: &Wedge,
) -> JoinHandle<Result<libradicle::SeedConnectReport, Error>> {
    let dial = {
        let mut network = node.network();
        std::thread::spawn(move || network.connect_seed_book(Duration::from_secs(1)))
    };
    gate.wait_held(SOON);
    dial
}

/// The threads waiting on the node for a caller: on Linux, by their name;
/// elsewhere, the calls the node lists.
fn helpers(node: &Embedded) -> usize {
    #[cfg(target_os = "linux")]
    {
        let _ = node;
        std::fs::read_dir("/proc/self/task")
            .unwrap()
            .filter_map(|task| std::fs::read_to_string(task.ok()?.path().join("comm")).ok())
            .filter(|name| name.trim_end() == "radicle-wait")
            .count()
    }
    #[cfg(not(target_os = "linux"))]
    {
        node.waiting_on_node().len()
    }
}

/// Waits up to `within` for `done`, or panics naming `what`.
fn wait_for(what: &str, within: Duration, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < within, "no {what} within {within:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The calls `node` lists as waiting, each with whether it's overdue.
fn listed(node: &Embedded) -> Vec<(&'static str, bool)> {
    node.waiting_on_node()
        .iter()
        .map(|wait| (wait.call, wait.overdue))
        .collect()
}

fn deadline(answer: &Result<impl std::fmt::Debug, Error>, call: &str, bound: Duration) -> bool {
    matches!(answer, Err(Error::NodeDeadline { call: c, within }) if *c == call && *within == bound)
}

fn busy(answer: &Result<impl std::fmt::Debug, Error>, call: &str) -> bool {
    matches!(answer, Err(Error::NodeBusy { call: c, waiting }) if *c == call && *waiting == NODE_WAIT_LIMIT)
}

#[test]
fn a_wedged_reactor_answers_no_call_past_its_bound() {
    let _turn = turn();
    let home = home("waits-calls");
    let gate = Arc::new(Wedge::default());
    let repositories = RepoGate::new();
    let mut node = start(&home, Arc::clone(&gate), repositories.clone());
    let rid = RepoId::from_str("rad:z3gqcJUoA1n9HaHKufZs5FCSGazv5").unwrap();
    let bound = Duration::from_millis(300);

    // A repository to write to, and one quarantined and shut, to remove.
    let writes = writes(&mut node, &home);
    let (removed, _) = import(&mut node, "removed");
    let mut quarantine = Quarantine::open(home.join("node").join(QUARANTINE_DB_FILE)).unwrap();
    let record = quarantine
        .quarantine(
            removed,
            Kind::Namespace,
            "test",
            None,
            "a test",
            Default::default(),
        )
        .unwrap();
    repositories
        .close(
            removed,
            RepoState::Quarantined(Shut::Namespace),
            Duration::from_secs(5),
        )
        .expect("the gate closes");
    let removed_storage = home.join("storage").join(removed.canonical());
    assert!(removed_storage.is_dir());

    // The node answers while its reactor runs.
    assert_eq!(node.connected_peers_within(bound).expect("an answer"), 0);
    let seeders = node.seeders_within(writes.rid, bound).expect("an answer");
    assert!(node.waiting_on_node().is_empty());

    let dial = wedge(&node, &gate);

    // Each call returns at its bound, and is named as waiting, overdue.
    let asked = Instant::now();
    let peers = node.connected_peers_within(bound);
    assert!(deadline(&peers, "sessions", bound), "{peers:?}");
    let unseed = node.unseed_repo_within(rid, bound);
    assert!(deadline(&unseed, "unseed", bound), "{unseed:?}");
    let seeding = node.seeders_within(writes.rid, bound);
    assert!(deadline(&seeding, "seeds_for", bound), "{seeding:?}");
    assert!(!seeding.unwrap_err().is_announce());
    // A removal's unseed is sent, and not answered: nothing after it runs.
    let removal = node.remove_quarantined_within(removed, record.qid, &mut quarantine, bound);
    assert!(deadline(&removal, "unseed", bound), "{removal:?}");
    let took = asked.elapsed();
    assert!(took < SOON, "four calls took {took:?}");
    assert!(quarantine.get(&removed).unwrap().is_some());
    assert!(removed_storage.is_dir());
    assert_eq!(
        listed(&node),
        [
            ("sessions", true),
            ("unseed", true),
            ("seeds_for", true),
            ("unseed", true)
        ]
    );

    // Callers that all arrive at once, more than may wait: as many as there's
    // room for are sent, and wait to their bound; the rest aren't sent. A
    // call sent to the wedged reactor stays listed, and its thread waits, so
    // once every caller has its answer, the list holds all that were ever
    // sent: no more than the limit, nor more threads.
    let room = NODE_WAIT_LIMIT - node.waiting_on_node().len();
    let callers = NODE_WAIT_LIMIT + 4;
    let burst = Duration::from_secs(2);
    let start = Barrier::new(callers + 1);
    let answers = std::thread::scope(|scope| {
        let calls = (0..callers)
            .map(|_| {
                scope.spawn(|| {
                    start.wait();
                    let asked = Instant::now();
                    let answer = node.connected_peers_within(burst);
                    (answer, asked.elapsed())
                })
            })
            .collect::<Vec<_>>();
        start.wait();
        wait_for("full list of calls", SOON, || {
            node.waiting_on_node().len() == NODE_WAIT_LIMIT && helpers(&node) == NODE_WAIT_LIMIT
        });
        calls
            .into_iter()
            .map(|call| call.join().expect("a caller"))
            .collect::<Vec<_>>()
    });
    let sent = answers
        .iter()
        .filter(|(answer, took)| deadline(answer, "sessions", burst) && *took >= burst)
        .count();
    let refused = answers
        .iter()
        .filter(|(answer, _)| busy(answer, "sessions"))
        .count();
    assert_eq!((sent, refused), (room, callers - room), "{answers:?}");
    let waiting = node.waiting_on_node();
    assert_eq!(waiting.len(), NODE_WAIT_LIMIT, "{waiting:?}");
    assert!(waiting.iter().all(|wait| wait.overdue), "{waiting:?}");
    assert_eq!(helpers(&node), NODE_WAIT_LIMIT);

    // Every place is taken, by calls past their bounds: no call is sent.
    let before = stands(&node, &writes);
    let peers = node.connected_peers_within(SOON);
    assert!(busy(&peers, "sessions"), "{peers:?}");
    let seeding = node.seeders_within(writes.rid, SOON);
    assert!(busy(&seeding, "seeds_for"), "{seeding:?}");
    // A removal touches nothing.
    let removal = node.remove_quarantined_within(removed, record.qid, &mut quarantine, SOON);
    assert!(busy(&removal, "unseed"), "{removal:?}");
    assert!(quarantine.get(&removed).unwrap().is_some());
    assert!(removed_storage.is_dir());
    // Each COB change stands, unannounced, and its error is the
    // announcement's.
    for answer in write_each(&node, &writes, "Unannounced", true, SOON) {
        assert!(busy(&answer, "announce_refs_for"), "{answer:?}");
        assert!(answer.unwrap_err().is_announce());
    }
    let after = stands(&node, &writes);
    assert_eq!(
        after,
        Stands {
            titles: vec!["Unannounced".into(), "Written".into()],
            comments: before.comments + 1,
            closed: true,
            patch_comments: before.patch_comments + 1,
        }
    );
    // A COB change that fails before its announcement isn't refused at the
    // cap, and its error isn't an announcement's.
    let refused = node.edit_issue_state_within(writes.rid, &writes.issue, "bogus", SOON);
    assert!(
        matches!(&refused, Err(error) if !error.is_announce() && !busy(&refused, "announce_refs_for")),
        "{refused:?}"
    );
    assert_eq!(stands(&node, &writes), after);
    assert_eq!(node.waiting_on_node().len(), NODE_WAIT_LIMIT);
    assert_eq!(helpers(&node), NODE_WAIT_LIMIT);

    // The reactor comes back: it answers the calls left waiting, which end,
    // and answers new calls again.
    gate.let_go();
    wait_for("end of the waiting calls", SOON, || {
        node.waiting_on_node().is_empty() && helpers(&node) == 0
    });
    assert_eq!(node.connected_peers_within(SOON).expect("an answer"), 0);
    assert!(!node.unseed_repo_within(rid, SOON).expect("an answer"));
    assert_eq!(
        node.seeders_within(writes.rid, SOON).expect("an answer"),
        seeders
    );
    // The removal, run again, completes.
    assert_eq!(
        node.remove_quarantined_within(removed, record.qid, &mut quarantine, SOON)
            .expect("a removal"),
        Removal::Removed
    );
    assert!(quarantine.get(&removed).unwrap().is_none());
    assert!(!removed_storage.exists());
    assert!(node.waiting_on_node().is_empty());
    dial.join()
        .expect("the dial's thread")
        .expect("a seed book report");

    node.shutdown().expect("clean shutdown");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_wedged_reactor_bounds_each_cob_changes_announcement() {
    let _turn = turn();
    let home = home("waits-cobs");
    let gate = Arc::new(Wedge::default());
    let mut node = start(&home, Arc::clone(&gate), RepoGate::new());
    let bound = Duration::from_millis(300);
    let writes = writes(&mut node, &home);
    let before = stands(&node, &writes);
    assert!(!before.closed);

    let dial = wedge(&node, &gate);

    // Each COB change is committed, then announced: the announcement is
    // sent, and returns at its bound. The change stands, and the error is
    // the announcement's.
    let asked = Instant::now();
    for answer in write_each(&node, &writes, "Wedged", true, bound) {
        assert!(deadline(&answer, "announce_refs_for", bound), "{answer:?}");
        assert!(answer.unwrap_err().is_announce());
    }
    let took = asked.elapsed();
    assert!(took < SOON, "four changes took {took:?}");
    let wedged = stands(&node, &writes);
    assert_eq!(
        wedged,
        Stands {
            titles: vec!["Wedged".into(), "Written".into()],
            comments: before.comments + 1,
            closed: true,
            patch_comments: before.patch_comments + 1,
        }
    );
    assert_eq!(listed(&node), [("announce_refs_for", true); 4]);
    // A change that fails before its announcement sends nothing: it
    // returns its own error, which isn't an announcement's.
    let refused = node.comment_patch_within(writes.rid, &writes.issue, "On no revision.", SOON);
    assert!(
        matches!(&refused, Err(error) if !error.is_announce()),
        "{refused:?}"
    );
    assert_eq!(stands(&node, &writes), wedged);
    assert_eq!(node.waiting_on_node().len(), 4);

    // The reactor comes back: the announcements left waiting end, and each
    // change is announced again.
    gate.let_go();
    wait_for("end of the waiting announcements", SOON, || {
        node.waiting_on_node().is_empty() && helpers(&node) == 0
    });
    for answer in write_each(&node, &writes, "Announced", false, SOON) {
        answer.expect("a change, announced");
    }
    let announced = stands(&node, &writes);
    assert_eq!(announced.comments, before.comments + 2);
    assert!(!announced.closed);
    assert_eq!(announced.patch_comments, before.patch_comments + 2);
    assert!(node.waiting_on_node().is_empty());
    dial.join()
        .expect("the dial's thread")
        .expect("a seed book report");

    node.shutdown().expect("clean shutdown");
    let _ = std::fs::remove_dir_all(&home);
}
