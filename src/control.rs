#![cfg(unix)]

use std::{
    env, fs,
    io::{self, Read, Write},
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

use crate::config::McpAccess;

pub const CONTROL_PROTOCOL_VERSION: u32 = 1;
pub const MAX_CONTROL_FRAME_BYTES: usize = 8 * 1024 * 1024;
const CONTROL_QUEUE: usize = 64;
const IO_POLL_INTERVAL: Duration = Duration::from_millis(100);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(65);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstanceInfo {
    pub protocol_version: u32,
    pub instance_id: String,
    pub scope_id: String,
    pub pid: u32,
    pub config_path: PathBuf,
    pub socket_path: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PaneKind {
    Task,
    ConfigTerminal,
    SessionTerminal,
    Command,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaneInfo {
    pub pane_id: String,
    pub name: String,
    pub kind: PaneKind,
    pub status: String,
    pub pid: Option<u32>,
    pub cwd: PathBuf,
    pub generation: u64,
    pub accepts_input: bool,
    pub first_line: u64,
    pub next_line: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutputPage {
    pub pane_id: String,
    pub first_line: u64,
    pub next_cursor: String,
    pub end_cursor: String,
    pub truncated_before_cursor: bool,
    pub lines: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchMatch {
    pub line: u64,
    pub text: String,
    pub before: Vec<String>,
    pub after: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchResults {
    pub pane_id: String,
    pub query: String,
    pub matches: Vec<SearchMatch>,
    pub total_matches: usize,
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WaitResult {
    pub pane_id: String,
    pub matched: bool,
    pub timed_out: bool,
    pub status: String,
    pub cursor: String,
    pub line: Option<u64>,
}

#[derive(
    Clone, Copy, Debug, Default, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
pub enum CaptureView {
    #[default]
    Workspace,
    Full,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CaptureResult {
    pub view: CaptureView,
    pub columns: u16,
    pub rows: u16,
    pub width: u32,
    pub height: u32,
    pub font: String,
    pub missing_glyphs: usize,
    pub png: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlRequest {
    Ping,
    ListPanes,
    ReadOutput {
        pane_id: String,
        cursor: Option<String>,
        max_lines: u32,
    },
    SearchOutput {
        pane_id: String,
        query: String,
        max_results: u32,
        context_lines: u32,
    },
    WaitForOutput {
        pane_id: String,
        query: Option<String>,
        status: Option<String>,
        after_cursor: Option<String>,
        timeout_ms: u64,
    },
    RestartTask {
        pane_id: String,
    },
    RestartAll,
    InterruptPane {
        pane_id: String,
    },
    SendInput {
        pane_id: String,
        text: String,
        submit: bool,
    },
    RunCommand {
        command: String,
        cwd: Option<PathBuf>,
        name: Option<String>,
    },
    WaitForCommand {
        pane_id: String,
        timeout_ms: u64,
    },
    CloseCommand {
        pane_id: String,
    },
    CaptureTui {
        view: CaptureView,
    },
}

impl ControlRequest {
    pub fn requires_write(&self) -> bool {
        matches!(
            self,
            Self::RestartTask { .. }
                | Self::RestartAll
                | Self::InterruptPane { .. }
                | Self::SendInput { .. }
                | Self::RunCommand { .. }
                | Self::CloseCommand { .. }
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlResponse {
    Instance { instance: InstanceInfo },
    Panes { panes: Vec<PaneInfo> },
    Output { output: OutputPage },
    Search { results: SearchResults },
    Wait { result: WaitResult },
    Capture { capture: CaptureResult },
    Command { pane: PaneInfo },
    Ok { message: String },
    Error { code: String, message: String },
}

impl ControlResponse {
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Error {
            code: code.into(),
            message: message.into(),
        }
    }
}

pub struct ControlEnvelope {
    pub request: ControlRequest,
    pub reply: SyncSender<ControlResponse>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CursorToken {
    version: u8,
    instance_id: String,
    pane_id: String,
    line: u64,
}

pub fn encode_cursor(instance_id: &str, pane_id: &str, line: u64) -> String {
    let token = CursorToken {
        version: 1,
        instance_id: instance_id.to_owned(),
        pane_id: pane_id.to_owned(),
        line,
    };
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(&token).expect("cursor token serializes"))
}

pub fn decode_cursor(token: &str, instance_id: &str, pane_id: &str) -> Result<u64> {
    let bytes = URL_SAFE_NO_PAD
        .decode(token)
        .context("cursor is not a valid Demons cursor")?;
    let cursor: CursorToken =
        serde_json::from_slice(&bytes).context("cursor is not a valid Demons cursor")?;
    if cursor.version != 1 || cursor.instance_id != instance_id || cursor.pane_id != pane_id {
        bail!("cursor belongs to a different Demons instance or pane");
    }
    Ok(cursor.line)
}

pub struct ControlListener {
    pub info: InstanceInfo,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    discovery_path: PathBuf,
}

impl ControlListener {
    pub fn start(scope_id: &str, config_path: &Path) -> Result<(Self, Receiver<ControlEnvelope>)> {
        uuid::Uuid::parse_str(scope_id).context("invalid MCP project scope ID")?;
        let runtime_dir = runtime_dir()?;
        let instance_id = uuid::Uuid::new_v4().to_string();
        let socket_path = runtime_dir.join(format!("{instance_id}.sock"));
        let discovery_path = runtime_dir.join(format!("{scope_id}-{instance_id}.json"));
        remove_owned_file_if_present(&socket_path)?;
        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("failed to bind {}", socket_path.display()))?;
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;

        let info = InstanceInfo {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            instance_id,
            scope_id: scope_id.to_owned(),
            pid: std::process::id(),
            config_path: normalized_path(config_path),
            socket_path: socket_path.clone(),
        };
        write_private_json(&discovery_path, &info)?;

        let (tx, rx) = mpsc::sync_channel(CONTROL_QUEUE);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread_info = info.clone();
        let thread = thread::Builder::new()
            .name("demons-control-listener".to_owned())
            .spawn(move || listener_loop(listener, tx, thread_stop, thread_info))
            .context("failed to start MCP control listener")?;

        Ok((
            Self {
                info,
                stop,
                thread: Some(thread),
                discovery_path,
            },
            rx,
        ))
    }
}

impl Drop for ControlListener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().ok();
        }
        remove_owned_file_if_present(&self.info.socket_path).ok();
        remove_owned_file_if_present(&self.discovery_path).ok();
    }
}

fn listener_loop(
    listener: UnixListener,
    tx: SyncSender<ControlEnvelope>,
    stop: Arc<AtomicBool>,
    info: InstanceInfo,
) {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                if !peer_is_current_user(&stream) {
                    continue;
                }
                let connection_tx = tx.clone();
                let connection_stop = Arc::clone(&stop);
                let connection_info = info.clone();
                thread::Builder::new()
                    .name("demons-control-client".to_owned())
                    .spawn(move || {
                        connection_loop(stream, connection_tx, connection_stop, connection_info)
                    })
                    .ok();
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(_) => break,
        }
    }
}

fn connection_loop(
    mut stream: UnixStream,
    tx: SyncSender<ControlEnvelope>,
    stop: Arc<AtomicBool>,
    info: InstanceInfo,
) {
    stream.set_read_timeout(Some(IO_POLL_INTERVAL)).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
    while !stop.load(Ordering::Acquire) {
        let request = match read_frame::<ControlRequest>(&mut stream) {
            Ok(Some(request)) => request,
            Ok(None) => break,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            Err(_) => break,
        };
        if matches!(request, ControlRequest::Ping) {
            if write_frame(
                &mut stream,
                &ControlResponse::Instance {
                    instance: info.clone(),
                },
            )
            .is_err()
            {
                break;
            }
            continue;
        }
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        if tx
            .try_send(ControlEnvelope {
                request,
                reply: reply_tx,
            })
            .is_err()
        {
            let response = ControlResponse::error("busy", "Demons control queue is full");
            if write_frame(&mut stream, &response).is_err() {
                break;
            }
            continue;
        }
        let response = loop {
            if stop.load(Ordering::Acquire) {
                break ControlResponse::error("disabled", "MCP access is disabled");
            }
            match reply_rx.recv_timeout(IO_POLL_INTERVAL) {
                Ok(response) => break response,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    break ControlResponse::error("unavailable", "Demons stopped the request");
                }
            }
        };
        if write_frame(&mut stream, &response).is_err() {
            break;
        }
    }
}

pub fn discover_instances(scope_id: &str, config_path: &Path) -> Result<Vec<InstanceInfo>> {
    uuid::Uuid::parse_str(scope_id).context("invalid MCP project scope ID")?;
    let config_path = normalized_path(config_path);
    let prefix = format!("{scope_id}-");
    let mut instances = Vec::new();
    for dir in runtime_dir_candidates()? {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", dir.display()));
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with(&prefix) || !name.ends_with(".json") {
                continue;
            }
            let path = entry.path();
            if !owned_regular_file(&path) {
                continue;
            }
            let info = match fs::read_to_string(&path)
                .ok()
                .and_then(|source| serde_json::from_str::<InstanceInfo>(&source).ok())
            {
                Some(info) => info,
                None => continue,
            };
            if info.scope_id != scope_id
                || info.protocol_version != CONTROL_PROTOCOL_VERSION
                || normalized_path(&info.config_path) != config_path
            {
                continue;
            }
            if !process_is_alive(info.pid) || !owned_socket(&info.socket_path) {
                remove_owned_file_if_present(&path).ok();
                remove_owned_file_if_present(&info.socket_path).ok();
                continue;
            }
            if !instances
                .iter()
                .any(|existing: &InstanceInfo| existing.instance_id == info.instance_id)
            {
                instances.push(info);
            }
        }
    }
    instances.sort_by(|left, right| left.instance_id.cmp(&right.instance_id));
    Ok(instances)
}

fn normalized_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub fn request(instance: &InstanceInfo, request: &ControlRequest) -> Result<ControlResponse> {
    if !owned_socket(&instance.socket_path) {
        bail!("Demons control socket is missing or not owned by this user");
    }
    let mut stream = UnixStream::connect(&instance.socket_path)
        .with_context(|| format!("failed to connect to {}", instance.socket_path.display()))?;
    stream.set_read_timeout(Some(RESPONSE_TIMEOUT))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    write_frame(&mut stream, request)?;
    read_frame(&mut stream)?.context("Demons closed the control connection")
}

pub fn authorize(access: McpAccess, request: &ControlRequest) -> Result<()> {
    if !access.allows_read() {
        bail!("MCP access is disabled");
    }
    if request.requires_write() && !access.allows_write() {
        bail!("MCP write access is disabled");
    }
    Ok(())
}

fn runtime_dir() -> Result<PathBuf> {
    let candidates = runtime_dir_candidates()?;
    let dir = candidates
        .first()
        .cloned()
        .context("no safe runtime directory is available")?;
    let uid = unsafe { libc::geteuid() };
    ensure_private_dir(&dir, uid)?;
    Ok(dir)
}

fn runtime_dir_candidates() -> Result<Vec<PathBuf>> {
    let uid = unsafe { libc::geteuid() };
    let mut bases = Vec::new();
    if let Some(path) = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        bases.push(path);
    }
    #[cfg(target_os = "linux")]
    {
        let path = PathBuf::from(format!("/run/user/{uid}"));
        if path.is_dir() && !bases.contains(&path) {
            bases.push(path);
        }
    }
    let fallback = env::temp_dir().join(format!("demons-{uid}"));
    if !bases.contains(&fallback) {
        bases.push(fallback);
    }

    let mut directories = Vec::new();
    for base in bases {
        if ensure_private_dir(&base, uid).is_err() {
            continue;
        }
        let directory = base.join("demons");
        if ensure_private_dir(&directory, uid).is_ok() {
            directories.push(directory);
        }
    }
    if directories.is_empty() {
        bail!("no safe runtime directory is available");
    }
    Ok(directories)
}

fn ensure_private_dir(path: &Path, uid: u32) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                bail!("{} is not a safe runtime directory", path.display());
            }
            if metadata.uid() != uid {
                bail!("{} is not owned by the current user", path.display());
            }
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder
                .create(path)
                .with_context(|| format!("failed to create {}", path.display()))?;
            let metadata = fs::symlink_metadata(path)?;
            if metadata.uid() != uid || !metadata.file_type().is_dir() {
                bail!("{} is not a safe runtime directory", path.display());
            }
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    }
    Ok(())
}

fn write_private_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    fs::write(path, bytes).with_context(|| format!("failed to write {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn remove_owned_file_if_present(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || metadata.uid() != unsafe { libc::geteuid() } {
                bail!("refusing to remove unsafe runtime path {}", path.display());
            }
            fs::remove_file(path).with_context(|| format!("failed to remove {}", path.display()))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

fn owned_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.uid() == unsafe { libc::geteuid() }
            && metadata.file_type().is_file()
            && !metadata.file_type().is_symlink()
    })
}

fn owned_socket(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.uid() == unsafe { libc::geteuid() }
            && metadata.file_type().is_socket()
            && !metadata.file_type().is_symlink()
    })
}

fn process_is_alive(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(target_os = "linux")]
fn peer_is_current_user(stream: &UnixStream) -> bool {
    use std::mem::{size_of, zeroed};
    use std::os::fd::AsRawFd;
    let mut credentials: libc::ucred = unsafe { zeroed() };
    let mut length = size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    result == 0 && credentials.uid == unsafe { libc::geteuid() }
}

#[cfg(target_os = "macos")]
fn peer_is_current_user(stream: &UnixStream) -> bool {
    use std::os::fd::AsRawFd;
    let mut uid = 0;
    let mut gid = 0;
    let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    result == 0 && uid == unsafe { libc::geteuid() }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn peer_is_current_user(_stream: &UnixStream) -> bool {
    false
}

fn write_frame<T: Serialize>(stream: &mut UnixStream, value: &T) -> io::Result<()> {
    let payload = serde_json::to_vec(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if payload.len() > MAX_CONTROL_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control frame is too large",
        ));
    }
    let length = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "control frame is too large"))?;
    stream.write_all(&length.to_be_bytes())?;
    stream.write_all(&payload)?;
    stream.flush()
}

fn read_frame<T: for<'de> Deserialize<'de>>(stream: &mut UnixStream) -> io::Result<Option<T>> {
    let mut length = [0_u8; 4];
    match stream.read_exact(&mut length) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_CONTROL_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control frame is too large",
        ));
    }
    let mut payload = vec![0_u8; length];
    stream.read_exact(&mut payload)?;
    serde_json::from_slice(&payload)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorization_distinguishes_read_and_write_access() {
        assert!(authorize(McpAccess::Off, &ControlRequest::ListPanes).is_err());
        assert!(authorize(McpAccess::ReadOnly, &ControlRequest::ListPanes).is_ok());
        assert!(authorize(McpAccess::ReadOnly, &ControlRequest::RestartAll).is_err());
        assert!(authorize(McpAccess::Full, &ControlRequest::RestartAll).is_ok());
    }

    #[test]
    fn control_frames_reject_oversized_payloads() {
        let request = ControlRequest::SendInput {
            pane_id: "pane".to_owned(),
            text: "x".repeat(MAX_CONTROL_FRAME_BYTES),
            submit: false,
        };
        let (mut left, _right) = UnixStream::pair().unwrap();
        assert_eq!(
            write_frame(&mut left, &request).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn cursors_are_bound_to_instance_and_pane() {
        let cursor = encode_cursor("instance-a", "server", 42);
        assert_eq!(decode_cursor(&cursor, "instance-a", "server").unwrap(), 42);
        assert!(decode_cursor(&cursor, "instance-b", "server").is_err());
        assert!(decode_cursor(&cursor, "instance-a", "client").is_err());
    }
}
