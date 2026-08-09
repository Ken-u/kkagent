//! Parallel tool scheduler with resource-conflict gating (kimi ToolScheduler).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures::FutureExt;
use kkagent_tools::accesses::{tool_accesses, ToolAccesses};
use tokio::sync::{oneshot, Mutex};

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

pub struct ToolCallTask<R> {
    pub accesses: ToolAccesses,
    pub start: Box<dyn FnOnce() -> BoxFuture<R> + Send>,
}

struct ScheduledTask<R> {
    id: u64,
    accesses: ToolAccesses,
    start: Option<Box<dyn FnOnce() -> BoxFuture<R> + Send>>,
    result_tx: Option<oneshot::Sender<Result<R, String>>>,
}

struct ActiveSlot {
    id: u64,
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
}

impl<R: Send + 'static> ToolScheduler<R> {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(SchedulerState {
                next_id: 1,
                active: Vec::new(),
                queued: Vec::new(),
            })),
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
                accesses: task.accesses,
                start: Some(task.start),
                result_tx: Some(tx),
            }
        };

        {
            let mut state = self.inner.lock().await;
            if Self::is_blocked(&scheduled.accesses, &state.active, &state.queued) {
                state.queued.push(scheduled);
            } else {
                Self::start_locked(&self.inner, &mut state, scheduled);
            }
        }

        rx.await
            .map_err(|_| "tool scheduler result channel closed".to_string())?
    }

    /// Schedule many tasks and wait for all results in the original order.
    pub async fn run_all(tasks: Vec<ToolCallTask<R>>) -> Vec<Result<R, String>> {
        let scheduler = Self::new();
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

    fn is_blocked(
        accesses: &ToolAccesses,
        active: &[ActiveSlot],
        queued_before: &[ScheduledTask<R>],
    ) -> bool {
        active
            .iter()
            .any(|a| tool_accesses::conflict(accesses, &a.accesses))
            || queued_before
                .iter()
                .any(|q| tool_accesses::conflict(accesses, &q.accesses))
    }

    fn start_locked(
        inner: &Arc<Mutex<SchedulerState<R>>>,
        state: &mut SchedulerState<R>,
        mut task: ScheduledTask<R>,
    ) {
        let id = task.id;
        let accesses = task.accesses.clone();
        state.active.push(ActiveSlot {
            id,
            accesses: accesses.clone(),
        });
        let start = task.start.take().expect("start fn");
        let result_tx = task.result_tx.take().expect("result tx");
        let inner = Arc::clone(inner);
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
            Self::start_queued_locked(&inner, &mut state);
        });
    }

    fn start_queued_locked(inner: &Arc<Mutex<SchedulerState<R>>>, state: &mut SchedulerState<R>) {
        let queued = std::mem::take(&mut state.queued);
        let mut still = Vec::new();
        for task in queued {
            if Self::is_blocked(&task.accesses, &state.active, &still) {
                still.push(task);
            } else {
                Self::start_locked(inner, state, task);
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

    #[tokio::test]
    async fn parallel_non_conflicting() {
        let counter = Arc::new(AtomicUsize::new(0));
        let max = Arc::new(AtomicUsize::new(0));
        let c1 = counter.clone();
        let m1 = max.clone();
        let c2 = counter.clone();
        let m2 = max.clone();

        let tasks = vec![
            ToolCallTask {
                accesses: tool_accesses::read_file("/a"),
                start: box_start(move || {
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
            },
            ToolCallTask {
                accesses: tool_accesses::read_file("/b"),
                start: box_start(move || {
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
            },
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
            ToolCallTask {
                accesses: tool_accesses::write_file("/a"),
                start: box_start(move || {
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
            },
            ToolCallTask {
                accesses: tool_accesses::read_file("/a"),
                start: box_start(move || {
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
            },
        ];
        let results = ToolScheduler::run_all(tasks).await;
        assert_eq!(
            results.into_iter().collect::<Result<Vec<_>, _>>().unwrap(),
            vec![1, 2]
        );
        assert_eq!(max.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn panic_does_not_deadlock_conflicting_queued_tasks() {
        let tasks = vec![
            ToolCallTask {
                accesses: tool_accesses::write_file("/a"),
                start: box_start(|| async move { panic!("boom") }),
            },
            ToolCallTask {
                accesses: tool_accesses::read_file("/a"),
                start: box_start(|| async move { 2 }),
            },
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
