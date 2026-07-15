use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque, hash_map::DefaultHasher},
    fs,
    hash::{Hash, Hasher},
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result, bail};
use notify::{Config as NotifyConfig, Event, RecommendedWatcher, RecursiveMode, Watcher};
use walkdir::{DirEntry, WalkDir};

use crate::config::{Config, Task, WatchMode, parse_watch_poll_interval, resolve_from_task_cwd};

const RAW_EVENT_CAPACITY: usize = 1_024;
const OUTPUT_EVENT_CAPACITY: usize = 256;
const LOOP_INTERVAL: Duration = Duration::from_millis(50);
const SENTINEL_INTERVAL: Duration = Duration::from_secs(10);
const MAX_EVENT_PATHS: usize = 16;
const MAX_PENDING_NOTICES: usize = 32;
const MAX_NATIVE_SEEN_PATHS: usize = 4_096;
const MAX_SNAPSHOT_ENTRIES: usize = 250_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchEventSource {
    Native,
    Polling,
    Sentinel,
    Overflow,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchEvent {
    Changed {
        task: String,
        paths: Vec<PathBuf>,
        source: WatchEventSource,
        overflow: bool,
    },
    Warning {
        task: Option<String>,
        message: String,
    },
    Fatal(String),
}

pub struct WatchService {
    events: Receiver<WatchEvent>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl WatchService {
    pub fn start(root: &Path, config: &Config, enabled: bool) -> Result<Option<Self>> {
        if !enabled {
            return Ok(None);
        }
        let specs = build_specs(root, config)?;
        if specs.is_empty() {
            return Ok(None);
        }

        let poll_interval = parse_watch_poll_interval(&config.settings.watch_poll_interval)?;
        let mode = config.settings.watch_mode;
        let (event_tx, event_rx) = mpsc::sync_channel(OUTPUT_EVENT_CAPACITY);
        let (startup_tx, startup_rx) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("demons-file-watch".to_owned())
            .spawn(move || {
                let coordinator = Coordinator::new(specs, mode, poll_interval, event_tx);
                match coordinator {
                    Ok(mut coordinator) => {
                        let _ = startup_tx.send(Ok::<(), String>(()));
                        coordinator.run(&thread_stop);
                    }
                    Err(error) => {
                        let _ = startup_tx.send(Err(format!("{error:#}")));
                    }
                }
            })
            .context("failed to start file watcher thread")?;

        match startup_rx.recv() {
            Ok(Ok(())) => Ok(Some(Self {
                events: event_rx,
                stop,
                thread: Some(thread),
            })),
            Ok(Err(error)) => {
                let _ = thread.join();
                bail!("failed to start file watcher: {error}")
            }
            Err(_) => {
                let _ = thread.join();
                bail!("file watcher stopped during startup")
            }
        }
    }

    pub fn try_recv(&self) -> Result<Option<WatchEvent>, TryRecvError> {
        match self.events.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

impl Drop for WatchService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Clone, Debug)]
struct TaskWatchSpec {
    name: String,
    cwd: PathBuf,
    roots: Vec<WatchRoot>,
    ignores: Vec<IgnorePath>,
    polling: bool,
    sentinel: bool,
    snapshot: Snapshot,
    next_check: Instant,
    native_seen: BTreeSet<PathBuf>,
    native_seen_overflow: bool,
}

#[derive(Clone, Debug)]
struct WatchRoot {
    configured: PathBuf,
    target: PathBuf,
    target_is_dir: bool,
}

impl WatchRoot {
    fn load(cwd: &Path, configured: &Path) -> Result<Self> {
        let configured = normalize_path(&resolve_from_task_cwd(cwd, configured));
        let target = fs::canonicalize(&configured)
            .with_context(|| format!("watched path does not exist: {}", configured.display()))?;
        let metadata = fs::metadata(&target)
            .with_context(|| format!("cannot inspect watched path {}", configured.display()))?;
        if !metadata.is_file() && !metadata.is_dir() {
            bail!(
                "watched path is not a file or directory: {}",
                configured.display()
            );
        }
        Ok(Self {
            configured,
            target: normalize_path(&target),
            target_is_dir: metadata.is_dir(),
        })
    }

    fn matches(&self, path: &Path) -> bool {
        if self.target_is_dir {
            path.starts_with(&self.configured) || path.starts_with(&self.target)
        } else {
            path == self.configured || path == self.target
        }
    }

    fn structurally_matches(&self, path: &Path) -> bool {
        path == self.configured || path == self.target
    }
}

#[derive(Clone, Debug)]
struct IgnorePath {
    configured: PathBuf,
    target: Option<PathBuf>,
}

impl IgnorePath {
    fn load(cwd: &Path, configured: &Path) -> Self {
        let configured = normalize_path(&resolve_from_task_cwd(cwd, configured));
        let target = fs::canonicalize(&configured)
            .ok()
            .map(|path| normalize_path(&path));
        Self { configured, target }
    }

    fn matches(&self, display_path: &Path, source_path: &Path) -> bool {
        display_path.starts_with(&self.configured)
            || source_path.starts_with(&self.configured)
            || self.target.as_ref().is_some_and(|target| {
                display_path.starts_with(target) || source_path.starts_with(target)
            })
    }
}

fn build_specs(root: &Path, config: &Config) -> Result<Vec<TaskWatchSpec>> {
    let now = Instant::now();
    config
        .tasks
        .iter()
        .filter(|task| !task.watch.is_empty())
        .map(|task| build_spec(root, task, now))
        .collect()
}

fn build_spec(root: &Path, task: &Task, now: Instant) -> Result<TaskWatchSpec> {
    let cwd = if task.cwd.is_absolute() {
        task.cwd.clone()
    } else {
        root.join(&task.cwd)
    };
    let cwd = fs::canonicalize(&cwd)
        .with_context(|| format!("cannot resolve cwd for watched task {:?}", task.name))?;
    let roots = task
        .watch
        .iter()
        .map(|path| WatchRoot::load(&cwd, path))
        .collect::<Result<Vec<_>>>()?;
    let ignores = task
        .watch_ignore
        .iter()
        .map(|path| IgnorePath::load(&cwd, path))
        .collect();
    Ok(TaskWatchSpec {
        name: task.name.clone(),
        cwd,
        roots,
        ignores,
        polling: false,
        sentinel: false,
        snapshot: Snapshot::new(),
        next_check: now,
        native_seen: BTreeSet::new(),
        native_seen_overflow: false,
    })
}

enum RawEvent {
    Notify(notify::Result<Event>),
}

struct Coordinator {
    specs: Vec<TaskWatchSpec>,
    mode: WatchMode,
    poll_interval: Duration,
    output: SyncSender<WatchEvent>,
    raw_tx: SyncSender<RawEvent>,
    raw_rx: Receiver<RawEvent>,
    raw_overflow: Arc<AtomicBool>,
    watcher: Option<RecommendedWatcher>,
    pending_changes: BTreeMap<String, PendingChange>,
    pending_notices: VecDeque<WatchEvent>,
}

#[derive(Clone, Debug)]
struct PendingChange {
    paths: BTreeSet<PathBuf>,
    source: WatchEventSource,
    overflow: bool,
}

impl Coordinator {
    fn new(
        specs: Vec<TaskWatchSpec>,
        mode: WatchMode,
        poll_interval: Duration,
        output: SyncSender<WatchEvent>,
    ) -> Result<Self> {
        let (raw_tx, raw_rx) = mpsc::sync_channel(RAW_EVENT_CAPACITY);
        let raw_overflow = Arc::new(AtomicBool::new(false));
        let mut coordinator = Self {
            specs,
            mode,
            poll_interval,
            output,
            raw_tx,
            raw_rx,
            raw_overflow,
            watcher: None,
            pending_changes: BTreeMap::new(),
            pending_notices: VecDeque::new(),
        };

        if mode == WatchMode::Polling {
            for spec in &mut coordinator.specs {
                spec.polling = true;
            }
        } else {
            coordinator.install_native_watcher(true)?;
        }

        for spec in &mut coordinator.specs {
            if mode == WatchMode::Auto
                && !spec.polling
                && spec
                    .roots
                    .iter()
                    .any(|root| suspicious_filesystem(&root.target))
            {
                spec.sentinel = true;
            }
            if spec.polling || spec.sentinel {
                spec.snapshot = snapshot_spec(spec)?;
                spec.next_check = Instant::now()
                    + if spec.polling {
                        poll_interval
                    } else {
                        SENTINEL_INTERVAL
                    };
            }
        }
        Ok(coordinator)
    }

    fn install_native_watcher(&mut self, startup: bool) -> Result<()> {
        let raw_tx = self.raw_tx.clone();
        let overflow = Arc::clone(&self.raw_overflow);
        let watcher = RecommendedWatcher::new(
            move |event| match raw_tx.try_send(RawEvent::Notify(event)) {
                Ok(()) | Err(TrySendError::Disconnected(_)) => {}
                Err(TrySendError::Full(_)) => overflow.store(true, Ordering::Release),
            },
            NotifyConfig::default(),
        );
        let mut watcher = match watcher {
            Ok(watcher) => watcher,
            Err(error) if self.mode == WatchMode::Auto => {
                for index in 0..self.specs.len() {
                    self.promote_to_polling(
                        index,
                        format!("native watcher unavailable; using polling ({error})"),
                    )?;
                }
                self.watcher = None;
                return Ok(());
            }
            Err(error) => return Err(error).context("native watcher is unavailable"),
        };

        let registrations = native_registrations(&self.specs);
        let mut failed_specs = HashMap::<usize, String>::new();
        for (path, registration) in registrations {
            let mode = if registration.recursive {
                RecursiveMode::Recursive
            } else {
                RecursiveMode::NonRecursive
            };
            if let Err(error) = watcher.watch(&path, mode) {
                if self.mode == WatchMode::Native {
                    return Err(error)
                        .with_context(|| format!("failed to watch {}", path.display()));
                }
                for index in registration.specs {
                    failed_specs.entry(index).or_insert_with(|| {
                        format!(
                            "cannot watch {} natively; using polling ({error})",
                            path.display()
                        )
                    });
                }
            }
        }
        for (index, warning) in failed_specs {
            self.promote_to_polling(index, warning)?;
        }

        if self.specs.iter().all(|spec| spec.polling) {
            self.watcher = None;
        } else {
            self.watcher = Some(watcher);
        }
        if !startup {
            self.try_flush();
        }
        Ok(())
    }

    fn promote_to_polling(&mut self, index: usize, warning: String) -> Result<()> {
        let Some(spec) = self.specs.get_mut(index) else {
            return Ok(());
        };
        if spec.polling {
            return Ok(());
        }
        spec.polling = true;
        spec.sentinel = false;
        spec.snapshot = snapshot_spec(spec)?;
        spec.next_check = Instant::now() + self.poll_interval;
        let task = spec.name.clone();
        self.queue_notice(WatchEvent::Warning {
            task: Some(task),
            message: warning,
        });
        Ok(())
    }

    fn run(&mut self, stop: &AtomicBool) {
        self.try_flush();
        while !stop.load(Ordering::Acquire) {
            match self.raw_rx.recv_timeout(LOOP_INTERVAL) {
                Ok(event) => self.handle_raw(event),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            while let Ok(event) = self.raw_rx.try_recv() {
                self.handle_raw(event);
            }
            if self.raw_overflow.swap(false, Ordering::AcqRel) {
                let indexes = self
                    .specs
                    .iter()
                    .enumerate()
                    .filter_map(|(index, spec)| (!spec.polling).then_some(index))
                    .collect::<Vec<_>>();
                for index in indexes {
                    self.queue_change(index, Vec::new(), WatchEventSource::Overflow, true);
                }
            }
            self.run_due_checks(stop);
            self.try_flush();
        }
    }

    fn handle_raw(&mut self, raw: RawEvent) {
        match raw {
            RawEvent::Notify(Ok(event)) => self.handle_native_event(event),
            RawEvent::Notify(Err(error)) => self.handle_native_error(error),
        }
    }

    fn handle_native_event(&mut self, event: Event) {
        if event.kind.is_access() {
            return;
        }
        if event.paths.is_empty() {
            let indexes = self
                .specs
                .iter()
                .enumerate()
                .filter_map(|(index, spec)| (!spec.polling).then_some(index))
                .collect::<Vec<_>>();
            for index in indexes {
                self.queue_change(index, Vec::new(), WatchEventSource::Native, true);
            }
            return;
        }

        let paths = event
            .paths
            .iter()
            .map(|path| normalize_path(path))
            .collect::<Vec<_>>();
        let mut changes = Vec::new();
        let mut rebuild = false;
        for (index, spec) in self.specs.iter_mut().enumerate() {
            if spec.polling {
                continue;
            }
            let matched = paths
                .iter()
                .filter(|path| spec.matches(path) && !spec.ignored(path, path))
                .cloned()
                .collect::<Vec<_>>();
            if !matched.is_empty() {
                for path in &matched {
                    if spec.native_seen.len() < MAX_NATIVE_SEEN_PATHS {
                        spec.native_seen.insert(path.clone());
                    } else {
                        spec.native_seen_overflow = true;
                    }
                }
                changes.push((index, matched));
            }
            if (event.kind.is_create() || event.kind.is_remove() || event.kind.is_modify())
                && paths.iter().any(|path| {
                    spec.roots
                        .iter()
                        .any(|root| root.structurally_matches(path))
                })
            {
                rebuild = true;
            }
        }
        for (index, paths) in changes {
            self.queue_change(index, paths, WatchEventSource::Native, false);
        }

        if rebuild && let Err(error) = self.rebuild_after_structure_change() {
            self.queue_notice(WatchEvent::Fatal(format!(
                "failed to rebuild native watcher: {error:#}"
            )));
        }
    }

    fn rebuild_after_structure_change(&mut self) -> Result<()> {
        for index in 0..self.specs.len() {
            if self.specs[index].polling {
                continue;
            }
            let cwd = self.specs[index].cwd.clone();
            for root in &mut self.specs[index].roots {
                let configured = root.configured.clone();
                match WatchRoot::load(&cwd, &configured) {
                    Ok(refreshed) => *root = refreshed,
                    Err(error) if self.mode == WatchMode::Auto => {
                        self.promote_to_polling(
                            index,
                            format!(
                                "watched path changed or disappeared; using polling ({error:#})"
                            ),
                        )?;
                        break;
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        self.watcher = None;
        if self.specs.iter().any(|spec| !spec.polling) {
            self.install_native_watcher(false)?;
        }
        Ok(())
    }

    fn handle_native_error(&mut self, error: notify::Error) {
        if self.mode == WatchMode::Native {
            self.queue_notice(WatchEvent::Fatal(format!("native watcher failed: {error}")));
            return;
        }
        let indexes = self
            .specs
            .iter()
            .enumerate()
            .filter_map(|(index, spec)| (!spec.polling).then_some(index))
            .collect::<Vec<_>>();
        for index in indexes {
            if let Err(promote_error) = self.promote_to_polling(
                index,
                format!("native watcher failed; using polling ({error})"),
            ) {
                self.queue_notice(WatchEvent::Fatal(format!(
                    "native watcher and polling both failed: {promote_error:#}"
                )));
            }
        }
        self.watcher = None;
    }

    fn run_due_checks(&mut self, stop: &AtomicBool) {
        let now = Instant::now();
        let due = self
            .specs
            .iter()
            .enumerate()
            .filter_map(|(index, spec)| {
                ((spec.polling || spec.sentinel) && now >= spec.next_check).then_some(index)
            })
            .collect::<Vec<_>>();
        for index in due {
            if let Err(error) = self.check_spec(index, stop) {
                let task = self.specs[index].name.clone();
                self.queue_notice(WatchEvent::Warning {
                    task: Some(task),
                    message: format!("watch scan failed; retrying ({error:#})"),
                });
                self.specs[index].next_check = Instant::now() + self.poll_interval;
            }
        }
    }

    fn check_spec(&mut self, index: usize, stop: &AtomicBool) -> Result<()> {
        let next = if self.specs[index].polling {
            self.poll_interval
        } else {
            SENTINEL_INTERVAL
        };
        let new_snapshot = snapshot_spec_until(&self.specs[index], Some(stop))?;
        let changed = diff_snapshots(&self.specs[index].snapshot, &new_snapshot);
        self.specs[index].snapshot = new_snapshot;
        self.specs[index].next_check = Instant::now() + next;
        if changed.is_empty() {
            if self.specs[index].sentinel {
                self.specs[index].native_seen.clear();
                self.specs[index].native_seen_overflow = false;
            }
            return Ok(());
        }

        if self.specs[index].polling {
            self.queue_change(index, changed, WatchEventSource::Polling, false);
            return Ok(());
        }

        if self.specs[index].native_seen_overflow {
            self.specs[index].native_seen.clear();
            self.specs[index].native_seen_overflow = false;
            return Ok(());
        }
        let missed = changed
            .into_iter()
            .filter(|path| {
                !self.specs[index]
                    .native_seen
                    .iter()
                    .any(|seen| paths_related(path, seen))
            })
            .collect::<Vec<_>>();
        self.specs[index].native_seen.clear();
        self.specs[index].native_seen_overflow = false;
        if missed.is_empty() {
            return Ok(());
        }
        self.promote_to_polling(
            index,
            "native events missed a filesystem change; using polling for this session".to_owned(),
        )?;
        self.queue_change(index, missed, WatchEventSource::Sentinel, false);
        Ok(())
    }

    fn queue_change(
        &mut self,
        index: usize,
        paths: Vec<PathBuf>,
        source: WatchEventSource,
        overflow: bool,
    ) {
        let Some(spec) = self.specs.get(index) else {
            return;
        };
        let pending = self
            .pending_changes
            .entry(spec.name.clone())
            .or_insert_with(|| PendingChange {
                paths: BTreeSet::new(),
                source,
                overflow: false,
            });
        if pending.paths.len() < MAX_EVENT_PATHS {
            pending.paths.extend(
                paths
                    .into_iter()
                    .take(MAX_EVENT_PATHS - pending.paths.len()),
            );
        }
        pending.overflow |= overflow;
        if source == WatchEventSource::Sentinel || source == WatchEventSource::Overflow {
            pending.source = source;
        }
    }

    fn queue_notice(&mut self, event: WatchEvent) {
        if self.pending_notices.len() < MAX_PENDING_NOTICES {
            self.pending_notices.push_back(event);
        }
    }

    fn try_flush(&mut self) {
        while let Some(event) = self.pending_notices.pop_front() {
            match self.output.try_send(event) {
                Ok(()) => {}
                Err(TrySendError::Full(event)) => {
                    self.pending_notices.push_front(event);
                    return;
                }
                Err(TrySendError::Disconnected(_)) => return,
            }
        }

        let tasks = self.pending_changes.keys().cloned().collect::<Vec<_>>();
        for task in tasks {
            let Some(pending) = self.pending_changes.remove(&task) else {
                continue;
            };
            let event = WatchEvent::Changed {
                task: task.clone(),
                paths: pending.paths.into_iter().collect(),
                source: pending.source,
                overflow: pending.overflow,
            };
            match self.output.try_send(event) {
                Ok(()) => {}
                Err(TrySendError::Full(WatchEvent::Changed {
                    paths,
                    source,
                    overflow,
                    ..
                })) => {
                    self.pending_changes.insert(
                        task,
                        PendingChange {
                            paths: paths.into_iter().collect(),
                            source,
                            overflow,
                        },
                    );
                    return;
                }
                Err(TrySendError::Full(_)) => unreachable!(),
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
    }
}

impl TaskWatchSpec {
    fn matches(&self, path: &Path) -> bool {
        self.roots.iter().any(|root| root.matches(path))
    }

    fn ignored(&self, display_path: &Path, source_path: &Path) -> bool {
        self.ignores
            .iter()
            .any(|ignore| ignore.matches(display_path, source_path))
    }
}

#[derive(Default)]
struct Registration {
    recursive: bool,
    specs: HashSet<usize>,
}

fn native_registrations(specs: &[TaskWatchSpec]) -> HashMap<PathBuf, Registration> {
    let mut registrations = HashMap::new();
    for (index, spec) in specs.iter().enumerate() {
        if spec.polling {
            continue;
        }
        for root in &spec.roots {
            if root.target_is_dir {
                add_registration(&mut registrations, root.target.clone(), true, index);
            } else if let Some(parent) = root.target.parent() {
                add_registration(&mut registrations, parent.to_path_buf(), false, index);
            }
            if let Some(parent) = root.configured.parent() {
                add_registration(&mut registrations, parent.to_path_buf(), false, index);
            }
            if let Some(parent) = root.target.parent() {
                add_registration(&mut registrations, parent.to_path_buf(), false, index);
            }
        }
    }
    registrations
}

fn add_registration(
    registrations: &mut HashMap<PathBuf, Registration>,
    path: PathBuf,
    recursive: bool,
    spec: usize,
) {
    let registration = registrations.entry(path).or_default();
    registration.recursive |= recursive;
    registration.specs.insert(spec);
}

type Snapshot = BTreeMap<PathBuf, Fingerprint>;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Fingerprint {
    kind: u8,
    len: u64,
    modified: Option<SystemTime>,
    identity: u64,
    target_hash: u64,
}

impl Fingerprint {
    fn from_metadata(metadata: &fs::Metadata, target: &Path) -> Self {
        let mut hasher = DefaultHasher::new();
        target.hash(&mut hasher);
        let kind = if metadata.is_dir() {
            1
        } else if metadata.is_file() {
            2
        } else {
            3
        };
        Self {
            kind,
            // Directory membership is already represented by snapshot keys.
            // Ignored child changes must not leak through a parent mtime.
            len: if kind == 1 { 0 } else { metadata.len() },
            modified: if kind == 1 {
                None
            } else {
                metadata.modified().ok()
            },
            identity: metadata_identity(metadata),
            target_hash: hasher.finish(),
        }
    }

    fn missing() -> Self {
        Self {
            kind: 0,
            len: 0,
            modified: None,
            identity: 0,
            target_hash: 0,
        }
    }
}

#[cfg(unix)]
fn metadata_identity(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;

    metadata.dev().rotate_left(32) ^ metadata.ino()
}

#[cfg(not(unix))]
fn metadata_identity(_metadata: &fs::Metadata) -> u64 {
    0
}

fn snapshot_spec(spec: &TaskWatchSpec) -> Result<Snapshot> {
    snapshot_spec_until(spec, None)
}

fn snapshot_spec_until(spec: &TaskWatchSpec, stop: Option<&AtomicBool>) -> Result<Snapshot> {
    let mut snapshot = Snapshot::new();
    for root in &spec.roots {
        snapshot_root(spec, root, &mut snapshot, stop)?;
    }
    Ok(snapshot)
}

fn snapshot_root(
    spec: &TaskWatchSpec,
    root: &WatchRoot,
    snapshot: &mut Snapshot,
    stop: Option<&AtomicBool>,
) -> Result<()> {
    ensure_scan_running(stop)?;
    let target = match fs::canonicalize(&root.configured) {
        Ok(target) => normalize_path(&target),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            snapshot.insert(root.configured.clone(), Fingerprint::missing());
            ensure_snapshot_size(snapshot)?;
            return Ok(());
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot scan {}", root.configured.display()));
        }
    };
    let metadata = fs::metadata(&target)
        .with_context(|| format!("cannot inspect {}", root.configured.display()))?;
    snapshot.insert(
        root.configured.clone(),
        Fingerprint::from_metadata(&metadata, &target),
    );
    ensure_snapshot_size(snapshot)?;
    if !metadata.is_dir() {
        return Ok(());
    }

    let walker = WalkDir::new(&target)
        .follow_links(true)
        .into_iter()
        .filter_entry(|entry| include_entry(spec, &target, &root.configured, entry));
    for entry in walker {
        ensure_scan_running(stop)?;
        let entry = entry.with_context(|| format!("cannot scan {}", root.configured.display()))?;
        if entry.path() == target {
            continue;
        }
        let relative = entry.path().strip_prefix(&target).unwrap_or(entry.path());
        let display = normalize_path(&root.configured.join(relative));
        let source = normalize_path(entry.path());
        if spec.ignored(&display, &source) {
            continue;
        }
        let metadata = entry
            .metadata()
            .with_context(|| format!("cannot inspect {}", display.display()))?;
        snapshot.insert(display, Fingerprint::from_metadata(&metadata, &source));
        ensure_snapshot_size(snapshot)?;
    }
    Ok(())
}

fn ensure_scan_running(stop: Option<&AtomicBool>) -> Result<()> {
    if stop.is_some_and(|stop| stop.load(Ordering::Acquire)) {
        bail!("watch scan cancelled");
    }
    Ok(())
}

fn ensure_snapshot_size(snapshot: &Snapshot) -> Result<()> {
    if snapshot.len() > MAX_SNAPSHOT_ENTRIES {
        bail!(
            "watch contains more than {MAX_SNAPSHOT_ENTRIES} entries; narrow the watched paths or ignore generated directories"
        );
    }
    Ok(())
}

fn include_entry(spec: &TaskWatchSpec, target: &Path, configured: &Path, entry: &DirEntry) -> bool {
    if entry.path() == target {
        return true;
    }
    let relative = entry.path().strip_prefix(target).unwrap_or(entry.path());
    let display = normalize_path(&configured.join(relative));
    let source = normalize_path(entry.path());
    !spec.ignored(&display, &source)
}

fn diff_snapshots(old: &Snapshot, new: &Snapshot) -> Vec<PathBuf> {
    old.keys()
        .chain(new.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|path| old.get(*path) != new.get(*path))
        .take(MAX_EVENT_PATHS)
        .cloned()
        .collect()
}

fn paths_related(left: &Path, right: &Path) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component.as_os_str());
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

#[cfg(target_os = "linux")]
fn suspicious_filesystem(path: &Path) -> bool {
    let Ok(source) = fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    suspicious_filesystem_from_mountinfo(path, &source)
}

#[cfg(not(target_os = "linux"))]
fn suspicious_filesystem(_path: &Path) -> bool {
    false
}

#[cfg(target_os = "linux")]
fn suspicious_filesystem_from_mountinfo(path: &Path, source: &str) -> bool {
    let path = fs::canonicalize(path).unwrap_or_else(|_| normalize_path(path));
    let mut best: Option<(usize, &str)> = None;
    for line in source.lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        let Some(separator) = fields.iter().position(|field| *field == "-") else {
            continue;
        };
        if fields.len() <= separator + 1 || fields.len() <= 4 {
            continue;
        }
        let mount = PathBuf::from(decode_mount_field(fields[4]));
        if !path.starts_with(&mount) {
            continue;
        }
        let depth = mount.components().count();
        if best.is_none_or(|(best_depth, _)| depth > best_depth) {
            best = Some((depth, fields[separator + 1]));
        }
    }
    best.is_some_and(|(_, kind)| is_suspicious_filesystem_type(kind))
}

fn is_suspicious_filesystem_type(kind: &str) -> bool {
    matches!(
        kind,
        "9p" | "nfs" | "nfs4" | "cifs" | "smb3" | "sshfs" | "virtiofs" | "ceph"
    ) || kind.starts_with("fuse.")
        || kind.starts_with("fuseblk")
        || kind.starts_with("glusterfs")
}

fn decode_mount_field(field: &str) -> String {
    field
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

#[cfg(test)]
mod tests {
    use std::{fs, thread, time::Duration};

    use tempfile::tempdir;

    use super::*;
    use crate::config::{DEFAULT_WATCH_POLL_INTERVAL, Settings, TaskCommand};

    fn watched_config(path: PathBuf, mode: WatchMode) -> Config {
        Config {
            settings: Settings {
                watch_mode: mode,
                watch_poll_interval: DEFAULT_WATCH_POLL_INTERVAL.to_owned(),
                ..Settings::default()
            },
            tasks: vec![Task {
                name: "server".to_owned(),
                command: TaskCommand::Shell("echo ready".to_owned()),
                cwd: PathBuf::from("."),
                env: BTreeMap::new(),
                depends_on: Vec::new(),
                start_delay: None,
                watch: vec![path],
                watch_ignore: Vec::new(),
                watch_delay: None,
                run_on_change: None,
                repeat: None,
            }],
            ..Config::default()
        }
    }

    #[test]
    fn no_service_is_started_without_watched_paths_or_when_disabled() {
        let temp = tempdir().unwrap();
        assert!(
            WatchService::start(temp.path(), &Config::default(), true)
                .unwrap()
                .is_none()
        );
        let config = watched_config(PathBuf::from("src"), WatchMode::Auto);
        assert!(
            WatchService::start(temp.path(), &config, false)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn snapshots_prune_ignored_subtrees_and_report_changes() {
        let temp = tempdir().unwrap();
        let src = temp.path().join("src");
        fs::create_dir_all(src.join("generated")).unwrap();
        fs::write(src.join("main.rs"), "one").unwrap();
        fs::write(src.join("generated/code.rs"), "one").unwrap();
        let mut config = watched_config(PathBuf::from("src"), WatchMode::Polling);
        config.tasks[0].watch_ignore = vec![PathBuf::from("src/generated")];
        let spec = build_specs(temp.path(), &config).unwrap().remove(0);
        let before = snapshot_spec(&spec).unwrap();

        fs::write(src.join("main.rs"), "two-two").unwrap();
        fs::write(src.join("generated/code.rs"), "two-two").unwrap();
        let after = snapshot_spec(&spec).unwrap();
        let changed = diff_snapshots(&before, &after);

        assert_eq!(changed, vec![src.join("main.rs")]);
        assert!(
            !after
                .keys()
                .any(|path| path.starts_with(src.join("generated")))
        );
    }

    #[test]
    fn polling_snapshot_does_not_leak_ignored_child_changes_through_directory_metadata() {
        let temp = tempdir().unwrap();
        let src = temp.path().join("src");
        fs::create_dir(&src).unwrap();
        let mut config = watched_config(PathBuf::from("src"), WatchMode::Polling);
        config.tasks[0].watch_ignore = vec![PathBuf::from("src/ignored.txt")];
        let spec = build_specs(temp.path(), &config).unwrap().remove(0);
        let before = snapshot_spec(&spec).unwrap();

        fs::write(src.join("ignored.txt"), "ignored").unwrap();
        let ignored = snapshot_spec(&spec).unwrap();
        assert!(diff_snapshots(&before, &ignored).is_empty());

        fs::write(src.join("main.rs"), "tracked").unwrap();
        let tracked = snapshot_spec(&spec).unwrap();
        assert_eq!(
            diff_snapshots(&ignored, &tracked),
            vec![src.join("main.rs")]
        );
    }

    #[test]
    fn polling_service_reports_file_changes() {
        let temp = tempdir().unwrap();
        let src = temp.path().join("src");
        fs::create_dir(&src).unwrap();
        let file = src.join("main.rs");
        fs::write(&file, "one").unwrap();
        let mut config = watched_config(PathBuf::from("src"), WatchMode::Polling);
        config.settings.watch_poll_interval = "250ms".to_owned();
        let service = WatchService::start(temp.path(), &config, true)
            .unwrap()
            .unwrap();

        fs::write(&file, "two-two").unwrap();
        let event = wait_for_change(&service, Duration::from_secs(3));

        assert_eq!(event.0, "server");
        assert!(event.1.contains(&file));
        assert_eq!(event.2, WatchEventSource::Polling);
    }

    #[test]
    fn native_service_reports_recursive_and_atomic_save_changes() {
        let temp = tempdir().unwrap();
        let src = temp.path().join("src");
        fs::create_dir_all(src.join("nested")).unwrap();
        let file = src.join("nested/main.rs");
        fs::write(&file, "one").unwrap();
        let config = watched_config(PathBuf::from("src"), WatchMode::Native);
        let service = WatchService::start(temp.path(), &config, true)
            .unwrap()
            .unwrap();
        thread::sleep(Duration::from_millis(100));

        let replacement = src.join("nested/.main.rs.tmp");
        fs::write(&replacement, "two").unwrap();
        fs::rename(&replacement, &file).unwrap();
        let event = wait_for_change(&service, Duration::from_secs(3));

        assert_eq!(event.0, "server");
        assert_eq!(event.2, WatchEventSource::Native);
        assert!(!event.1.is_empty());
    }

    #[test]
    fn native_exact_file_watch_ignores_sibling_changes() {
        let temp = tempdir().unwrap();
        let watched = temp.path().join("watched.txt");
        let sibling = temp.path().join("sibling.txt");
        fs::write(&watched, "one").unwrap();
        fs::write(&sibling, "one").unwrap();
        let config = watched_config(PathBuf::from("watched.txt"), WatchMode::Native);
        let service = WatchService::start(temp.path(), &config, true)
            .unwrap()
            .unwrap();
        thread::sleep(Duration::from_millis(100));

        fs::write(&sibling, "two-two").unwrap();
        assert_no_change(&service, Duration::from_millis(300));

        fs::write(&watched, "two-two").unwrap();
        let event = wait_for_change(&service, Duration::from_secs(3));
        assert_eq!(event.0, "server");
        assert!(event.1.contains(&watched));
    }

    #[test]
    fn native_directory_watch_filters_ignored_subtrees() {
        let temp = tempdir().unwrap();
        let src = temp.path().join("src");
        let generated = src.join("generated");
        fs::create_dir_all(&generated).unwrap();
        let main = src.join("main.rs");
        let ignored = generated.join("code.rs");
        fs::write(&main, "one").unwrap();
        fs::write(&ignored, "one").unwrap();
        let mut config = watched_config(PathBuf::from("src"), WatchMode::Native);
        config.tasks[0].watch_ignore = vec![PathBuf::from("src/generated")];
        let service = WatchService::start(temp.path(), &config, true)
            .unwrap()
            .unwrap();
        thread::sleep(Duration::from_millis(100));

        fs::write(&ignored, "two-two").unwrap();
        assert_no_change(&service, Duration::from_millis(300));

        fs::write(&main, "two-two").unwrap();
        let event = wait_for_change(&service, Duration::from_secs(3));
        assert!(event.1.contains(&main));
    }

    #[test]
    fn overlapping_native_watches_report_each_task() {
        let temp = tempdir().unwrap();
        let src = temp.path().join("src");
        fs::create_dir(&src).unwrap();
        let file = src.join("main.rs");
        fs::write(&file, "one").unwrap();
        let mut config = watched_config(PathBuf::from("src"), WatchMode::Native);
        let mut second = config.tasks[0].clone();
        second.name = "client".to_owned();
        config.tasks.push(second);
        let service = WatchService::start(temp.path(), &config, true)
            .unwrap()
            .unwrap();
        thread::sleep(Duration::from_millis(100));

        fs::write(&file, "two-two").unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut tasks = BTreeSet::new();
        while Instant::now() < deadline && tasks.len() < 2 {
            if let Some(WatchEvent::Changed { task, .. }) = service.try_recv().unwrap() {
                tasks.insert(task);
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            tasks,
            BTreeSet::from(["client".to_owned(), "server".to_owned()])
        );
    }

    #[test]
    fn auto_watch_continues_after_exact_file_is_deleted_and_recreated() {
        let temp = tempdir().unwrap();
        let file = temp.path().join("watched.txt");
        fs::write(&file, "one").unwrap();
        let mut config = watched_config(PathBuf::from("watched.txt"), WatchMode::Auto);
        config.settings.watch_poll_interval = "250ms".to_owned();
        let service = WatchService::start(temp.path(), &config, true)
            .unwrap()
            .unwrap();
        thread::sleep(Duration::from_millis(100));

        fs::remove_file(&file).unwrap();
        let removed = wait_for_change(&service, Duration::from_secs(3));
        assert_eq!(removed.0, "server");

        fs::write(&file, "recreated").unwrap();
        let recreated = wait_for_change(&service, Duration::from_secs(3));
        assert_eq!(recreated.0, "server");
    }

    #[test]
    fn dropping_watch_service_stops_coordinator_promptly() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("watched.txt"), "one").unwrap();
        let config = watched_config(PathBuf::from("watched.txt"), WatchMode::Native);
        let service = WatchService::start(temp.path(), &config, true)
            .unwrap()
            .unwrap();
        let started = Instant::now();
        drop(service);
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn polling_waits_the_full_interval_after_a_completed_scan() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("watched.txt"), "one").unwrap();
        let config = watched_config(PathBuf::from("watched.txt"), WatchMode::Polling);
        let specs = build_specs(temp.path(), &config).unwrap();
        let (tx, _rx) = mpsc::sync_channel(8);
        let interval = Duration::from_millis(250);
        let mut coordinator = Coordinator::new(specs, WatchMode::Polling, interval, tx).unwrap();
        let stop = AtomicBool::new(false);

        coordinator.check_spec(0, &stop).unwrap();

        assert!(coordinator.specs[0].next_check >= Instant::now() + Duration::from_millis(200));
    }

    #[test]
    fn polling_scan_honors_shutdown_before_traversal() {
        let temp = tempdir().unwrap();
        fs::create_dir(temp.path().join("src")).unwrap();
        let config = watched_config(PathBuf::from("src"), WatchMode::Polling);
        let spec = build_specs(temp.path(), &config).unwrap().remove(0);
        let stop = AtomicBool::new(true);

        let error = snapshot_spec_until(&spec, Some(&stop)).unwrap_err();

        assert!(format!("{error:#}").contains("watch scan cancelled"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mountinfo_classifier_uses_the_deepest_mount() {
        let mountinfo = r#"
1 0 8:1 / / rw - ext4 /dev/root rw
2 1 0:2 / /home/user/remote rw - nfs server:/repo rw
3 1 0:3 / /home/user/space\040dir rw - fuse.sshfs host:/repo rw
"#;
        assert!(suspicious_filesystem_from_mountinfo(
            Path::new("/home/user/remote/project"),
            mountinfo
        ));
        assert!(suspicious_filesystem_from_mountinfo(
            Path::new("/home/user/space dir/project"),
            mountinfo
        ));
        assert!(!suspicious_filesystem_from_mountinfo(
            Path::new("/var/project"),
            mountinfo
        ));
    }

    fn wait_for_change(
        service: &WatchService,
        timeout: Duration,
    ) -> (String, Vec<PathBuf>, WatchEventSource) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match service.try_recv() {
                Ok(Some(WatchEvent::Changed {
                    task,
                    paths,
                    source,
                    ..
                })) => return (task, paths, source),
                Ok(Some(WatchEvent::Warning { .. })) | Ok(None) => {}
                Ok(Some(WatchEvent::Fatal(error))) => panic!("watcher failed: {error}"),
                Err(TryRecvError::Disconnected) => panic!("watcher disconnected"),
                Err(TryRecvError::Empty) => unreachable!(),
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for file change");
    }

    fn assert_no_change(service: &WatchService, duration: Duration) {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            match service.try_recv() {
                Ok(Some(WatchEvent::Changed { task, paths, .. })) => {
                    panic!("unexpected change for {task}: {paths:?}")
                }
                Ok(Some(WatchEvent::Fatal(error))) => panic!("watcher failed: {error}"),
                Ok(Some(WatchEvent::Warning { .. })) | Ok(None) => {}
                Err(TryRecvError::Disconnected) => panic!("watcher disconnected"),
                Err(TryRecvError::Empty) => unreachable!(),
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}
