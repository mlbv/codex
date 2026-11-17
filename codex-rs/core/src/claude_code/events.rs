use crate::CodexThread;
use crate::context::ContextualUserFragment;
use crate::context::InternalContextSource;
use crate::context::InternalModelContextFragment;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::turn_input::TurnInput;
use codex_protocol::turn_input::TurnInputRequest;
use codex_protocol::turn_input::TurnInputSubmission;
use codex_protocol::turn_input::TurnStartOptions;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::Weak;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// A yielded turn remains unfinished while its observations can resume the agent.
pub(crate) async fn wait_until_idle(
    runtime: &super::Runtime,
    cancellation: &CancellationToken,
) -> anyhow::Result<AgentStatus> {
    let thread = runtime
        .thread
        .upgrade()
        .ok_or_else(|| anyhow::anyhow!("child stopped while waiting for background work"))?;
    let mut statuses = thread.subscribe_status();
    loop {
        let status = statuses.borrow_and_update().clone();
        if matches!(
            status,
            AgentStatus::Errored(_)
                | AgentStatus::Interrupted
                | AgentStatus::Shutdown
                | AgentStatus::NotFound
        ) {
            return Ok(status);
        }
        let idle = runtime.idle.notified();
        tokio::pin!(idle);
        idle.as_mut().enable();
        if runtime.tasks.has_pending_work() || runtime.events.has_pending() {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => anyhow::bail!("agent observation cancelled"),
                _ = runtime.cancellation.cancelled() => anyhow::bail!("child stopped while waiting for background work"),
                changed = statuses.changed() => changed?,
                _ = async {
                    runtime.tasks.wait_for_settled(cancellation).await;
                    runtime.events.wait_for_settled(cancellation).await;
                } => {},
            }
            continue;
        }
        let active = thread.session.active_turn.lock().await;
        if active.is_none() && !runtime.tasks.has_pending_work() && !runtime.events.has_pending() {
            return Ok(statuses.borrow_and_update().clone());
        }
        drop(active);
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => anyhow::bail!("agent observation cancelled"),
            _ = runtime.cancellation.cancelled() => anyhow::bail!("child stopped while waiting for background work"),
            changed = statuses.changed() => changed?,
            _ = idle => {},
        }
    }
}

#[derive(Clone)]
pub(crate) struct EventSink(Arc<State>);

struct State {
    thread: Weak<CodexThread>,
    pending: Mutex<VecDeque<NativeEvent>>,
    count: AtomicUsize,
    changed: Notify,
    wake: Notify,
    closed: CancellationToken,
    worker: StdMutex<Option<JoinHandle<()>>>,
}

struct NativeEvent {
    kind: String,
    text: String,
}

impl NativeEvent {
    fn fragment(&self) -> InternalModelContextFragment {
        InternalModelContextFragment::new(
            InternalContextSource::from_static("background"),
            format!(
                "<background_event>\nSource: {}\n{}\n</background_event>",
                self.kind, self.text
            ),
        )
    }
}

impl EventSink {
    pub(crate) fn new(thread: Weak<CodexThread>) -> Self {
        let state = Arc::new(State {
            thread,
            pending: Mutex::new(VecDeque::new()),
            count: AtomicUsize::new(0),
            changed: Notify::new(),
            wake: Notify::new(),
            closed: CancellationToken::new(),
            worker: StdMutex::new(None),
        });
        Self(state)
    }

    pub(crate) fn start(&self) {
        let weak = Arc::downgrade(&self.0);
        let worker = tokio::spawn(async move {
            while let Some(state) = weak.upgrade() {
                tokio::select! {
                    biased;
                    _ = state.closed.cancelled() => break,
                    _ = state.wake.notified() => {}
                }
                if let Err(error) = dispatch(&state).await {
                    if let Some(thread) = state.thread.upgrade() {
                        crate::claude_code::report_error(&thread.session, error).await;
                    }
                }
            }
        });
        *self.0.worker.lock().unwrap() = Some(worker);
    }

    /// Enqueues the observation before returning; a separate worker admits model turns.
    pub(crate) async fn emit(&self, kind: &str, text: String) -> anyhow::Result<()> {
        anyhow::ensure!(!self.0.closed.is_cancelled(), "conversation has stopped");
        let mut queue = self.0.pending.lock().await;
        queue.push_back(NativeEvent {
            kind: kind.to_string(),
            text,
        });
        self.0.count.fetch_add(1, Ordering::Release);
        drop(queue);
        self.0.wake.notify_one();
        Ok(())
    }

    pub(crate) async fn flush(&self) -> anyhow::Result<()> {
        self.0.wake.notify_one();
        Ok(())
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.0.count.load(Ordering::Acquire) != 0
    }

    /// Waits until queued observations have been accepted into an active or newly started turn.
    pub(crate) async fn wait_for_settled(&self, cancellation: &CancellationToken) {
        loop {
            let changed = self.0.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !self.has_pending() {
                return;
            }
            tokio::select! {
                _ = changed => {},
                _ = cancellation.cancelled() => return,
                _ = self.0.closed.cancelled() => return,
            }
        }
    }

    pub(crate) fn close(&self) {
        self.0.closed.cancel();
    }

    pub(crate) async fn shutdown(&self) -> anyhow::Result<()> {
        self.close();
        let worker = self.0.worker.lock().unwrap().take();
        if let Some(worker) = worker {
            worker.await?;
        }
        Ok(())
    }
}

async fn dispatch(state: &State) -> anyhow::Result<()> {
    let thread = state
        .thread
        .upgrade()
        .ok_or_else(|| anyhow::anyhow!("conversation has stopped"))?;
    let _residency = tokio::select! {
        biased;
        _ = state.closed.cancelled() => return Ok(()),
        residency = Arc::clone(&thread.residency_gate).read_owned() => residency,
    };
    loop {
        if thread.session.is_interrupted() {
            return Ok(());
        }
        let fragment = {
            let queue = state.pending.lock().await;
            let Some(event) = queue.front() else {
                return Ok(());
            };
            event.fragment()
        };
        let delivery = async {
            let input = match thread
                .session
                .inject_if_running(vec![ContextualUserFragment::into(fragment)])
                .await
            {
                Ok(()) => return Ok(true),
                Err(mut items) => items.pop().unwrap(),
            };
            let request =
                TurnInputRequest::new(TurnInput::ResponseItem(input)).on_start(TurnStartOptions {
                    turn_trigger: Some("background_event".to_string()),
                    ..Default::default()
                });
            let submission = thread.wake_if_idle(request).await?;
            anyhow::Ok(matches!(
                submission,
                TurnInputSubmission::Started { .. } | TurnInputSubmission::Steered { .. }
            ))
        };
        let accepted = tokio::select! {
            biased;
            _ = state.closed.cancelled() => return Ok(()),
            accepted = delivery => accepted?,
        };
        if !accepted {
            return Ok(());
        }
        state.pending.lock().await.pop_front();
        state.count.fetch_sub(1, Ordering::Release);
        state.changed.notify_waiters();
    }
}
