//! Keep render construction errors and retry construction on each show request.

use std::sync::{Arc, Mutex};

use dormant_core::traits::RenderSink;
use dormant_core::types::{CmdFailure, DisplayId, StageKind};

pub(crate) type SinkBuilder =
    Box<dyn Fn() -> Result<Arc<dyn RenderSink>, CmdFailure> + Send + Sync>;

struct State {
    sink: Option<Arc<dyn RenderSink>>,
    error: Option<CmdFailure>,
    retry_error: Option<CmdFailure>,
}

#[derive(Clone)]
pub(crate) struct RetryRenderSink {
    display: DisplayId,
    builder: Option<Arc<SinkBuilder>>,
    state: Arc<Mutex<State>>,
}

impl RetryRenderSink {
    pub(crate) fn new(did: DisplayId, builder: SinkBuilder) -> Self {
        let initial = builder();
        let (sink, error) = match initial {
            Ok(sink) => (Some(sink), None),
            Err(error) => {
                tracing::warn!(event = "render_sink_build_failed", display = %did, %error);
                (None, Some(error))
            }
        };
        Self {
            display: did,
            builder: Some(Arc::new(builder)),
            state: Arc::new(Mutex::new(State {
                sink,
                error,
                retry_error: None,
            })),
        }
    }

    pub(crate) fn unavailable(display: DisplayId, error: CmdFailure) -> Self {
        Self {
            display,
            builder: None,
            state: Arc::new(Mutex::new(State {
                sink: None,
                error: Some(error),
                retry_error: None,
            })),
        }
    }

    fn get_or_build(&self) -> Result<Arc<dyn RenderSink>, CmdFailure> {
        let mut state = self.state.lock().expect("render retry state poisoned");
        if let Some(sink) = &state.sink {
            return Ok(sink.clone());
        }
        let Some(builder) = &self.builder else {
            return Err(state.error.clone().expect("unavailable sink has a reason"));
        };
        match builder() {
            Ok(sink) => {
                tracing::info!(event = "render_sink_built_late", display = %self.display);
                state.error = None;
                state.sink = Some(sink.clone());
                Ok(sink)
            }
            Err(error) => {
                if state.retry_error.as_ref() != Some(&error) {
                    tracing::warn!(event = "render_sink_build_retry_failed", display = %self.display, %error);
                }
                state.retry_error = Some(error.clone());
                state.error = Some(error.clone());
                Err(error)
            }
        }
    }

    fn built(&self) -> Option<Arc<dyn RenderSink>> {
        self.state
            .lock()
            .expect("render retry state poisoned")
            .sink
            .clone()
    }
}

#[async_trait::async_trait]
impl RenderSink for RetryRenderSink {
    async fn show(&self, r#gen: u64, idx: usize, kind: StageKind) -> Result<(), CmdFailure> {
        let sink = if let Some(sink) = self.built() {
            sink
        } else {
            let retry = self.clone();
            tokio::task::spawn_blocking(move || retry.get_or_build())
                .await
                .map_err(|error| CmdFailure {
                    controller: "render".into(),
                    error: format!("E_RENDER_UNAVAILABLE: render builder task: {error}"),
                })??
        };
        sink.show(r#gen, idx, kind).await
    }

    async fn teardown(&self, r#gen: u64) {
        if let Some(sink) = self.built() {
            sink.teardown(r#gen).await;
        }
    }

    async fn show_current_overlay(&self) -> Result<(), CmdFailure> {
        if let Some(sink) = self.built() {
            sink.show_current_overlay().await
        } else {
            Ok(())
        }
    }

    fn health_error(&self) -> Option<String> {
        let state = self.state.lock().expect("render retry state poisoned");
        state.error.as_ref().map(|error| error.error.clone())
    }
}
