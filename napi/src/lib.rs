//! Node.js binding over `libradicle`, in the style of Freedom Browser's
//! myotis-node addon: every export returns a JSON string, blocking work
//! runs on the libuv thread pool via `AsyncTask`.
//!
//! One embedded node per process, held in a global slot. The Freedom main
//! process owns exactly one Radicle profile at a time, so a singleton is
//! the honest shape — a second `start` without `shutdown` is an error.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use napi::bindgen_prelude::AsyncTask;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi::{Env, Result, Task};
use napi_derive::napi;

use libradicle::{
    CancelToken, Embedded, FetchPolicy, Network, Options, Progress, SeedConnectReport,
};

static NODE: Mutex<Option<Embedded>> = Mutex::new(None);

/// In-flight clone cancellation tokens, keyed by full RID. Registered when
/// a progress clone starts and removed when it finishes; `cancel_clone`
/// flips the matching token.
static CANCELS: LazyLock<Mutex<HashMap<String, CancelToken>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// A JS `(event: string) => void` progress callback. Non-callee-handled:
/// JS receives the JSON string directly, no error tuple.
type ProgressCb = ThreadsafeFunction<String, (), String, napi::Status, false>;

fn err_json(e: impl std::fmt::Display) -> String {
    serde_json::json!({ "error": e.to_string() }).to_string()
}

fn seed_report_json(report: SeedConnectReport) -> String {
    serde_json::json!({
        "attempted": report.attempted,
        "connected": report.connected,
        "target": report.target,
        "targetReached": report.target_reached,
        "elapsedMs": report.elapsed.as_millis(),
        "failures": report.failures.into_iter().map(|failure| serde_json::json!({
            "nid": failure.nid,
            "addr": failure.addr,
            "reason": failure.reason,
        })).collect::<Vec<_>>(),
    })
    .to_string()
}

/// One blocking call scheduled on the libuv thread pool (myotis pattern).
pub struct BlockingJson {
    run: Option<Box<dyn FnOnce() -> String + Send>>,
}

impl Task for BlockingJson {
    type Output = String;
    type JsValue = String;

    fn compute(&mut self) -> Result<Self::Output> {
        match self.run.take() {
            Some(f) => Ok(f()),
            None => Ok(r#"{"error":"task already ran"}"#.to_string()),
        }
    }

    fn resolve(&mut self, _env: Env, output: String) -> Result<String> {
        Ok(output)
    }
}

fn blocking(f: impl FnOnce() -> String + Send + 'static) -> AsyncTask<BlockingJson> {
    AsyncTask::new(BlockingJson {
        run: Some(Box::new(f)),
    })
}

fn with_node(f: impl FnOnce(&mut Embedded) -> String) -> String {
    match NODE.lock() {
        Ok(mut guard) => match guard.as_mut() {
            Some(node) => f(node),
            None => err_json("node not started"),
        },
        Err(_) => err_json("node lock poisoned"),
    }
}

fn network() -> std::result::Result<Network, String> {
    match NODE.lock() {
        Ok(guard) => guard
            .as_ref()
            .map(Embedded::network)
            .ok_or_else(|| "node not started".to_string()),
        Err(_) => Err("node lock poisoned".to_string()),
    }
}

fn parse_rid(rid: &str) -> std::result::Result<radicle::identity::RepoId, String> {
    rid.parse().map_err(|e| format!("invalid RID {rid:?}: {e}"))
}

/// Render a [`Progress`] event as a flat `{ "phase": ..., ... }` JSON string.
fn progress_json(p: &Progress) -> String {
    use serde_json::json;
    let mut v = match p {
        Progress::Resolving { candidates } => json!({ "candidates": candidates }),
        Progress::Connecting {
            nid,
            addr,
            index,
            total,
        } => {
            json!({ "nid": nid, "addr": addr, "index": index, "total": total })
        }
        Progress::Fetching { nid, index, total } => {
            json!({ "nid": nid, "index": index, "total": total })
        }
        Progress::PeerFailed {
            nid,
            index,
            total,
            reason,
        } => {
            json!({ "nid": nid, "index": index, "total": total, "reason": reason })
        }
        Progress::Failed { reason } => json!({ "reason": reason }),
        Progress::Done | Progress::Cancelled => json!({}),
    };
    v["phase"] = serde_json::Value::from(p.phase());
    v.to_string()
}

fn comment_json<L>(
    id: &radicle::cob::EntryId,
    comment: &radicle::cob::thread::Comment<L>,
) -> serde_json::Value {
    serde_json::json!({
        "id": id.to_string(),
        "author": radicle::cob::common::Author::new(comment.author()),
        "body": comment.body(),
        "timestamp": comment.timestamp(),
        "replyTo": comment.reply_to().map(|id| id.to_string()),
    })
}

fn issue_json(
    id: &radicle::cob::ObjectId,
    issue: &radicle::cob::issue::Issue,
) -> serde_json::Value {
    serde_json::json!({
        "id": id.to_string(),
        "author": issue.author(),
        "title": issue.title(),
        "state": issue.state(),
        "labels": issue.labels().map(ToString::to_string).collect::<Vec<_>>(),
        "assignees": issue.assignees().map(ToString::to_string).collect::<Vec<_>>(),
        "discussion": issue.comments().map(|(id, comment)| comment_json(id, comment)).collect::<Vec<_>>(),
    })
}

fn patch_json(
    id: &radicle::cob::ObjectId,
    patch: &radicle::cob::patch::Patch,
) -> serde_json::Value {
    let revisions = patch
        .revisions()
        .map(|(id, revision)| {
            serde_json::json!({
                "id": id.to_string(),
                "author": revision.author(),
                "description": revision.description(),
                "base": revision.base().to_string(),
                "oid": revision.head().to_string(),
                "timestamp": revision.timestamp(),
                "discussions": revision.discussion().comments()
                    .map(|(id, comment)| comment_json(id, comment))
                    .collect::<Vec<_>>(),
                "reviews": revision.reviews().map(|(_, review)| serde_json::json!({
                    "id": review.id().to_string(),
                    "author": review.author(),
                    "verdict": review.verdict().map(|verdict| verdict.to_string()),
                    "summary": review.summary(),
                    "timestamp": review.timestamp(),
                })).collect::<Vec<_>>(),
            })
        })
        .collect::<Vec<_>>();
    let merges = patch
        .merges()
        .map(|(author, merge)| {
            serde_json::json!({
                "author": radicle::cob::common::Author::new(author),
                "revision": merge.revision.to_string(),
                "commit": merge.commit.to_string(),
                "timestamp": merge.timestamp,
            })
        })
        .collect::<Vec<_>>();

    serde_json::json!({
        "id": id.to_string(),
        "author": patch.author(),
        "title": patch.title(),
        "state": patch.state(),
        "target": patch.target(),
        "labels": patch.labels().map(ToString::to_string).collect::<Vec<_>>(),
        "assignees": patch.assignees().map(|did| did.to_string()).collect::<Vec<_>>(),
        "revisions": revisions,
        "merges": merges,
    })
}

/// Start the embedded node with a profile at `home`. Resolves to
/// `{"did": "..."}` once the node thread is up.
#[napi]
pub fn start(home: String, alias: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let mut guard = match NODE.lock() {
            Ok(g) => g,
            Err(_) => return err_json("node lock poisoned"),
        };
        if guard.is_some() {
            return err_json("node already started");
        }
        match Embedded::start(Options {
            home: home.into(),
            alias,
            listen: vec![],
        }) {
            Ok(node) => {
                let did = node.did();
                *guard = Some(node);
                serde_json::json!({ "did": did }).to_string()
            }
            Err(e) => err_json(e),
        }
    })
}

/// Concurrently bootstrap from the effective seed book. The response retains
/// `connected` and adds attempt/readiness diagnostics.
#[napi]
pub fn connect_seeds(timeout_ms: u32) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let mut network = match network() {
            Ok(network) => network,
            Err(e) => return err_json(e),
        };
        match network.connect_seed_book(Duration::from_millis(timeout_ms.into())) {
            Ok(report) => seed_report_json(report),
            Err(e) => err_json(e),
        }
    })
}

/// Seed + fetch a repository into storage. Resolves to `{"ok": true}`.
#[napi]
pub fn clone_repo(rid: String, timeout_ms: u32) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        let mut network = match network() {
            Ok(network) => network,
            Err(e) => return err_json(e),
        };
        match network.clone_repo(rid, Duration::from_millis(timeout_ms.into())) {
            Ok(()) => r#"{"ok":true}"#.to_string(),
            Err(e) => err_json(e),
        }
    })
}

/// Seed + fetch a repository, pushing progress events to `on_progress`
/// (a `(event: string) => void` callback receiving one JSON object per
/// event: `{phase, ...}`). Resolves to `{"ok":true}`, `{"cancelled":true}`,
/// or `{"error":...}`. Cancel mid-flight with `cancelClone(rid)`.
#[napi]
pub fn clone_repo_with_progress(
    rid: String,
    timeout_ms: u32,
    on_progress: ProgressCb,
) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let parsed = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        let mut network = match network() {
            Ok(network) => network,
            Err(e) => return err_json(e),
        };

        // Register a cancel token under the RID for the duration.
        let cancel = CancelToken::new();
        if let Ok(mut map) = CANCELS.lock() {
            map.insert(rid.clone(), cancel.clone());
        }

        let policy = FetchPolicy::from_timeout(Duration::from_millis(timeout_ms.into()));
        let mut emit = |p: Progress| {
            on_progress.call(progress_json(&p), ThreadsafeFunctionCallMode::NonBlocking);
        };
        let result = network.clone_repo_with(parsed, &policy, &cancel, &mut emit);

        if let Ok(mut map) = CANCELS.lock() {
            map.remove(&rid);
        }
        match result {
            Ok(()) => r#"{"ok":true}"#.to_string(),
            Err(libradicle::Error::Cancelled) => r#"{"cancelled":true}"#.to_string(),
            Err(e) => err_json(e),
        }
    })
}

/// Request cancellation of an in-flight `cloneRepoWithProgress` for `rid`.
/// Returns `{"cancelled": bool}` — false if no clone was running.
#[napi]
pub fn cancel_clone(rid: String) -> String {
    let found = CANCELS
        .lock()
        .ok()
        .and_then(|map| map.get(&rid).map(|t| t.cancel()))
        .is_some();
    serde_json::json!({ "cancelled": found }).to_string()
}

/// Stop seeding a repository. Resolves to `{"unseeded": bool}`.
#[napi]
pub fn unseed_repo(rid: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(|node| match node.unseed_repo(rid) {
            Ok(unseeded) => serde_json::json!({ "unseeded": unseeded }).to_string(),
            Err(e) => err_json(e),
        })
    })
}

/// The public profile identity used by the browser provider.
#[napi]
pub fn identity() -> AsyncTask<BlockingJson> {
    blocking(|| {
        with_node(|node| {
            let identity = node.identity();
            serde_json::json!({
                "did": identity.did,
                "nid": identity.nid,
                "alias": identity.alias,
            })
            .to_string()
        })
    })
}

/// Repositories the local node is currently seeding.
#[napi]
pub fn list_repos() -> AsyncTask<BlockingJson> {
    blocking(|| {
        with_node(|node| match node.list_repos() {
            Ok(repos) => serde_json::json!(repos
                .into_iter()
                .map(|repo| serde_json::json!({
                    "rid": repo.rid.to_string(),
                    "name": repo.name,
                    "description": repo.description,
                }))
                .collect::<Vec<_>>())
            .to_string(),
            Err(e) => err_json(e),
        })
    })
}

/// Explicit allow-seeding policies, including repositories still awaiting a
/// successful first fetch.
#[napi]
pub fn list_seeded_repos() -> AsyncTask<BlockingJson> {
    blocking(|| {
        with_node(|node| match node.list_seeded_repos() {
            Ok(repos) => serde_json::json!(repos
                .into_iter()
                .map(|repo| serde_json::json!({
                    "rid": repo.rid.to_string(),
                    "name": repo.name,
                    "description": repo.description,
                }))
                .collect::<Vec<_>>())
            .to_string(),
            Err(e) => err_json(e),
        })
    })
}

/// All issues in a repository, in radicle-httpd-compatible JSON shape.
#[napi]
pub fn issues(rid: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(|node| match node.issues(rid) {
            Ok(issues) => serde_json::json!(issues
                .iter()
                .map(|(id, issue)| issue_json(id, issue))
                .collect::<Vec<_>>())
            .to_string(),
            Err(e) => err_json(e),
        })
    })
}

/// One issue in radicle-httpd-compatible JSON shape.
#[napi]
pub fn issue(rid: String, issue_id: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(|node| match node.issue(rid, &issue_id) {
            Ok((id, issue)) => issue_json(&id, &issue).to_string(),
            Err(e) => err_json(e),
        })
    })
}

/// All patches in a repository, in radicle-httpd-compatible JSON shape.
#[napi]
pub fn patches(rid: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(|node| match node.patches(rid) {
            Ok(patches) => serde_json::json!(patches
                .iter()
                .map(|(id, patch)| patch_json(id, patch))
                .collect::<Vec<_>>())
            .to_string(),
            Err(e) => err_json(e),
        })
    })
}

/// One patch in radicle-httpd-compatible JSON shape.
#[napi]
pub fn patch(rid: String, patch_id: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(|node| match node.patch(rid, &patch_id) {
            Ok((id, patch)) => patch_json(&id, &patch).to_string(),
            Err(e) => err_json(e),
        })
    })
}

/// Create an issue directly in the COB store.
#[napi]
pub fn create_issue(
    rid: String,
    title: String,
    description: String,
    labels_json: String,
) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        let labels = match serde_json::from_str::<Vec<String>>(&labels_json) {
            Ok(labels) => labels,
            Err(e) => return err_json(format!("invalid labels: {e}")),
        };
        with_node(
            |node| match node.create_issue(rid, &title, &description, labels) {
                Ok(id) => serde_json::json!({ "id": id }).to_string(),
                Err(e) => err_json(e),
            },
        )
    })
}

/// Comment on an issue.
#[napi]
pub fn comment_issue(
    rid: String,
    issue_id: String,
    body: String,
    reply_to: Option<String>,
) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(
            |node| match node.comment_issue(rid, &issue_id, &body, reply_to.as_deref()) {
                Ok(id) => serde_json::json!({ "id": id }).to_string(),
                Err(e) => err_json(e),
            },
        )
    })
}

/// Transition an issue state.
#[napi]
pub fn edit_issue_state(rid: String, issue_id: String, state: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(|node| match node.edit_issue_state(rid, &issue_id, &state) {
            Ok(id) => serde_json::json!({ "id": id, "state": state }).to_string(),
            Err(e) => err_json(e),
        })
    })
}

/// Comment on a patch revision.
#[napi]
pub fn comment_patch(rid: String, revision_id: String, body: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(|node| match node.comment_patch(rid, &revision_id, &body) {
            Ok(id) => serde_json::json!({ "id": id }).to_string(),
            Err(e) => err_json(e),
        })
    })
}

/// Import a working Git repository into native Radicle storage.
#[napi]
pub fn import_repo(
    path: String,
    name: String,
    description: String,
    default_branch: String,
) -> AsyncTask<BlockingJson> {
    blocking(move || {
        with_node(|node| {
            match node.import_repo(
                std::path::Path::new(&path),
                &name,
                &description,
                &default_branch,
            ) {
                Ok(rid) => serde_json::json!({ "rid": rid.to_string() }).to_string(),
                Err(e) => err_json(e),
            }
        })
    })
}

/// Identity payload + head + COB counts, straight from storage.
#[napi]
pub fn repo_info(rid: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(|node| match node.repo_info(rid) {
            Ok(info) => serde_json::json!({
                "rid": info.rid.to_string(),
                "name": info.name,
                "description": info.description,
                "defaultBranch": info.default_branch,
                "head": info.head,
                "delegates": info.delegates,
                "threshold": info.threshold,
                "visibility": info.visibility,
                "issuesOpen": info.issues_open,
                "patchesOpen": info.patches_open,
            })
            .to_string(),
            Err(e) => err_json(e),
        })
    })
}

/// Paginated commit history reachable from a validated full commit id.
#[napi]
pub fn commits(rid: String, parent: String, page: u32, per_page: u32) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(
            |node| match node.commits(rid, &parent, page as usize, per_page as usize) {
                Ok(commits) => match serde_json::to_string(&commits) {
                    Ok(json) => json,
                    Err(e) => err_json(e),
                },
                Err(e) => err_json(e),
            },
        )
    })
}

/// Commit metadata plus its structured first-parent diff.
#[napi]
pub fn commit(rid: String, revision: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(|node| match node.commit(rid, &revision) {
            Ok(detail) => serde_json::json!({
                "commit": detail.commit,
                "diff": detail.diff,
            })
            .to_string(),
            Err(e) => err_json(e),
        })
    })
}

/// File paths at the head of the default branch, as a JSON array.
#[napi]
pub fn list_files(rid: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(|node| match node.list_files(rid) {
            Ok(files) => serde_json::json!(files).to_string(),
            Err(e) => err_json(e),
        })
    })
}

/// Tree entries at the head of the default branch, httpd-shaped:
/// `{"entries":[{"name","path","kind"}]}`.
#[napi]
pub fn tree(rid: String, path: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(|node| match node.tree(rid, &path) {
            Ok(entries) => serde_json::json!({
                "entries": entries.iter().map(|e| serde_json::json!({
                    "name": e.name,
                    "path": e.path,
                    "kind": e.kind,
                })).collect::<Vec<_>>(),
            })
            .to_string(),
            Err(e) => err_json(e),
        })
    })
}

/// Tree entries at a validated full commit id, including commit metadata.
#[napi]
pub fn tree_at(rid: String, revision: String, path: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(|node| match node.tree_at(rid, &revision, &path) {
            Ok((entries, commit)) => serde_json::json!({
                "entries": entries.iter().map(|e| serde_json::json!({
                    "name": e.name,
                    "path": e.path,
                    "kind": e.kind,
                })).collect::<Vec<_>>(),
                "lastCommit": {
                    "id": commit.id,
                    "summary": commit.summary,
                    "description": commit.description,
                    "author": {
                        "name": commit.author.name,
                        "email": commit.author.email,
                        "time": commit.author.time,
                    },
                    "committer": {
                        "name": commit.committer.name,
                        "email": commit.committer.email,
                        "time": commit.committer.time,
                    },
                },
            })
            .to_string(),
            Err(e) => err_json(e),
        })
    })
}

/// Blob at the head of the default branch, httpd-shaped:
/// `{"binary":bool,"content":"...","name":"..."}`. Binary blobs omit
/// content (the viewer renders a placeholder for them).
#[napi]
pub fn blob(rid: String, path: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        with_node(|node| match node.read_blob(rid, &path) {
            Ok(b) if b.binary => serde_json::json!({
                "binary": true,
                "name": name,
            })
            .to_string(),
            Ok(b) => serde_json::json!({
                "binary": false,
                "name": name,
                "content": String::from_utf8_lossy(&b.content),
            })
            .to_string(),
            Err(e) => err_json(e),
        })
    })
}

/// Blob at a validated full commit id.
#[napi]
pub fn blob_at(rid: String, revision: String, path: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        with_node(|node| match node.read_blob_at(rid, &revision, &path) {
            Ok(b) if b.binary => serde_json::json!({
                "binary": true,
                "name": name,
            })
            .to_string(),
            Ok(b) => serde_json::json!({
                "binary": false,
                "name": name,
                "content": String::from_utf8_lossy(&b.content),
            })
            .to_string(),
            Err(e) => err_json(e),
        })
    })
}

/// Signed remotes and their branch heads.
#[napi]
pub fn remotes(rid: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(|node| match node.remotes(rid) {
            Ok(remotes) => serde_json::json!(remotes
                .iter()
                .map(|remote| serde_json::json!({
                    "id": remote.id,
                    "did": remote.did,
                    "delegate": remote.delegate,
                    "heads": remote.heads,
                }))
                .collect::<Vec<_>>())
            .to_string(),
            Err(e) => err_json(e),
        })
    })
}

/// Repository history statistics reachable from a validated full commit id.
#[napi]
pub fn repo_stats(rid: String, revision: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(|node| match node.repo_stats(rid, &revision) {
            Ok(stats) => serde_json::json!({
                "commits": stats.commits,
                "branches": stats.branches,
                "contributors": stats.contributors,
            })
            .to_string(),
            Err(e) => err_json(e),
        })
    })
}

/// Node status: `{"connectedPeers": n}`.
#[napi]
pub fn status() -> AsyncTask<BlockingJson> {
    blocking(|| {
        with_node(|node| match node.connected_peers() {
            Ok(n) => serde_json::json!({ "connectedPeers": n }).to_string(),
            Err(e) => err_json(e),
        })
    })
}

/// Number of known seeders for a repo: `{"seeding": n}`.
#[napi]
pub fn seeders(rid: String) -> AsyncTask<BlockingJson> {
    blocking(move || {
        let rid = match parse_rid(&rid) {
            Ok(r) => r,
            Err(e) => return err_json(e),
        };
        with_node(|node| match node.seeders(rid) {
            Ok(n) => serde_json::json!({ "seeding": n }).to_string(),
            Err(e) => err_json(e),
        })
    })
}

/// Gracefully stop the node and join its thread. Resolves to `{"ok": true}`.
#[napi]
pub fn shutdown() -> AsyncTask<BlockingJson> {
    blocking(|| {
        let node = match NODE.lock() {
            Ok(mut guard) => guard.take(),
            Err(_) => return err_json("node lock poisoned"),
        };
        match node {
            Some(node) => match node.shutdown() {
                Ok(()) => r#"{"ok":true}"#.to_string(),
                Err(e) => err_json(e),
            },
            None => err_json("node not started"),
        }
    })
}
