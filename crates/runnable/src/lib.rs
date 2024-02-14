//! Defines baseline interface of Runnables in Zed.
// #![deny(missing_docs)]
pub mod static_runnable_file;
mod static_runner;
mod static_source;

use anyhow::{Context, Result};
use async_process::{ChildStderr, ChildStdout, ExitStatus};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use futures::future::{join_all, BoxFuture, Shared};
pub use futures::stream::Aborted as RunnableTerminated;
use futures::stream::{AbortHandle, Abortable};
use futures::{AsyncBufReadExt, AsyncRead, Future, FutureExt};
use gpui::{AppContext, AsyncAppContext, EntityId, Model, ModelContext, Task, WeakModel};
use parking_lot::Mutex;
use smol::io::BufReader;
pub use static_runner::StaticRunner;
pub use static_source::{StaticSource, TrackedFile};
use std::any::Any;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::task::Poll;
use util::ResultExt;

/// Represents a runnable that's already underway. That runnable can be cancelled at any time.
#[derive(Clone)]
pub struct RunnableHandle {
    fut: Shared<Task<Result<Result<ExitStatus, Arc<anyhow::Error>>, RunnableTerminated>>>,
    pub output: Option<PendingOutput>,
    cancel_token: AbortHandle,
}

#[derive(Clone, Debug)]
pub struct PendingOutput {
    output_read_tasks: [Shared<Task<()>>; 2],
    full_output: Arc<Mutex<String>>,
    output_lines_rx: Arc<Mutex<UnboundedReceiver<String>>>,
}

impl PendingOutput {
    fn new(stdout: ChildStdout, stderr: ChildStderr, cx: &mut AsyncAppContext) -> Self {
        let (output_lines_tx, output_lines_rx) = futures::channel::mpsc::unbounded();
        let output_lines_rx = Arc::new(Mutex::new(output_lines_rx));
        let full_output = Arc::new(Mutex::new(String::new()));

        let stdout_capture = Arc::clone(&full_output);
        let stdout_tx = output_lines_tx.clone();
        let stdout_task = cx
            .background_executor()
            .spawn(async move {
                handle_output(stdout, stdout_tx, stdout_capture)
                    .await
                    .context("stdout capture")
                    .log_err();
            })
            .shared();

        let stderr_capture = Arc::clone(&full_output);
        let stderr_tx = output_lines_tx;
        let stderr_task = cx
            .background_executor()
            .spawn(async move {
                handle_output(stderr, stderr_tx, stderr_capture)
                    .await
                    .context("stderr capture")
                    .log_err();
            })
            .shared();

        Self {
            output_read_tasks: [stdout_task, stderr_task],
            full_output,
            output_lines_rx,
        }
    }

    pub fn subscribe(&self) -> Arc<Mutex<UnboundedReceiver<String>>> {
        Arc::clone(&self.output_lines_rx)
    }

    pub fn full_output(self, cx: &mut AppContext) -> Task<String> {
        cx.spawn(|_| async move {
            let _: Vec<()> = join_all(self.output_read_tasks).await;
            self.full_output.lock().clone()
        })
    }
}

impl RunnableHandle {
    pub fn new(
        fut: BoxFuture<'static, Result<ExitStatus, Arc<anyhow::Error>>>,
        output: Option<PendingOutput>,
        cx: AsyncAppContext,
    ) -> Result<Self> {
        let (cancel_token, abort_registration) = AbortHandle::new_pair();
        let fut = cx
            .spawn(move |_| Abortable::new(fut, abort_registration))
            .shared();
        Ok(Self {
            fut,
            output,
            cancel_token,
        })
    }

    /// Returns a handle that can be used to cancel this runnable.
    pub fn termination_handle(&self) -> AbortHandle {
        self.cancel_token.clone()
    }

    pub fn result<'a>(&self) -> Option<Result<ExecutionResult, RunnableTerminated>> {
        self.fut.peek().cloned().map(|res| {
            res.map(|runnable_result| ExecutionResult {
                status: runnable_result,
                output: self.output.clone(),
            })
        })
    }
}

impl Future for RunnableHandle {
    type Output = Result<ExecutionResult, RunnableTerminated>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Self::Output> {
        match self.fut.poll_unpin(cx) {
            Poll::Ready(res) => match res {
                Ok(runnable_result) => Poll::Ready(Ok(ExecutionResult {
                    status: runnable_result,
                    output: self.output.clone(),
                })),
                Err(aborted) => Poll::Ready(Err(aborted)),
            },
            Poll::Pending => Poll::Pending,
        }
    }
}

#[derive(Clone, Debug)]
/// Represents the result of a runnable.
pub struct ExecutionResult {
    /// Status of the runnable. Should be `Ok` if the runnable launch succeeded, `Err` otherwise.
    pub status: Result<ExitStatus, Arc<anyhow::Error>>,
    pub output: Option<PendingOutput>,
}

/// Represents a short lived recipe of a runnable, whose main purpose
/// is to get spawned.
pub trait Runnable {
    fn name(&self) -> String;
    fn exec(&self, cwd: Option<PathBuf>, cx: gpui::AsyncAppContext) -> Result<RunnableHandle>;
    fn boxed_clone(&self) -> Box<dyn Runnable>;
}

/// [`Source`] produces runnables that can be scheduled.
///
/// Implementations of this trait could be e.g. [`StaticSource`] that parses tasks from a .json files and provides process templates to be spawned;
/// another one could be a language server providing lenses with tests or build server listing all targets for a given project.
pub trait Source: Any {
    fn as_any(&mut self) -> &mut dyn Any;
    fn runnables_for_path(
        &mut self,
        path: &Path,
        cx: &mut ModelContext<Box<dyn Source>>,
    ) -> anyhow::Result<Vec<RunnableToken>>;
}

#[derive(PartialEq)]
pub struct RunnableMetadata {
    source: WeakModel<Box<dyn Source>>,
    display_name: String,
}

impl RunnableMetadata {
    pub fn display_name(&self) -> &str {
        &self.display_name
    }
}

/// Represents a runnable that might or might not be already running.
#[derive(Clone)]
pub struct RunnableToken {
    metadata: Arc<RunnableMetadata>,
    state: Model<RunState>,
}

#[derive(Clone)]
pub(crate) enum RunState {
    NotScheduled(Arc<dyn Runnable>),
    Scheduled(RunnableHandle),
}

impl RunnableToken {
    /// Schedules a runnable or returns a handle to it if it's already running.
    pub fn schedule(&self, cwd: Option<PathBuf>, cx: &mut AppContext) -> Result<RunnableHandle> {
        let mut spawned_first_time = false;
        let ret = self.state.update(cx, |this, cx| match this {
            RunState::NotScheduled(runnable) => {
                let handle = runnable.exec(cwd, cx.to_async())?;
                spawned_first_time = true;
                *this = RunState::Scheduled(handle.clone());

                Ok(handle)
            }
            RunState::Scheduled(handle) => Ok(handle.clone()),
        });
        if spawned_first_time {
            // todo: this should be a noop when ran multiple times, but we should still strive to do it just once.
            cx.spawn(|_| async_process::driver()).detach();
            self.state.update(cx, |_, cx| {
                cx.spawn(|state, mut cx| async move {
                    let Some(this) = state.upgrade() else {
                        return;
                    };
                    let Some(handle) = this
                        .update(&mut cx, |this, _| {
                            if let RunState::Scheduled(this) = this {
                                Some(this.clone())
                            } else {
                                None
                            }
                        })
                        .ok()
                        .flatten()
                    else {
                        return;
                    };
                    let _ = handle.fut.await.log_err();
                })
                .detach()
            })
        }
        ret
    }

    pub fn handle(&self, cx: &AppContext) -> Option<RunnableHandle> {
        let state = self.state.read(cx);
        if let RunState::Scheduled(state) = state {
            Some(state.clone())
        } else {
            None
        }
    }

    pub fn result<'a>(
        &self,
        cx: &'a AppContext,
    ) -> Option<Result<ExecutionResult, RunnableTerminated>> {
        if let RunState::Scheduled(state) = self.state.read(cx) {
            state.fut.peek().cloned().map(|res| {
                res.map(|runnable_result| ExecutionResult {
                    status: runnable_result,
                    output: state.output.clone(),
                })
            })
        } else {
            None
        }
    }

    pub fn cancel_handle(&self, cx: &AppContext) -> Option<AbortHandle> {
        if let RunState::Scheduled(state) = self.state.read(cx) {
            Some(state.termination_handle())
        } else {
            None
        }
    }

    pub fn was_scheduled(&self, cx: &AppContext) -> bool {
        self.handle(cx).is_some()
    }

    pub fn metadata(&self) -> &RunnableMetadata {
        &self.metadata
    }

    pub fn id(&self) -> EntityId {
        self.state.entity_id()
    }
}

async fn handle_output<Output>(
    output: Output,
    output_tx: UnboundedSender<String>,
    capture: Arc<Mutex<String>>,
) -> anyhow::Result<()>
where
    Output: AsyncRead + Unpin + Send + 'static,
{
    let mut output = BufReader::new(output);
    let mut buffer = Vec::new();

    loop {
        buffer.clear();

        let bytes_read = output
            .read_until(b'\n', &mut buffer)
            .await
            .context("reading output newline")?;
        if bytes_read == 0 {
            return Ok(());
        }

        let output_line = String::from_utf8_lossy(&buffer);
        capture.lock().push_str(&output_line);
        output_tx.unbounded_send(output_line.to_string()).ok();

        // Don't starve the main thread when receiving lots of messages at once.
        smol::future::yield_now().await;
    }
}
