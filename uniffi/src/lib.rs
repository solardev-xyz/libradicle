//! UniFFI (Swift / iOS) binding over `libradicle`.
//!
//! This mirrors the napi desktop binding one-to-one: every export returns a
//! JSON string with the exact shapes the Freedom `window.radicle` provider
//! already parses, so the iOS webview consumes the same payloads as desktop.
//! It follows the Myotis pattern — plain `#[uniffi::export]` free functions,
//! `uniffi::setup_scaffolding!()` at the crate root — with one difference from
//! napi: calls are **synchronous** (UniFFI has no libuv thread pool), so the
//! Swift side dispatches them off the main thread. Progress is delivered via a
//! foreign callback interface instead of a napi ThreadsafeFunction.
//!
//! One embedded node per process, held in a global slot, exactly as on
//! desktop: the app owns a single Radicle profile at a time.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use libradicle::{CancelToken, Embedded, FetchPolicy, Network, Options, Progress};

uniffi::setup_scaffolding!();

static NODE: Mutex<Option<Embedded>> = Mutex::new(None);

/// In-flight clone cancellation tokens, keyed by full RID.
static CANCELS: LazyLock<Mutex<HashMap<String, CancelToken>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Foreign progress sink for `cloneRepoWithProgress`. Swift implements this;
/// it receives one JSON object per event (`{phase, ...}`).
#[uniffi::export(callback_interface)]
pub trait ProgressListener: Send + Sync {
    fn on_progress(&self, event: String);
}

fn err_json(e: impl std::fmt::Display) -> String {
    serde_json::json!({ "error": e.to_string() }).to_string()
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
        } => json!({ "nid": nid, "addr": addr, "index": index, "total": total }),
        Progress::Fetching { nid, index, total } => {
            json!({ "nid": nid, "index": index, "total": total })
        }
        Progress::PeerFailed {
            nid,
            index,
            total,
            reason,
        } => json!({ "nid": nid, "index": index, "total": total, "reason": reason }),
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

fn issue_json(id: &radicle::cob::ObjectId, issue: &radicle::cob::issue::Issue) -> serde_json::Value {
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

fn patch_json(id: &radicle::cob::ObjectId, patch: &radicle::cob::patch::Patch) -> serde_json::Value {
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

/// Start the embedded node with a profile at `home`. `{"did": "..."}` on success.
#[uniffi::export]
pub fn start(home: String, alias: String) -> String {
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
}

/// Connect to the profile's preferred seeds. `{"connected": n}`.
#[uniffi::export]
pub fn connect_seeds(timeout_ms: u32) -> String {
    let mut network = match network() {
        Ok(network) => network,
        Err(e) => return err_json(e),
    };
    match network.connect_preferred_seeds(Duration::from_millis(timeout_ms.into())) {
        Ok(n) => serde_json::json!({ "connected": n }).to_string(),
        Err(e) => err_json(e),
    }
}

/// Seed + fetch a repository into storage. `{"ok": true}`.
#[uniffi::export]
pub fn clone_repo(rid: String, timeout_ms: u32) -> String {
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
}

/// Seed + fetch a repository, pushing one JSON progress event per phase to
/// `on_progress`. `{"ok":true}`, `{"cancelled":true}`, or `{"error":...}`.
/// Cancel mid-flight with `cancelClone(rid)`.
#[uniffi::export]
pub fn clone_repo_with_progress(
    rid: String,
    timeout_ms: u32,
    on_progress: Box<dyn ProgressListener>,
) -> String {
    let parsed = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    let mut network = match network() {
        Ok(network) => network,
        Err(e) => return err_json(e),
    };

    let cancel = CancelToken::new();
    if let Ok(mut map) = CANCELS.lock() {
        map.insert(rid.clone(), cancel.clone());
    }

    let policy = FetchPolicy::from_timeout(Duration::from_millis(timeout_ms.into()));
    let mut emit = |p: Progress| on_progress.on_progress(progress_json(&p));
    let result = network.clone_repo_with(parsed, &policy, &cancel, &mut emit);

    if let Ok(mut map) = CANCELS.lock() {
        map.remove(&rid);
    }
    match result {
        Ok(()) => r#"{"ok":true}"#.to_string(),
        Err(libradicle::Error::Cancelled) => r#"{"cancelled":true}"#.to_string(),
        Err(e) => err_json(e),
    }
}

/// Request cancellation of an in-flight clone for `rid`. `{"cancelled": bool}`.
#[uniffi::export]
pub fn cancel_clone(rid: String) -> String {
    let found = CANCELS
        .lock()
        .ok()
        .and_then(|map| map.get(&rid).map(|t| t.cancel()))
        .is_some();
    serde_json::json!({ "cancelled": found }).to_string()
}

/// Stop seeding a repository. `{"unseeded": bool}`.
#[uniffi::export]
pub fn unseed_repo(rid: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(|node| match node.unseed_repo(rid) {
        Ok(unseeded) => serde_json::json!({ "unseeded": unseeded }).to_string(),
        Err(e) => err_json(e),
    })
}

/// The public profile identity used by the browser provider.
#[uniffi::export]
pub fn identity() -> String {
    with_node(|node| {
        let identity = node.identity();
        serde_json::json!({
            "did": identity.did,
            "nid": identity.nid,
            "alias": identity.alias,
        })
        .to_string()
    })
}

/// Repositories the local node is currently seeding.
#[uniffi::export]
pub fn list_repos() -> String {
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
}

/// Explicit allow-seeding policies, including repositories still awaiting a
/// successful first fetch.
#[uniffi::export]
pub fn list_seeded_repos() -> String {
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
}

/// All issues in a repository, in radicle-httpd-compatible JSON shape.
#[uniffi::export]
pub fn issues(rid: String) -> String {
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
}

/// One issue in radicle-httpd-compatible JSON shape.
#[uniffi::export]
pub fn issue(rid: String, issue_id: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(|node| match node.issue(rid, &issue_id) {
        Ok((id, issue)) => issue_json(&id, &issue).to_string(),
        Err(e) => err_json(e),
    })
}

/// All patches in a repository, in radicle-httpd-compatible JSON shape.
#[uniffi::export]
pub fn patches(rid: String) -> String {
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
}

/// One patch in radicle-httpd-compatible JSON shape.
#[uniffi::export]
pub fn patch(rid: String, patch_id: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(|node| match node.patch(rid, &patch_id) {
        Ok((id, patch)) => patch_json(&id, &patch).to_string(),
        Err(e) => err_json(e),
    })
}

/// Create an issue directly in the COB store. `labels_json` is a JSON string array.
#[uniffi::export]
pub fn create_issue(rid: String, title: String, description: String, labels_json: String) -> String {
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
}

/// Comment on an issue.
#[uniffi::export]
pub fn comment_issue(rid: String, issue_id: String, body: String, reply_to: Option<String>) -> String {
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
}

/// Transition an issue state.
#[uniffi::export]
pub fn edit_issue_state(rid: String, issue_id: String, state: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(|node| match node.edit_issue_state(rid, &issue_id, &state) {
        Ok(id) => serde_json::json!({ "id": id, "state": state }).to_string(),
        Err(e) => err_json(e),
    })
}

/// Comment on a patch revision.
#[uniffi::export]
pub fn comment_patch(rid: String, revision_id: String, body: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(|node| match node.comment_patch(rid, &revision_id, &body) {
        Ok(id) => serde_json::json!({ "id": id }).to_string(),
        Err(e) => err_json(e),
    })
}

/// Import a working Git repository into native Radicle storage. Desktop-only:
/// heartwood shells out to `git` to push a working copy, so this is unavailable
/// on a no-spawn build.
#[uniffi::export]
pub fn import_repo(path: String, name: String, description: String, default_branch: String) -> String {
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
}

/// Identity payload + head + COB counts, straight from storage.
#[uniffi::export]
pub fn repo_info(rid: String) -> String {
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
}

/// Paginated commit history reachable from a validated full commit id.
#[uniffi::export]
pub fn commits(rid: String, parent: String, page: u32, per_page: u32) -> String {
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
}

/// Commit metadata plus its structured first-parent diff.
#[uniffi::export]
pub fn commit(rid: String, revision: String) -> String {
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
}

/// File paths at the head of the default branch, as a JSON array.
#[uniffi::export]
pub fn list_files(rid: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(|node| match node.list_files(rid) {
        Ok(files) => serde_json::json!(files).to_string(),
        Err(e) => err_json(e),
    })
}

/// Tree entries at the head of the default branch, httpd-shaped.
#[uniffi::export]
pub fn tree(rid: String, path: String) -> String {
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
}

/// Tree entries at a validated full commit id, including commit metadata.
#[uniffi::export]
pub fn tree_at(rid: String, revision: String, path: String) -> String {
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
}

/// Blob at the head of the default branch, httpd-shaped. Binary blobs omit content.
#[uniffi::export]
pub fn blob(rid: String, path: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    let name = path.rsplit('/').next().unwrap_or(&path).to_string();
    with_node(|node| match node.read_blob(rid, &path) {
        Ok(b) if b.binary => serde_json::json!({ "binary": true, "name": name }).to_string(),
        Ok(b) => serde_json::json!({
            "binary": false,
            "name": name,
            "content": String::from_utf8_lossy(&b.content),
        })
        .to_string(),
        Err(e) => err_json(e),
    })
}

/// Blob at a validated full commit id.
#[uniffi::export]
pub fn blob_at(rid: String, revision: String, path: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    let name = path.rsplit('/').next().unwrap_or(&path).to_string();
    with_node(|node| match node.read_blob_at(rid, &revision, &path) {
        Ok(b) if b.binary => serde_json::json!({ "binary": true, "name": name }).to_string(),
        Ok(b) => serde_json::json!({
            "binary": false,
            "name": name,
            "content": String::from_utf8_lossy(&b.content),
        })
        .to_string(),
        Err(e) => err_json(e),
    })
}

/// Signed remotes and their branch heads.
#[uniffi::export]
pub fn remotes(rid: String) -> String {
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
}

/// Repository history statistics reachable from a validated full commit id.
#[uniffi::export]
pub fn repo_stats(rid: String, revision: String) -> String {
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
}

/// Node status: `{"connectedPeers": n}`.
#[uniffi::export]
pub fn status() -> String {
    with_node(|node| match node.connected_peers() {
        Ok(n) => serde_json::json!({ "connectedPeers": n }).to_string(),
        Err(e) => err_json(e),
    })
}

/// Number of known seeders for a repo: `{"seeding": n}`.
#[uniffi::export]
pub fn seeders(rid: String) -> String {
    let rid = match parse_rid(&rid) {
        Ok(r) => r,
        Err(e) => return err_json(e),
    };
    with_node(|node| match node.seeders(rid) {
        Ok(n) => serde_json::json!({ "seeding": n }).to_string(),
        Err(e) => err_json(e),
    })
}

/// Gracefully stop the node and join its thread. `{"ok": true}`.
#[uniffi::export]
pub fn shutdown() -> String {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exported surface is callable and returns the documented sentinels
    /// before a node is started (host-side smoke — device tests run on macOS).
    #[test]
    fn exports_return_not_started_sentinels() {
        assert!(identity().contains("node not started"));
        assert!(list_repos().contains("node not started"));
        assert!(status().contains("node not started"));
        assert!(shutdown().contains("node not started"));
        // Argument validation happens before the node check.
        assert!(clone_repo("not-a-rid".into(), 1000).contains("invalid RID"));
        // Cancelling an unknown clone is a well-formed negative.
        assert_eq!(cancel_clone("whatever".into()), r#"{"cancelled":false}"#);
    }

    #[test]
    fn progress_json_is_flat_with_phase() {
        let p = Progress::Connecting {
            nid: "z6Mk".into(),
            addr: "1.2.3.4:8776".into(),
            index: 1,
            total: 3,
        };
        let v: serde_json::Value = serde_json::from_str(&progress_json(&p)).unwrap();
        assert_eq!(v["phase"], "connecting");
        assert_eq!(v["nid"], "z6Mk");
        assert_eq!(v["total"], 3);
    }
}
