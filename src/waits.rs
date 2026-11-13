//! Calls that wait on the node's reactor for an answer, each within a bound.
//!
//! heartwood's handle sends a command to the reactor and waits for its answer
//! with no bound (`Handle::sessions`, `Handle::unseed`, ...). A reactor that
//! never answers would hold the caller, and any lock the caller holds, for
//! good. So each such call here runs on a thread of its own, and its caller
//! waits for the answer up to a bound: past it, the caller has
//! [`Error::NodeDeadline`], and the thread goes on waiting in the background
//! until the reactor answers, or ends, and so drops the command.
//!
//! The calls still waiting are listed, by name, for a host that stops the node
//! to report ([`Waits::waiting`]). Calls left waiting past their bounds are
//! capped: at [`NODE_OVERDUE_LIMIT`], a new call isn't sent, and its caller
//! has [`Error::NodeOverdue`] at once, so that a wedged reactor doesn't
//! gather a thread for every call made of it.

use std::sync::{mpsc, Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use radicle_node::runtime::handle::{Error as HandleError, Handle as NodeHandle};

use crate::Error;

/// How long [`crate::Embedded::connected_peers`] and
/// [`crate::Embedded::unseed_repo`] wait for the node's answer. A caller with
/// a bound of its own passes it to their `_within` forms.
pub const NODE_ANSWER_WITHIN: Duration = Duration::from_secs(30);

/// How many calls left waiting on the node past their bounds stop a new call
/// from being sent ([`Error::NodeOverdue`]).
pub const NODE_OVERDUE_LIMIT: usize = 8;

/// A call waiting on the node's reactor ([`crate::Embedded::waiting_on_node`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeWait {
    /// The call's name: `sessions` or `unseed`.
    pub call: &'static str,
    /// How long it has waited.
    pub waited: Duration,
    /// Whether its caller stopped waiting for it at its bound.
    pub overdue: bool,
}

/// The calls of one node waiting on its reactor.
#[derive(Clone, Default)]
pub(crate) struct Waits(Arc<Mutex<Inner>>);

#[derive(Default)]
struct Inner {
    next: u64,
    calls: Vec<Call>,
}

struct Call {
    id: u64,
    name: &'static str,
    since: Instant,
    overdue: bool,
}

/// A call's place in [`Waits`], given up when the call ends, however it ends.
struct Entry {
    waits: Waits,
    id: u64,
}

impl Drop for Entry {
    fn drop(&mut self) {
        self.waits.lock().calls.retain(|call| call.id != self.id);
    }
}

impl Waits {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The calls waiting on the reactor, oldest first.
    pub(crate) fn waiting(&self) -> Vec<NodeWait> {
        self.lock()
            .calls
            .iter()
            .map(|call| NodeWait {
                call: call.name,
                waited: call.since.elapsed(),
                overdue: call.overdue,
            })
            .collect()
    }

    /// Runs `ask`, which waits on the reactor through `handle`, on a thread of
    /// its own, and waits for its answer up to `within`.
    pub(crate) fn call<T, F>(
        &self,
        handle: &NodeHandle,
        name: &'static str,
        within: Duration,
        ask: F,
    ) -> Result<T, Error>
    where
        T: Send + 'static,
        F: FnOnce(NodeHandle) -> Result<T, HandleError> + Send + 'static,
    {
        let entry = {
            let mut inner = self.lock();
            let overdue = inner.calls.iter().filter(|call| call.overdue).count();
            if overdue >= NODE_OVERDUE_LIMIT {
                return Err(Error::NodeOverdue {
                    call: name,
                    overdue,
                });
            }
            inner.next += 1;
            let id = inner.next;
            inner.calls.push(Call {
                id,
                name,
                since: Instant::now(),
                overdue: false,
            });
            Entry {
                waits: self.clone(),
                id,
            }
        };
        let id = entry.id;
        let (send, answer) = mpsc::sync_channel(1);
        let handle = handle.clone();
        // A failed spawn drops the closure, and the entry with it; so does a
        // panic. The entry goes before the answer does, so that a caller
        // with its answer doesn't find the call still waiting.
        std::thread::Builder::new()
            .name("radicle-wait".into())
            .spawn(move || {
                let answer = ask(handle);
                drop(entry);
                send.send(answer).ok();
            })?;
        match answer.recv_timeout(within) {
            Ok(answer) => Ok(answer?),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(call) = self.lock().calls.iter_mut().find(|call| call.id == id) {
                    call.overdue = true;
                }
                Err(Error::NodeDeadline { call: name, within })
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(Error::NodeThread(format!(
                "the thread waiting on the node's answer to {name} ended without it"
            ))),
        }
    }
}
