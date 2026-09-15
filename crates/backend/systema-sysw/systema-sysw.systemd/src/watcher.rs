//! Job completion watcher driving the bridge's blocking semantics.
//!
//! `Manager.StartUnit` / `StopUnit` / `RestartUnit` / `ReloadUnit` block
//! until the corresponding job reaches a terminal state, exactly like
//! System A's in-process D-Bus layer did.  Completion is delivered by the
//! control-port `job.completed` event performed by the mirror task; this
//! watcher resolves the waiting D-Bus calls.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::oneshot;

#[derive(Default)]
struct WatcherInner {
    outstanding: HashMap<u64, Vec<oneshot::Sender<String>>>,
    completed: HashMap<u64, String>,
}

#[derive(Default)]
pub struct JobWatcher {
    inner: parking_lot::Mutex<WatcherInner>,
}

impl JobWatcher {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Record job completion (called from the mirror event loop).
    pub fn notify(&self, job_id: u64, result: &str) {
        let mut inner = self.inner.lock();
        if let Some(waiters) = inner.outstanding.remove(&job_id) {
            for w in waiters {
                let _ = w.send(result.to_string());
            }
        } else {
            inner.completed.insert(job_id, result.to_string());
        }
    }

    /// Await a job's terminal result.
    ///
    /// Returns the result string ("done", "failed", "cancelled", "timeout",
    /// ...).  A job that finished before we subscribed resolves immediately
    /// from the completed-queue.  If the session dies mid-wait, this returns
    /// an error so the D-Bus call can surface a disconnect.
    pub async fn wait(&self, job_id: u64) -> anyhow::Result<String> {
        let rx = {
            let mut inner = self.inner.lock();
            if let Some(result) = inner.completed.remove(&job_id) {
                return Ok(result);
            }
            let (tx, rx) = oneshot::channel();
            inner.outstanding.entry(job_id).or_default().push(tx);
            rx
        };
        rx.await
            .map_err(|_| anyhow::anyhow!("control session closed while waiting for job {job_id}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wait_resolves_on_notify() {
        let watcher = JobWatcher::new();
        let waiter = watcher.clone();
        let handle = tokio::spawn(async move { waiter.wait(7).await.unwrap() });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        watcher.notify(7, "done");
        assert_eq!(&handle.await.unwrap(), "done");
    }

    #[tokio::test]
    async fn wait_returns_pre_completed() {
        let watcher = JobWatcher::new();
        watcher.notify(9, "failed");
        assert_eq!(watcher.wait(9).await.unwrap(), "failed");
    }

    #[tokio::test]
    async fn notify_before_wait_forwards_result() {
        let watcher = JobWatcher::new();
        watcher.notify(11, "done");
        let result = watcher.wait(11).await.unwrap();
        assert_eq!(result, "done");
    }
}