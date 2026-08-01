//! Linux XDG `ScreenCast` portal and `PipeWire` capture implementation.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use dormant_core::config::schema::StreamMode;
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use time::OffsetDateTime;
use zbus::zvariant::{OwnedFd as ZbusOwnedFd, OwnedObjectPath, OwnedValue, Value};

use super::{
    CaptureError, CaptureSource, ConnectedStream, ConsentBinding, DisplayExpectation, Grant,
    RawFrame, WEAR_SAMPLING_WRONG_MONITOR,
};

const PORTAL_SERVICE: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SCREENCAST_INTERFACE: &str = "org.freedesktop.portal.ScreenCast";
const REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";
const SESSION_INTERFACE: &str = "org.freedesktop.portal.Session";

/// Portal session handle retained until its `PipeWire` stream closes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortalSession {
    path: OwnedObjectPath,
}

impl PortalSession {
    #[cfg(test)]
    fn fake() -> Self {
        Self {
            path: OwnedObjectPath::try_from("/org/freedesktop/portal/desktop/session/fake")
                .expect("literal session path is valid"),
        }
    }
}

/// Source categories accepted by the `ScreenCast` portal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceTypes(u32);

impl SourceTypes {
    /// Request monitor capture only.
    pub const MONITOR: Self = Self(1);
}

/// Cursor visibility requested from the `ScreenCast` portal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorMode(u32);

impl CursorMode {
    /// Exclude the cursor from captured frames.
    pub const HIDDEN: Self = Self(1);
}

/// Options passed to `ScreenCast.SelectSources`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectSourcesOptions {
    /// Only monitor sources are valid for display wear sampling.
    pub types: SourceTypes,
    /// The sampler tracks exactly one configured display.
    pub multiple: bool,
    /// The cursor is not panel content and must not affect luma reduction.
    pub cursor_mode: CursorMode,
    /// Request a persistent token so a daemon restart can reattach silently.
    pub persist_mode: u32,
    /// Existing token supplied only during a reconnect.
    pub restore_token: Option<String>,
}

impl SelectSourcesOptions {
    fn for_grant() -> Self {
        Self {
            types: SourceTypes::MONITOR,
            multiple: false,
            cursor_mode: CursorMode::HIDDEN,
            persist_mode: 2,
            restore_token: None,
        }
    }

    fn for_reattach(token: &str) -> Self {
        Self {
            restore_token: Some(token.to_owned()),
            ..Self::for_grant()
        }
    }
}

/// One compositor stream returned by `ScreenCast.Start`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortalStream {
    /// `PipeWire` node visible only through the portal-supplied fd.
    pub node_id: u32,
    /// Compositor-coordinate width reported by the portal.
    pub width: u32,
    /// Compositor-coordinate height reported by the portal.
    pub height: u32,
    /// Stable compositor identity when the portal provides one.
    pub persistent_id: Option<String>,
}

/// Successful `ScreenCast.Start` response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortalStartResult {
    /// The exactly one requested monitor stream.
    pub streams: Vec<PortalStream>,
    /// The token to persist, which can rotate on every successful start.
    pub restore_token: String,
}

impl PortalStartResult {
    #[cfg(test)]
    fn single(
        node_id: u32,
        width: u32,
        height: u32,
        persistent_id: Option<&str>,
        restore_token: &str,
    ) -> Self {
        Self {
            streams: vec![PortalStream {
                node_id,
                width,
                height,
                persistent_id: persistent_id.map(str::to_owned),
            }],
            restore_token: restore_token.to_owned(),
        }
    }
}

/// Injectable D-Bus boundary for the `ScreenCast` portal protocol.
#[async_trait]
pub trait PortalTransport: Send + Sync + 'static {
    /// Creates a portal session while retaining its D-Bus connection.
    async fn create_session(&self) -> Result<PortalSession, CaptureError>;
    /// Selects the monitor source for a session.
    async fn select_sources(
        &self,
        session: &PortalSession,
        options: SelectSourcesOptions,
    ) -> Result<(), CaptureError>;
    /// Starts the source and returns stream metadata plus a rotated token.
    async fn start(&self, session: &PortalSession) -> Result<PortalStartResult, CaptureError>;
    /// Opens the portal-private `PipeWire` remote for a started session.
    async fn open_pipewire_remote(&self, session: &PortalSession) -> Result<OwnedFd, CaptureError>;
    /// Closes the session without leaking an active portal stream.
    async fn close(&self, session: PortalSession);
}

/// Linux `CaptureSource` backed by an XDG `ScreenCast` portal session.
pub struct PortalPipeWireSource<T = ZbusPortalTransport> {
    transport: T,
    session: Option<PortalSession>,
    stream: Option<ConnectedStream>,
    pipewire_fd: Option<OwnedFd>,
    #[cfg(test)]
    scripted_frames: std::collections::VecDeque<Result<RawFrame, CaptureError>>,
}

impl PortalPipeWireSource<ZbusPortalTransport> {
    /// Connects to the session bus and retains it for the portal session lifetime.
    ///
    /// # Errors
    ///
    /// Returns a transport error when the session bus cannot be reached.
    pub async fn new() -> Result<Self, CaptureError> {
        Ok(Self::from_transport(ZbusPortalTransport::new().await?))
    }
}

impl<T> PortalPipeWireSource<T> {
    /// Builds a capture source around a transport seam.
    pub fn from_transport(transport: T) -> Self {
        Self {
            transport,
            session: None,
            stream: None,
            pipewire_fd: None,
            #[cfg(test)]
            scripted_frames: std::collections::VecDeque::new(),
        }
    }

    #[cfg(test)]
    fn from_transport_with_frames(
        transport: T,
        frames: impl IntoIterator<Item = Result<RawFrame, CaptureError>>,
    ) -> Self {
        let mut source = Self::from_transport(transport);
        source.scripted_frames = frames.into_iter().collect();
        source
    }
}

impl<T: PortalTransport> PortalPipeWireSource<T> {
    async fn open(
        &mut self,
        options: SelectSourcesOptions,
    ) -> Result<ConnectedStream, CaptureError> {
        self.close().await;
        let session = self.transport.create_session().await?;
        let opened = async {
            self.transport.select_sources(&session, options).await?;
            let stream = connected_stream(self.transport.start(&session).await?)?;
            let pipewire_fd = self.transport.open_pipewire_remote(&session).await?;
            Ok::<_, CaptureError>((stream, pipewire_fd))
        }
        .await;
        match opened {
            Ok((stream, pipewire_fd)) => {
                self.session = Some(session);
                self.stream = Some(stream.clone());
                self.pipewire_fd = Some(pipewire_fd);
                Ok(stream)
            }
            Err(error) => {
                self.transport.close(session).await;
                Err(error)
            }
        }
    }
}

#[async_trait]
impl<T: PortalTransport> CaptureSource for PortalPipeWireSource<T> {
    async fn connect(
        &mut self,
        binding: &ConsentBinding<'_>,
    ) -> Result<ConnectedStream, CaptureError> {
        let stream = self
            .open(SelectSourcesOptions::for_reattach(binding.token))
            .await?;
        if let Err(error) = reconcile_start_with_binding(&stream, binding) {
            self.close().await;
            return Err(error);
        }
        let frame = self.capture_one(StreamMode::Warm).await?;
        if let Err(error) = reconcile_reattached_frame(&frame, binding) {
            self.close().await;
            return Err(error);
        }
        Ok(self
            .stream
            .clone()
            .expect("capture keeps a connected stream"))
    }

    async fn request_consent(
        &mut self,
        _display: &DisplayExpectation,
    ) -> Result<Grant, CaptureError> {
        self.open(SelectSourcesOptions::for_grant()).await?;
        self.capture_one(StreamMode::Warm).await?;
        Ok(Grant {
            stream: self
                .stream
                .clone()
                .expect("capture keeps a connected stream"),
            granted_at: OffsetDateTime::now_utc(),
        })
    }

    async fn capture_one(&mut self, _mode: StreamMode) -> Result<RawFrame, CaptureError> {
        #[cfg(test)]
        if let Some(frame) = self.scripted_frames.pop_front() {
            let frame = frame?;
            if let Some(stream) = self.stream.as_mut() {
                stream.frame_width = frame.width;
                stream.frame_height = frame.height;
            }
            return Ok(frame);
        }
        let stream = self.stream.as_ref().ok_or_else(|| {
            CaptureError::Protocol("capture requested before portal connection".to_owned())
        })?;
        let fd = self
            .pipewire_fd
            .as_ref()
            .ok_or_else(|| CaptureError::Protocol("portal remote fd is unavailable".to_owned()))?
            .try_clone()
            .map_err(|error| {
                CaptureError::Transport(format!("clone portal PipeWire fd: {error}"))
            })?;
        let node_id = stream.node_id;
        let frame = tokio::task::spawn_blocking(move || acquire_one_frame(fd, node_id))
            .await
            .map_err(|error| CaptureError::Transport(format!("PipeWire worker join: {error}")))??;
        if let Some(stream) = self.stream.as_mut() {
            stream.frame_width = frame.width;
            stream.frame_height = frame.height;
        }
        Ok(frame)
    }

    async fn close(&mut self) {
        self.pipewire_fd = None;
        self.stream = None;
        if let Some(session) = self.session.take() {
            self.transport.close(session).await;
        }
    }
}

fn connected_stream(start: PortalStartResult) -> Result<ConnectedStream, CaptureError> {
    let [stream] = start.streams.as_slice() else {
        return Err(CaptureError::Protocol(
            "portal Start did not return exactly one monitor stream".to_owned(),
        ));
    };
    if start.restore_token.is_empty() {
        return Err(CaptureError::Protocol(
            "portal Start did not return a restore token".to_owned(),
        ));
    }
    Ok(ConnectedStream {
        node_id: stream.node_id,
        restore_token: start.restore_token,
        persistent_id: stream.persistent_id.clone(),
        width: stream.width,
        height: stream.height,
        frame_width: 0,
        frame_height: 0,
    })
}

fn reconcile_start_with_binding(
    stream: &ConnectedStream,
    binding: &ConsentBinding<'_>,
) -> Result<(), CaptureError> {
    let persistent_id_matches = binding.portal_persistent_ids.is_empty()
        || stream
            .persistent_id
            .as_ref()
            .is_some_and(|id| binding.portal_persistent_ids.contains(id));
    if !persistent_id_matches {
        return Err(CaptureError::Protocol(
            WEAR_SAMPLING_WRONG_MONITOR.to_owned(),
        ));
    }
    Ok(())
}

/// Validates native frame dimensions against a persisted consent record.
///
/// # Errors
///
/// Returns `wear_sampling_wrong_monitor` when the native frame dimensions differ.
pub fn reconcile_reattached_frame(
    frame: &RawFrame,
    binding: &ConsentBinding<'_>,
) -> Result<(), CaptureError> {
    if (frame.width, frame.height) != (binding.granted_width, binding.granted_height) {
        return Err(CaptureError::Protocol(
            WEAR_SAMPLING_WRONG_MONITOR.to_owned(),
        ));
    }
    Ok(())
}

struct FrameState {
    format: spa::param::video::VideoInfoRaw,
    reply: std::sync::mpsc::Sender<Result<RawFrame, CaptureError>>,
}

fn acquire_one_frame(fd: OwnedFd, node_id: u32) -> Result<RawFrame, CaptureError> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|error| CaptureError::Transport(format!("PipeWire main loop: {error}")))?;
    let context = pw::context::ContextRc::new(&mainloop, None)
        .map_err(|error| CaptureError::Transport(format!("PipeWire context: {error}")))?;
    let core = context
        .connect_fd_rc(fd, None)
        .map_err(|error| CaptureError::Transport(format!("PipeWire portal remote: {error}")))?;
    let stream = pw::stream::StreamRc::new(
        core,
        "dormant-active-sampling",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|error| CaptureError::Transport(format!("PipeWire stream: {error}")))?;
    let (reply, frame) = std::sync::mpsc::channel();
    let loop_for_process = mainloop.clone();
    let _listener = stream
        .add_local_listener_with_user_data(FrameState {
            format: spa::param::video::VideoInfoRaw::default(),
            reply,
        })
        .param_changed(|_, state, id, param| {
            let Some(param) = param else {
                return;
            };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = pw::spa::param::format_utils::parse_format(param)
            else {
                return;
            };
            if media_type == pw::spa::param::format::MediaType::Video
                && media_subtype == pw::spa::param::format::MediaSubtype::Raw
            {
                let _ = state.format.parse(param);
            }
        })
        .process(move |stream, state| {
            let result = capture_buffer(stream, &state.format);
            let _ = state.reply.send(result);
            loop_for_process.quit();
        })
        .register()
        .map_err(|error| CaptureError::Transport(format!("PipeWire stream listener: {error}")))?;
    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut [],
        )
        .map_err(|error| CaptureError::Transport(format!("PipeWire stream connect: {error}")))?;
    mainloop.run();
    frame.recv().map_err(|error| {
        CaptureError::Transport(format!("PipeWire ended before delivering a frame: {error}"))
    })?
}

fn capture_buffer(
    stream: &pw::stream::Stream,
    format: &spa::param::video::VideoInfoRaw,
) -> Result<RawFrame, CaptureError> {
    let mut buffer = stream.dequeue_buffer().ok_or_else(|| {
        CaptureError::Transport("PipeWire process callback had no buffer".to_owned())
    })?;
    let data = buffer
        .datas_mut()
        .first_mut()
        .ok_or_else(|| CaptureError::Transport("PipeWire frame had no data plane".to_owned()))?;
    let size = usize::try_from(data.chunk().size()).map_err(|_| {
        CaptureError::Protocol("PipeWire frame chunk size overflows usize".to_owned())
    })?;
    if size == 0 {
        return Err(CaptureError::Transport(
            "PipeWire frame chunk is empty".to_owned(),
        ));
    }
    let stride = usize::try_from(data.chunk().stride())
        .map_err(|_| CaptureError::Protocol("PipeWire frame stride is negative".to_owned()))?;
    let offset = usize::try_from(data.chunk().offset()).map_err(|_| {
        CaptureError::Protocol("PipeWire frame chunk offset overflows usize".to_owned())
    })?;
    let input = data.data().ok_or_else(|| {
        CaptureError::Transport("PipeWire frame data is not memory mapped".to_owned())
    })?;
    let pixels = input.get(offset..offset + size).ok_or_else(|| {
        CaptureError::Protocol("PipeWire frame chunk exceeds mapped data".to_owned())
    })?;
    let width = format.size().width;
    let height = format.size().height;
    rgba_from_pipewire(pixels, width, height, stride, format.format())
}

fn rgba_from_pipewire(
    input: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    format: spa::param::video::VideoFormat,
) -> Result<RawFrame, CaptureError> {
    let width = usize::try_from(width)
        .map_err(|_| CaptureError::Protocol("PipeWire frame width overflows usize".to_owned()))?;
    let height = usize::try_from(height)
        .map_err(|_| CaptureError::Protocol("PipeWire frame height overflows usize".to_owned()))?;
    let row_bytes = width.checked_mul(4).ok_or_else(|| {
        CaptureError::Protocol("PipeWire frame row byte count overflows usize".to_owned())
    })?;
    if stride < row_bytes || input.len() < stride.saturating_mul(height) {
        return Err(CaptureError::Protocol(
            "PipeWire frame data is shorter than its dimensions".to_owned(),
        ));
    }
    let mut rgba = Vec::with_capacity(row_bytes.saturating_mul(height));
    for row in input.chunks_exact(stride).take(height) {
        for pixel in row[..row_bytes].chunks_exact(4) {
            let converted = if format == spa::param::video::VideoFormat::RGBA {
                [pixel[0], pixel[1], pixel[2], pixel[3]]
            } else if format == spa::param::video::VideoFormat::RGBx {
                [pixel[0], pixel[1], pixel[2], u8::MAX]
            } else if format == spa::param::video::VideoFormat::BGRA {
                [pixel[2], pixel[1], pixel[0], pixel[3]]
            } else if format == spa::param::video::VideoFormat::BGRx {
                [pixel[2], pixel[1], pixel[0], u8::MAX]
            } else {
                return Err(CaptureError::Protocol(
                    "PipeWire returned an unsupported video format".to_owned(),
                ));
            };
            rgba.extend_from_slice(&converted);
        }
    }
    Ok(RawFrame {
        rgba,
        width: u32::try_from(width)
            .map_err(|_| CaptureError::Protocol("PipeWire width exceeds u32".to_owned()))?,
        height: u32::try_from(height)
            .map_err(|_| CaptureError::Protocol("PipeWire height exceeds u32".to_owned()))?,
        stride: row_bytes,
    })
}

/// Real session-bus transport. The connection is owned so the portal session
/// remains valid while `PipeWire` consumes its private remote fd.
pub struct ZbusPortalTransport {
    connection: zbus::Connection,
    next_token: AtomicU64,
}

impl ZbusPortalTransport {
    /// Connects to the session bus once for the source lifetime.
    ///
    /// # Errors
    ///
    /// Returns a transport error when the session bus cannot be reached.
    pub async fn new() -> Result<Self, CaptureError> {
        let connection = zbus::Connection::session()
            .await
            .map_err(|error| CaptureError::Transport(format!("portal session bus: {error}")))?;
        Ok(Self {
            connection,
            next_token: AtomicU64::new(1),
        })
    }

    fn token(&self, kind: &str) -> String {
        let value = self.next_token.fetch_add(1, Ordering::Relaxed);
        format!("dormant_{kind}_{value}")
    }

    async fn screencast_proxy(&self) -> Result<zbus::Proxy<'_>, CaptureError> {
        zbus::Proxy::new(
            &self.connection,
            PORTAL_SERVICE,
            PORTAL_PATH,
            SCREENCAST_INTERFACE,
        )
        .await
        .map_err(|error| CaptureError::Transport(format!("portal proxy: {error}")))
    }

    fn request_path(&self, token: &str) -> Result<OwnedObjectPath, CaptureError> {
        let sender = self.connection.unique_name().ok_or_else(|| {
            CaptureError::Transport("portal session bus has no unique name".to_owned())
        })?;
        OwnedObjectPath::try_from(format!(
            "/org/freedesktop/portal/desktop/request/{}/{token}",
            sender.as_str().trim_start_matches(':').replace('.', "_")
        ))
        .map_err(|error| CaptureError::Protocol(format!("portal request path: {error}")))
    }

    async fn response(
        &self,
        request_path: &OwnedObjectPath,
    ) -> Result<HashMap<String, OwnedValue>, CaptureError> {
        use futures_util::StreamExt;

        let request = zbus::Proxy::new(
            &self.connection,
            PORTAL_SERVICE,
            request_path,
            REQUEST_INTERFACE,
        )
        .await
        .map_err(|error| CaptureError::Transport(format!("portal request proxy: {error}")))?;
        let mut responses = request.receive_signal("Response").await.map_err(|error| {
            CaptureError::Transport(format!("portal signal subscribe: {error}"))
        })?;
        let response = responses
            .next()
            .await
            .ok_or_else(|| CaptureError::Transport("portal response stream ended".to_owned()))?;
        let (code, results): (u32, HashMap<String, OwnedValue>) = response
            .body()
            .deserialize()
            .map_err(|error| CaptureError::Protocol(format!("portal Response decode: {error}")))?;
        match code {
            0 => Ok(results),
            1 => Err(CaptureError::ConsentDenied),
            2 => Err(CaptureError::Protocol(
                "portal request failed: response=2".to_owned(),
            )),
            other => Err(CaptureError::Protocol(format!(
                "portal request failed: response={other}"
            ))),
        }
    }
}

#[async_trait]
impl PortalTransport for ZbusPortalTransport {
    async fn create_session(&self) -> Result<PortalSession, CaptureError> {
        let handle_token = self.token("create");
        let session_handle_token = self.token("session");
        let expected_path = self.request_path(&handle_token)?;
        let mut options = HashMap::new();
        options.insert("handle_token".to_owned(), Value::Str(handle_token.into()));
        options.insert(
            "session_handle_token".to_owned(),
            Value::Str(session_handle_token.into()),
        );
        let proxy = self.screencast_proxy().await?;
        let response = self.response(&expected_path);
        let returned: OwnedObjectPath = proxy
            .call("CreateSession", &(options,))
            .await
            .map_err(|error| CaptureError::Transport(format!("portal CreateSession: {error}")))?;
        if returned != expected_path {
            return Err(CaptureError::Protocol(
                "portal CreateSession returned an unexpected request path".to_owned(),
            ));
        }
        let results = response.await?;
        let session_handle = results
            .get("session_handle")
            .ok_or_else(|| {
                CaptureError::Protocol("portal CreateSession omitted session_handle".to_owned())
            })?
            .downcast_ref::<&str>()
            .map_err(|error| CaptureError::Protocol(format!("portal session_handle: {error}")))?;
        let path = OwnedObjectPath::try_from(session_handle)
            .map_err(|error| CaptureError::Protocol(format!("portal session path: {error}")))?;
        Ok(PortalSession { path })
    }

    async fn select_sources(
        &self,
        session: &PortalSession,
        options: SelectSourcesOptions,
    ) -> Result<(), CaptureError> {
        let handle_token = self.token("select");
        let expected_path = self.request_path(&handle_token)?;
        let mut values = HashMap::new();
        values.insert("handle_token".to_owned(), Value::Str(handle_token.into()));
        values.insert("types".to_owned(), Value::U32(options.types.0));
        values.insert("multiple".to_owned(), Value::Bool(options.multiple));
        values.insert("cursor_mode".to_owned(), Value::U32(options.cursor_mode.0));
        values.insert("persist_mode".to_owned(), Value::U32(options.persist_mode));
        if let Some(token) = options.restore_token {
            values.insert("restore_token".to_owned(), Value::Str(token.into()));
        }
        let proxy = self.screencast_proxy().await?;
        let response = self.response(&expected_path);
        let returned: OwnedObjectPath = proxy
            .call("SelectSources", &(&session.path, values))
            .await
            .map_err(|error| CaptureError::Transport(format!("portal SelectSources: {error}")))?;
        if returned != expected_path {
            return Err(CaptureError::Protocol(
                "portal SelectSources returned an unexpected request path".to_owned(),
            ));
        }
        response.await.map(|_| ())
    }

    async fn start(&self, session: &PortalSession) -> Result<PortalStartResult, CaptureError> {
        let handle_token = self.token("start");
        let expected_path = self.request_path(&handle_token)?;
        let mut options = HashMap::new();
        options.insert("handle_token".to_owned(), Value::Str(handle_token.into()));
        let proxy = self.screencast_proxy().await?;
        let response = self.response(&expected_path);
        let returned: OwnedObjectPath = proxy
            .call("Start", &(&session.path, "", options))
            .await
            .map_err(|error| CaptureError::Transport(format!("portal Start: {error}")))?;
        if returned != expected_path {
            return Err(CaptureError::Protocol(
                "portal Start returned an unexpected request path".to_owned(),
            ));
        }
        parse_start_result(response.await?)
    }

    async fn open_pipewire_remote(&self, session: &PortalSession) -> Result<OwnedFd, CaptureError> {
        let fd: ZbusOwnedFd = self
            .screencast_proxy()
            .await?
            .call(
                "OpenPipeWireRemote",
                &(&session.path, HashMap::<String, Value<'static>>::new()),
            )
            .await
            .map_err(|error| {
                CaptureError::Transport(format!("portal OpenPipeWireRemote: {error}"))
            })?;
        Ok(fd.into())
    }

    async fn close(&self, session: PortalSession) {
        let Ok(proxy) = zbus::Proxy::new(
            &self.connection,
            PORTAL_SERVICE,
            &session.path,
            SESSION_INTERFACE,
        )
        .await
        else {
            return;
        };
        let _ = proxy.call::<_, _, ()>("Close", &()).await;
    }
}

fn parse_start_result(
    mut results: HashMap<String, OwnedValue>,
) -> Result<PortalStartResult, CaptureError> {
    let restore_token: String = results
        .remove("restore_token")
        .ok_or_else(|| CaptureError::Protocol("portal Start omitted restore_token".to_owned()))?
        .try_into()
        .map_err(|error| CaptureError::Protocol(format!("portal restore_token: {error}")))?;
    let streams: Vec<(u32, HashMap<String, OwnedValue>)> = results
        .remove("streams")
        .ok_or_else(|| CaptureError::Protocol("portal Start omitted streams".to_owned()))?
        .try_into()
        .map_err(|error| CaptureError::Protocol(format!("portal streams: {error}")))?;
    let streams = streams
        .into_iter()
        .map(|(node_id, mut properties)| {
            let (width, height): (i32, i32) = properties
                .remove("size")
                .ok_or_else(|| CaptureError::Protocol("portal stream omitted size".to_owned()))?
                .try_into()
                .map_err(|error| CaptureError::Protocol(format!("portal stream size: {error}")))?;
            Ok(PortalStream {
                node_id,
                width: u32::try_from(width).map_err(|_| {
                    CaptureError::Protocol("portal stream width is negative".to_owned())
                })?,
                height: u32::try_from(height).map_err(|_| {
                    CaptureError::Protocol("portal stream height is negative".to_owned())
                })?,
                persistent_id: properties
                    .remove("id")
                    .map(|value| {
                        value.try_into().map_err(|error| {
                            CaptureError::Protocol(format!("portal stream id: {error}"))
                        })
                    })
                    .transpose()?,
            })
        })
        .collect::<Result<Vec<_>, CaptureError>>()?;
    Ok(PortalStartResult {
        streams,
        restore_token,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum PortalCall {
        CreateSession {
            handle_token: String,
            session_handle_token: String,
        },
        SelectSources {
            options: SelectSourcesOptions,
        },
        Start {
            handle_token: String,
        },
        OpenPipeWireRemote,
        Close,
    }

    #[derive(Clone)]
    struct FakePortalTransport {
        state: Arc<Mutex<FakeState>>,
    }

    struct FakeState {
        calls: Vec<PortalCall>,
        start: Result<PortalStartResult, CaptureError>,
    }

    impl FakePortalTransport {
        fn grant_with(start: PortalStartResult) -> Self {
            Self {
                state: Arc::new(Mutex::new(FakeState {
                    calls: Vec::new(),
                    start: Ok(start),
                })),
            }
        }

        fn calls(&self) -> Vec<PortalCall> {
            self.state
                .lock()
                .expect("fake lock is not poisoned")
                .calls
                .clone()
        }
    }

    #[async_trait]
    impl PortalTransport for FakePortalTransport {
        async fn create_session(&self) -> Result<PortalSession, CaptureError> {
            self.state
                .lock()
                .expect("fake lock is not poisoned")
                .calls
                .push(PortalCall::CreateSession {
                    handle_token: "dormant_create_1".to_owned(),
                    session_handle_token: "dormant_session_2".to_owned(),
                });
            Ok(PortalSession::fake())
        }

        async fn select_sources(
            &self,
            _session: &PortalSession,
            options: SelectSourcesOptions,
        ) -> Result<(), CaptureError> {
            self.state
                .lock()
                .expect("fake lock is not poisoned")
                .calls
                .push(PortalCall::SelectSources { options });
            Ok(())
        }

        async fn start(&self, _session: &PortalSession) -> Result<PortalStartResult, CaptureError> {
            let mut state = self.state.lock().expect("fake lock is not poisoned");
            state.calls.push(PortalCall::Start {
                handle_token: "dormant_start_3".to_owned(),
            });
            state.start.clone()
        }

        async fn open_pipewire_remote(
            &self,
            _session: &PortalSession,
        ) -> Result<OwnedFd, CaptureError> {
            self.state
                .lock()
                .expect("fake lock is not poisoned")
                .calls
                .push(PortalCall::OpenPipeWireRemote);
            let (fd, _) = std::os::unix::net::UnixStream::pair()
                .map_err(|error| CaptureError::Transport(error.to_string()))?;
            Ok(fd.into())
        }

        async fn close(&self, _session: PortalSession) {
            self.state
                .lock()
                .expect("fake lock is not poisoned")
                .calls
                .push(PortalCall::Close);
        }
    }

    #[tokio::test]
    async fn active_sampling_protocol_grant_uses_exact_portal_options_and_rotates_token() {
        let transport = FakePortalTransport::grant_with(PortalStartResult::single(
            73,
            3072,
            1728,
            Some("persistent-output"),
            "rotated-token",
        ));
        let mut source = PortalPipeWireSource::from_transport_with_frames(
            transport.clone(),
            [Ok(RawFrame {
                rgba: vec![0; 4],
                width: 3840,
                height: 2160,
                stride: 3840 * 4,
            })],
        );

        let grant = source
            .request_consent(&DisplayExpectation {
                display: "oled".to_owned(),
            })
            .await
            .expect("scripted portal grant succeeds");

        assert_eq!(grant.stream.node_id, 73);
        assert_eq!(grant.stream.restore_token, "rotated-token");
        assert_eq!(
            grant.stream.persistent_id.as_deref(),
            Some("persistent-output")
        );
        assert_eq!((grant.stream.width, grant.stream.height), (3072, 1728));
        assert_eq!(
            (grant.stream.frame_width, grant.stream.frame_height),
            (3840, 2160)
        );
        assert_eq!(
            transport.calls(),
            vec![
                PortalCall::CreateSession {
                    handle_token: "dormant_create_1".to_owned(),
                    session_handle_token: "dormant_session_2".to_owned(),
                },
                PortalCall::SelectSources {
                    options: SelectSourcesOptions::for_grant(),
                },
                PortalCall::Start {
                    handle_token: "dormant_start_3".to_owned(),
                },
                PortalCall::OpenPipeWireRemote,
            ]
        );
    }

    #[test]
    fn active_sampling_protocol_rejects_wrong_native_frame_with_literal_reason() {
        let frame = RawFrame {
            rgba: vec![],
            width: 3840,
            height: 2160,
            stride: 3840 * 4,
        };
        let binding = ConsentBinding {
            token: "saved",
            sampled_display: "oled",
            portal_persistent_ids: &[],
            granted_width: 3072,
            granted_height: 1728,
        };

        assert_eq!(
            reconcile_reattached_frame(&frame, &binding),
            Err(CaptureError::Protocol(
                WEAR_SAMPLING_WRONG_MONITOR.to_owned()
            ))
        );
    }

    #[test]
    fn active_sampling_protocol_allows_logical_start_size_to_differ_from_native_record() {
        let stream = connected_stream(PortalStartResult::single(
            7,
            3072,
            1728,
            Some("persistent-output"),
            "token",
        ))
        .expect("start metadata is valid");
        let persistent_ids = vec!["persistent-output".to_owned()];
        let binding = ConsentBinding {
            token: "saved",
            sampled_display: "oled",
            portal_persistent_ids: &persistent_ids,
            granted_width: 3840,
            granted_height: 2160,
        };

        assert_eq!(reconcile_start_with_binding(&stream, &binding), Ok(()));
    }
}
