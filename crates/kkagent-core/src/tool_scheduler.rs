//! Parallel tool scheduler with resource-conflict gating (kimi ToolScheduler).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures::FutureExt;
use kkagent_tools::accesses::{tool_accesses, ToolAccesses};
use tokio::sync::{mpsc, oneshot, Mutex};

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

pub struct ToolCallTask<R> {
    pub tool_call_id: String,
    pub tool_name: String,
    pub accesses: ToolAccesses,
    pub start: Box<dyn FnOnce() -> BoxFuture<R> + Send>,
}

/// Status updates so the UI can distinguish running vs queued tools.
#[derive(Debug, Clone)]
pub enum SchedulerStatus {
    /// Task is waiting because it conflicts with an already-active/queued tool.
    Queued {
        tool_call_id: String,
        /// Human-readable name of the tool currently blocking this one.
        behind: String,
    },
    /// Task has started executing.
    Started { tool_call_id: String },
}

struct ScheduledTask<R> {
    id: u64,
    tool_call_id: String,
    tool_name: String,
    accesses: ToolAccesses,
    start: Option<Box<dyn FnOnce() -> BoxFuture<R> + Send>>,
    result_tx: Option<oneshot::Sender<Result<R, String>>>,
}

struct ActiveSlot {
    id: u64,
    tool_name: String,
    accesses: ToolAccesses,
}

struct SchedulerState<R: Send + 'static> {
    next_id: u64,
    active: Vec<ActiveSlot>,
    queued: Vec<ScheduledTask<R>>,
}

/// Conflict-aware concurrent scheduler for one model step's tool calls.
pub struct ToolScheduler<R: Send + 'static> {
    inner: Arc<Mutex<SchedulerState<R>>>,
    status_tx: Option<mpsc::UnboundedSender<SchedulerStatus>>,
}

impl<R: Send + 'static> ToolScheduler<R> {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(SchedulerState {
                next_id: 1,
                active: Vec::new(),
                queued: Vec::new(),
            })),
            status_tx: None,
        }
    }

    pub fn with_status(status_tx: mpsc::UnboundedSender<SchedulerStatus>) -> Self {
        let mut s = Self::new();
        s.status_tx = Some(status_tx);
        s
    }

    fn notify(&self, status: SchedulerStatus) {
        if let Some(tx) = &self.status_tx {
            let _ = tx.send(status);
        }
    }

    pub async fn add(&self, task: ToolCallTask<R>) -> Result<R, String> {
        let (tx, rx) = oneshot::channel();
        let scheduled = {
            let mut state = self.inner.lock().await;
            let id = state.next_id;
            state.next_id += 1;
            ScheduledTask {
                id,
                tool_call_id: task.tool_call_id,
                tool_name: task.tool_name,
                accesses: task.accesses,
                start: Some(task.start),
                result_tx: Some(tx),
            }
        };

        {
            let mut state = self.inner.lock().await;
            if let Some(behind) =
                Self::blocked_by(&scheduled.accesses, &state.active, &state.queued)
            {
                self.notify(SchedulerStatus::Queued {
                    tool_call_id: scheduled.tool_call_id.clone(),
                    behind,
                });
                state.queued.push(scheduled);
            } else {
                Self::start_locked(self, &mut state, scheduled);
            }
        }

        rx.await
            .map_err(|_| "tool scheduler result channel closed".to_string())?
    }

    /// Schedule many tasks and wait for all results in the original order.
    pub async fn run_all(tasks: Vec<ToolCallTask<R>>) -> Vec<Result<R, String>> {
        Self::run_all_with_status(tasks, None).await
    }

    pub async fn run_all_with_status(
        tasks: Vec<ToolCallTask<R>>,
        status_tx: Option<mpsc::UnboundedSender<SchedulerStatus>>,
    ) -> Vec<Result<R, String>> {
        let scheduler = match status_tx {
            Some(tx) => Self::with_status(tx),
            None => Self::new(),
        };
        let mut handles = Vec::with_capacity(tasks.len());
        for task in tasks {
            let sched = scheduler.clone();
            handles.push(tokio::spawn(async move { sched.add(task).await }));
        }
        let mut results = Vec::with_capacity(handles.len());
        for h in handles {
            results.push(match h.await {
                Ok(result) => result,
                Err(error) => Err(format!("tool scheduler task failed: {error}")),
            });
        }
        results
    }

    fn blocked_by(
        accesses: &ToolAccesses,
        active: &[ActiveSlot],
        queued_before: &[ScheduledTask<R>],
    ) -> Option<String> {
        if let Some(a) = active
            .iter()
            .find(|a| tool_accesses::conflict(accesses, &a.accesses))
        {
            return Some(a.tool_name.clone());
        }
        if let Some(q) = queued_before
            .iter()
            .find(|q| tool_accesses::conflict(accesses, &q.accesses))
        {
            return Some(q.tool_name.clone());
        }
        None
    }

    fn is_blocked(
        accesses: &ToolAccesses,
        active: &[ActiveSlot],
        queued_before: &[ScheduledTask<R>],
    ) -> bool {
        Self::blocked_by(accesses, active, queued_before).is_some()
    }

    fn start_locked(
        scheduler: &ToolScheduler<R>,
        state: &mut SchedulerState<R>,
        mut task: ScheduledTask<R>,
    ) {
        let id = task.id;
        let tool_call_id = task.tool_call_id.clone();
        let tool_name = task.tool_name.clone();
        let accesses = task.accesses.clone();
        state.active.push(ActiveSlot {
            id,
            tool_name,
            accesses: accesses.clone(),
        });
        scheduler.notify(SchedulerStatus::Started {
            tool_call_id: tool_call_id.clone(),
        });
        let start = task.start.take().expect("start fn");
        let result_tx = task.result_tx.take().expect("result tx");
        let inner = Arc::clone(&scheduler.inner);
        let status_tx = scheduler.status_tx.clone();
        tokio::spawn(async move {
            let result = std::panic::AssertUnwindSafe(start())
                .catch_unwind()
                .await
                .map_err(panic_message);
            let _ = result_tx.send(result);
            let mut state = inner.lock().await;
            if let Some(idx) = state.active.iter().position(|a| a.id == id) {
                state.active.remove(idx);
            }
            // Reconstruct a thin scheduler handle for start_queued notifications.
            let notify = ToolScheduler {
                inner: Arc::clone(&inner),
                status_tx,
            };
            Self::start_queued_locked(&notify, &mut state);
        });
    }

    fn start_queued_locked(scheduler: &ToolScheduler<R>, state: &mut SchedulerState<R>) {
        let queued = std::mem::take(&mut state.queued);
        let mut still = Vec::new();
        for task in queued {
            if Self::is_blocked(&task.accesses, &state.active, &still) {
                if let Some(behind) = Self::blocked_by(&task.accesses, &state.active, &still) {
                    scheduler.notify(SchedulerStatus::Queued {
                        tool_call_id: task.tool_call_id.clone(),
                        behind,
                    });
                }
                still.push(task);
            } else {
                Self::start_locked(scheduler, state, task);
            }
        }
        state.queued = still;
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    let message = payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into());
    format!("tool execution panicked: {message}")
}

impl<R: Send + 'static> Clone for ToolScheduler<R> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            status_tx: self.status_tx.clone(),
        }
    }
}

impl<R: Send + 'static> Default for ToolScheduler<R> {
    fn default() -> Self {
        Self::new()
    }
}

/// Helper to box an async start closure.
pub fn box_start<F, Fut, R>(f: F) -> Box<dyn FnOnce() -> BoxFuture<R> + Send>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = R> + Send + 'static,
    R: Send + 'static,
{
    Box::new(move || Box::pin(f()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kkagent_tools::accesses::tool_accesses;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn task<R: Send + 'static>(
        name: &str,
        accesses: ToolAccesses,
        start: Box<dyn FnOnce() -> BoxFuture<R> + Send>,
    ) -> ToolCallTask<R> {
        ToolCallTask {
            tool_call_id: format!("id-{name}"),
            tool_name: name.into(),
            accesses,
            start,
        }
    }

    #[tokio::test]
    async fn parallel_non_conflicting() {
        let counter = Arc::new(AtomicUsize::new(0));
        let max = Arc::new(AtomicUsize::new(0));
        let c1 = counter.clone();
        let m1 = max.clone();
        let c2 = counter.clone();
        let m2 = max.clone();

        let tasks = vec![
            task(
                "a",
                tool_accesses::read_file("/a"),
                box_start(move || {
                    let c1 = c1;
                    let m1 = m1;
                    async move {
                        let cur = c1.fetch_add(1, Ordering::SeqCst) + 1;
                        m1.fetch_max(cur, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        c1.fetch_sub(1, Ordering::SeqCst);
                        1
                    }
                }),
            ),
            task(
                "b",
                tool_accesses::read_file("/b"),
                box_start(move || {
                    let c2 = c2;
                    let m2 = m2;
                    async move {
                        let cur = c2.fetch_add(1, Ordering::SeqCst) + 1;
                        m2.fetch_max(cur, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        c2.fetch_sub(1, Ordering::SeqCst);
                        2
                    }
                }),
            ),
        ];
        let results = ToolScheduler::run_all(tasks).await;
        assert_eq!(
            results.into_iter().collect::<Result<Vec<_>, _>>().unwrap(),
            vec![1, 2]
        );
        assert!(max.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn serializes_conflicts() {
        let counter = Arc::new(AtomicUsize::new(0));
        let max = Arc::new(AtomicUsize::new(0));
        let c1 = counter.clone();
        let m1 = max.clone();
        let c2 = counter.clone();
        let m2 = max.clone();

        let tasks = vec![
            task(
                "write",
                tool_accesses::write_file("/a"),
                box_start(move || {
                    let c1 = c1;
                    let m1 = m1;
                    async move {
                        let cur = c1.fetch_add(1, Ordering::SeqCst) + 1;
                        m1.fetch_max(cur, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(40)).await;
                        c1.fetch_sub(1, Ordering::SeqCst);
                        1
                    }
                }),
            ),
            task(
                "read",
                tool_accesses::read_file("/a"),
                box_start(move || {
                    let c2 = c2;
                    let m2 = m2;
                    async move {
                        let cur = c2.fetch_add(1, Ordering::SeqCst) + 1;
                        m2.fetch_max(cur, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(40)).await;
                        c2.fetch_sub(1, Ordering::SeqCst);
                        2
                    }
                }),
            ),
        ];
        let (tx, mut rx) = mpsc::unbounded_channel();
        let results = ToolScheduler::run_all_with_status(tasks, Some(tx)).await;
        assert_eq!(
            results.into_iter().collect::<Result<Vec<_>, _>>().unwrap(),
            vec![1, 2]
        );
        assert_eq!(max.load(Ordering::SeqCst), 1);
        let mut saw_queued = false;
        while let Ok(status) = rx.try_recv() {
            if let SchedulerStatus::Queued { behind, .. } = status {
                assert_eq!(behind, "write");
                saw_queued = true;
            }
        }
        assert!(saw_queued, "conflicting task should report queued status");
    }

    #[tokio::test]
    async fn panic_does_not_deadlock_conflicting_queued_tasks() {
        let tasks = vec![
            task(
                "write",
                tool_accesses::write_file("/a"),
                box_start(|| async move { panic!("boom") }),
            ),
            task(
                "read",
                tool_accesses::read_file("/a"),
                box_start(|| async move { 2 }),
            ),
        ];
        let results = tokio::time::timeout(Duration::from_secs(1), ToolScheduler::run_all(tasks))
            .await
            .expect("queued task should be released after a panic");
        assert!(results[0]
            .as_ref()
            .unwrap_err()
            .contains("tool execution panicked: boom"));
        assert_eq!(results[1], Ok(2));
    }
}
