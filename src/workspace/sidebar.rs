//! Non-blocking domain model for a workspace file sidebar.
//!
//! Filesystem and Git operations run on one bounded background worker. Callers
//! drive completion by calling [`Sidebar::tick`].

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread;
use std::time::{Duration, Instant};

use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions as CapOpenOptions};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use super::{WorkspaceTrustState, WorkspaceTrustStore};

/// Default maximum number of children loaded from one directory.
pub const DEFAULT_ENTRY_CAP: usize = 10_000;
const GIT_OUTPUT_CAP_BYTES: usize = 4 * 1024 * 1024;
const GIT_TIMEOUT: Duration = Duration::from_secs(5);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);
const WATCH_CHANNEL_CAPACITY: usize = 64;

#[derive(Debug, Clone)]
pub struct SidebarConfig {
    pub entry_cap: usize,
    pub channel_capacity: usize,
    pub git_refresh_interval: Duration,
    pub fs_debounce_interval: Duration,
    pub fs_reconcile_interval: Duration,
}

impl Default for SidebarConfig {
    fn default() -> Self {
        Self {
            entry_cap: DEFAULT_ENTRY_CAP,
            channel_capacity: 8,
            git_refresh_interval: Duration::from_secs(3),
            fs_debounce_interval: Duration::from_millis(150),
            fs_reconcile_interval: Duration::from_secs(5),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Directory,
    File,
    SymlinkDirectory,
    Symlink,
    Other,
    /// A tracked path reported deleted by Git but absent from the filesystem.
    Deleted,
}

impl EntryKind {
    pub fn is_directory(self) -> bool {
        matches!(self, Self::Directory | Self::SymlinkDirectory)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GitStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Conflict,
    Untracked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GitDecoration {
    pub status: Option<GitStatus>,
    pub dirty_descendant: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitChange {
    /// Workspace-relative path, represented without UTF-8 conversion on Unix.
    pub path: PathBuf,
    pub original_path: Option<PathBuf>,
    pub status: GitStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarMutation {
    CreateFile { parent: PathBuf, name: String },
    CreateDirectory { parent: PathBuf, name: String },
    Rename { source: PathBuf, name: String },
    Trash { target: PathBuf },
}

#[derive(Debug)]
pub struct SidebarMutationCompletion {
    pub result: Result<(), String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarActivation {
    /// A file-like row that may be validated and opened by the backend.
    OpenFile(PathBuf),
    /// A real row was selected, but it must not cause an editor open.
    SelectOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarRow {
    pub path: PathBuf,
    pub name: OsString,
    pub depth: usize,
    pub kind: EntryKind,
    pub expanded: bool,
    pub loading: bool,
    pub selected: bool,
    pub error: Option<String>,
    pub git: GitDecoration,
}

impl SidebarRow {
    /// Display-safe name with replacement characters only for invalid Unicode.
    /// The lossless [`PathBuf`] identity remains available in [`Self::path`].
    pub fn display_name(&self) -> std::borrow::Cow<'_, str> {
        self.name.to_string_lossy()
    }
}

#[derive(Debug)]
struct Node {
    name: OsString,
    kind: EntryKind,
    expanded: bool,
    loading: bool,
    loaded: bool,
    error: Option<String>,
    children: Vec<PathBuf>,
    canonical_dir: Option<PathBuf>,
    synthetic_deleted: bool,
    git: GitDecoration,
}

impl Node {
    fn row(&self, path: PathBuf, depth: usize, selected: bool) -> SidebarRow {
        SidebarRow {
            path,
            name: self.name.clone(),
            depth,
            kind: self.kind,
            expanded: self.expanded,
            loading: self.loading,
            selected,
            error: self.error.clone(),
            git: self.git,
        }
    }
}

enum Job {
    Load {
        id: u64,
        path: PathBuf,
        root: PathBuf,
        ancestor_targets: Vec<PathBuf>,
        cap: usize,
    },
    Git {
        id: u64,
        root: PathBuf,
        cap: usize,
    },
    Mutation {
        id: u64,
        root: PathBuf,
        trust: Option<WorkspaceTrustStore>,
        mutation: SidebarMutation,
    },
}

enum Response {
    Load {
        id: u64,
        path: PathBuf,
        result: Result<LoadedDirectory, String>,
    },
    Git {
        id: u64,
        result: Result<Vec<GitChange>, String>,
    },
    Mutation {
        id: u64,
        result: Result<MutationOutcome, String>,
    },
}

enum WatchSignal {
    Paths(Vec<PathBuf>),
    Rescan,
}

#[derive(Debug)]
struct MutationOutcome {
    affected_parent: PathBuf,
    selected: PathBuf,
}

struct LoadedDirectory {
    canonical: PathBuf,
    entries: Vec<LoadedEntry>,
    truncated: bool,
}

struct LoadedEntry {
    path: PathBuf,
    name: OsString,
    kind: EntryKind,
    canonical_dir: Option<PathBuf>,
    error: Option<String>,
}

struct RankedLoadedEntry(LoadedEntry);

impl PartialEq for RankedLoadedEntry {
    fn eq(&self, other: &Self) -> bool {
        loaded_entry_cmp(&self.0, &other.0) == Ordering::Equal
    }
}

impl Eq for RankedLoadedEntry {}

impl PartialOrd for RankedLoadedEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RankedLoadedEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        loaded_entry_cmp(&self.0, &other.0)
    }
}

/// Stateful, non-blocking sidebar model.
///
/// `request_*`, `click_visible_row`, and `tick` never perform filesystem or
/// process I/O on the calling thread. A bounded channel applies backpressure;
/// request methods return `false` when the worker queue is full.
pub struct Sidebar {
    root: PathBuf,
    config: SidebarConfig,
    nodes: HashMap<PathBuf, Node>,
    selected: Option<PathBuf>,
    scroll: usize,
    jobs: SyncSender<Job>,
    responses: Receiver<Response>,
    next_id: u64,
    pending_loads: HashMap<PathBuf, u64>,
    pending_git: Option<u64>,
    git_refresh_due: bool,
    last_git_request: Instant,
    git_error: Option<String>,
    git_changes: Vec<GitChange>,
    _watcher: Option<RecommendedWatcher>,
    watch_events: Receiver<WatchSignal>,
    dirty_dirs: HashSet<PathBuf>,
    watch_dirty_since: Option<Instant>,
    reload_after_pending: HashSet<PathBuf>,
    next_fs_reconcile: Instant,
    trust_store: Option<WorkspaceTrustStore>,
    pending_mutation: Option<u64>,
    mutation_completion: Option<SidebarMutationCompletion>,
}

impl Sidebar {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Self> {
        Self::with_config_and_trust(root, SidebarConfig::default(), None)
    }

    pub fn with_trust(
        root: impl Into<PathBuf>,
        trust_store: Option<WorkspaceTrustStore>,
    ) -> io::Result<Self> {
        Self::with_config_and_trust(root, SidebarConfig::default(), trust_store)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn with_config(root: impl Into<PathBuf>, config: SidebarConfig) -> io::Result<Self> {
        Self::with_config_and_trust(root, config, None)
    }

    fn with_config_and_trust(
        root: impl Into<PathBuf>,
        config: SidebarConfig,
        trust_store: Option<WorkspaceTrustStore>,
    ) -> io::Result<Self> {
        if config.entry_cap == 0 || config.channel_capacity == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sidebar entry cap and channel capacity must be non-zero",
            ));
        }
        let root = root.into();
        let name = root
            .file_name()
            .map(OsStr::to_os_string)
            .unwrap_or_else(|| root.as_os_str().to_os_string());
        let mut nodes = HashMap::new();
        nodes.insert(
            root.clone(),
            Node {
                name,
                kind: EntryKind::Directory,
                expanded: false,
                loading: false,
                loaded: false,
                error: None,
                children: Vec::new(),
                canonical_dir: Some(root.clone()),
                synthetic_deleted: false,
                git: GitDecoration::default(),
            },
        );

        let (job_tx, job_rx) = mpsc::sync_channel(config.channel_capacity);
        let (response_tx, response_rx) = mpsc::sync_channel(config.channel_capacity);
        thread::Builder::new()
            .name("sidebar-worker".into())
            .spawn(move || worker(job_rx, response_tx))?;

        let (watch_tx, watch_rx) = mpsc::sync_channel(WATCH_CHANNEL_CAPACITY);
        let ignored_git = root.join(".git");
        let mut watcher =
            notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                let signal = match result {
                    Ok(event) => {
                        let paths = event
                            .paths
                            .into_iter()
                            .filter(|path| !path.starts_with(&ignored_git))
                            .collect::<Vec<_>>();
                        if paths.is_empty() {
                            return;
                        }
                        WatchSignal::Paths(paths)
                    }
                    Err(_) => WatchSignal::Rescan,
                };
                let _ = watch_tx.try_send(signal);
            })
            .ok();
        if let Some(active) = &mut watcher
            && active.watch(&root, RecursiveMode::Recursive).is_err()
        {
            watcher = None;
        }
        let now = Instant::now();
        let next_fs_reconcile = now + config.fs_reconcile_interval;

        Ok(Self {
            root,
            config,
            nodes,
            selected: None,
            scroll: 0,
            jobs: job_tx,
            responses: response_rx,
            next_id: 1,
            pending_loads: HashMap::new(),
            pending_git: None,
            git_refresh_due: false,
            last_git_request: now,
            git_error: None,
            git_changes: Vec::new(),
            _watcher: watcher,
            watch_events: watch_rx,
            dirty_dirs: HashSet::new(),
            watch_dirty_since: None,
            reload_after_pending: HashSet::new(),
            next_fs_reconcile,
            trust_store,
            pending_mutation: None,
            mutation_completion: None,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Expands and loads the root and requests Git state.
    pub fn request_initial(&mut self) {
        let root = self.root.clone();
        self.request_expand(&root);
        self.request_git_refresh();
    }

    /// Expands `path`, queueing a load if necessary. Returns whether a load was
    /// queued (an already-loaded directory is still expanded and returns false).
    pub fn request_expand(&mut self, path: &Path) -> bool {
        let Some(node) = self.nodes.get(path) else {
            return false;
        };
        if !node.kind.is_directory() {
            return false;
        }
        let needs_load = !node.loaded;
        if !needs_load {
            if let Some(node) = self.nodes.get_mut(path) {
                node.expanded = true;
            }
            return false;
        }

        let id = self.take_id();
        let ancestor_targets = self.ancestor_targets(path);
        let job = Job::Load {
            id,
            path: path.to_path_buf(),
            root: self.root.clone(),
            ancestor_targets,
            cap: self.config.entry_cap,
        };
        match self.jobs.try_send(job) {
            Ok(()) => {
                let node = self.nodes.get_mut(path).expect("node checked above");
                node.expanded = true;
                node.loading = true;
                node.error = None;
                self.pending_loads.insert(path.to_path_buf(), id);
                true
            }
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
        }
    }

    pub fn collapse(&mut self, path: &Path) {
        if let Some(node) = self.nodes.get_mut(path) {
            node.expanded = false;
            node.loading = false;
        }
        self.pending_loads.remove(path);
    }

    pub fn request_git_refresh(&mut self) -> bool {
        if self.pending_git.is_some() {
            return false;
        }
        let id = self.take_id();
        match self.jobs.try_send(Job::Git {
            id,
            root: self.root.clone(),
            cap: self.config.entry_cap,
        }) {
            Ok(()) => {
                self.pending_git = Some(id);
                self.git_refresh_due = false;
                self.last_git_request = Instant::now();
                true
            }
            Err(TrySendError::Full(_)) => {
                self.git_refresh_due = true;
                false
            }
            Err(TrySendError::Disconnected(_)) => {
                self.git_error = Some("sidebar worker is unavailable".to_owned());
                self.git_refresh_due = false;
                false
            }
        }
    }

    /// Applies worker replies, filesystem changes, and periodic refreshes.
    /// Returns `true` if model state changed.
    pub fn tick(&mut self) -> bool {
        let now = Instant::now();
        let mut changed = false;
        while let Ok(response) = self.responses.try_recv() {
            changed |= self.apply_response(response);
        }
        self.collect_watch_events(now);
        if self
            .watch_dirty_since
            .is_some_and(|since| now.duration_since(since) >= self.config.fs_debounce_interval)
        {
            self.watch_dirty_since = None;
            self.schedule_dirty_reloads();
        }
        if now >= self.next_fs_reconcile {
            self.next_fs_reconcile = now + self.config.fs_reconcile_interval;
            self.queue_all_loaded_dirs();
            self.schedule_dirty_reloads();
        }
        if self.pending_git.is_none()
            && (self.git_refresh_due
                || self.last_git_request.elapsed() >= self.config.git_refresh_interval)
        {
            self.request_git_refresh();
        }
        changed
    }

    fn collect_watch_events(&mut self, now: Instant) {
        let mut saw_event = false;
        while let Ok(signal) = self.watch_events.try_recv() {
            saw_event = true;
            match signal {
                WatchSignal::Paths(paths) => {
                    for path in paths {
                        self.mark_affected_directory(&path);
                    }
                }
                WatchSignal::Rescan => self.queue_all_loaded_dirs(),
            }
        }
        if saw_event {
            self.watch_dirty_since = Some(now);
        }
    }

    fn mark_affected_directory(&mut self, path: &Path) {
        let candidate = if path == self.root {
            self.root.as_path()
        } else {
            path.parent().unwrap_or(&self.root)
        };
        let mut cursor = Some(candidate);
        while let Some(path) = cursor {
            if self.nodes.get(path).is_some_and(|node| node.loaded) {
                self.dirty_dirs.insert(path.to_path_buf());
                return;
            }
            if path == self.root {
                break;
            }
            cursor = path.parent();
        }
        if self.nodes.get(&self.root).is_some_and(|node| node.loaded) {
            self.dirty_dirs.insert(self.root.clone());
        }
    }

    fn queue_all_loaded_dirs(&mut self) {
        self.dirty_dirs.extend(
            self.nodes
                .iter()
                .filter(|(_, node)| node.loaded && node.kind.is_directory())
                .map(|(path, _)| path.clone()),
        );
    }

    fn schedule_dirty_reloads(&mut self) {
        let dirty = std::mem::take(&mut self.dirty_dirs);
        for path in dirty {
            if self.pending_loads.contains_key(&path) {
                self.reload_after_pending.insert(path);
            } else if !self.request_reload(&path) {
                self.dirty_dirs.insert(path);
            }
        }
    }

    fn request_reload(&mut self, path: &Path) -> bool {
        let Some(node) = self.nodes.get(path) else {
            return true;
        };
        if !node.loaded || !node.kind.is_directory() {
            return true;
        }
        let id = self.take_id();
        let job = Job::Load {
            id,
            path: path.to_path_buf(),
            root: self.root.clone(),
            ancestor_targets: self.ancestor_targets(path),
            cap: self.config.entry_cap,
        };
        match self.jobs.try_send(job) {
            Ok(()) => {
                self.pending_loads.insert(path.to_path_buf(), id);
                true
            }
            Err(TrySendError::Full(_)) => false,
            Err(TrySendError::Disconnected(_)) => {
                self.git_error = Some("sidebar worker is unavailable".to_owned());
                true
            }
        }
    }

    /// Rows after expansion and scroll are applied, limited to `viewport_rows`.
    pub fn visible_rows(&self, viewport_rows: usize) -> Vec<SidebarRow> {
        self.flatten_rows()
            .into_iter()
            .skip(self.scroll)
            .take(viewport_rows)
            .collect()
    }

    /// All expanded rows, before scroll/viewport clipping.
    #[cfg(test)]
    fn all_visible_rows(&self) -> Vec<SidebarRow> {
        self.flatten_rows()
    }

    /// Selects a row relative to the current viewport without activating it.
    pub fn select_visible_row(&mut self, row: usize, viewport_rows: usize) -> Option<PathBuf> {
        if row >= viewport_rows {
            return None;
        }
        let path = self
            .flatten_rows()
            .get(self.scroll.saturating_add(row))?
            .path
            .clone();
        self.selected = Some(path.clone());
        Some(path)
    }

    /// Opens the selected file or toggles the selected directory.
    pub fn activate_selected(&mut self) -> Option<SidebarActivation> {
        let path = self.selected.clone()?;
        let node = self.nodes.get(&path)?;
        let directory = node.kind.is_directory();
        let open_file =
            matches!(node.kind, EntryKind::File | EntryKind::Symlink) && node.error.is_none();
        if directory {
            if self.nodes.get(&path).is_some_and(|node| node.expanded) {
                self.collapse(&path);
            } else {
                self.request_expand(&path);
            }
        }
        Some(if open_file {
            SidebarActivation::OpenFile(path)
        } else {
            SidebarActivation::SelectOnly
        })
    }

    pub fn scroll(&mut self, delta: isize, viewport_rows: usize) {
        let total = self.flatten_rows().len();
        let maximum = total.saturating_sub(viewport_rows);
        self.scroll = self.scroll.saturating_add_signed(delta).min(maximum);
    }

    pub fn ensure_path_visible_with_trailing(
        &mut self,
        path: &Path,
        viewport_rows: usize,
        trailing_rows: usize,
    ) {
        if viewport_rows == 0 {
            return;
        }
        let rows = self.flatten_rows();
        let Some(position) = rows.iter().position(|row| row.path == path) else {
            return;
        };
        if position < self.scroll {
            self.scroll = position;
        } else if position.saturating_add(trailing_rows) >= self.scroll + viewport_rows {
            self.scroll = position
                .saturating_add(trailing_rows)
                .saturating_add(1)
                .saturating_sub(viewport_rows)
                .min(rows.len().saturating_sub(1));
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn selected_path(&self) -> Option<&Path> {
        self.selected.as_deref()
    }

    pub fn entry_kind(&self, path: &Path) -> Option<EntryKind> {
        self.nodes.get(path).map(|node| node.kind)
    }

    pub fn request_mutation(&mut self, mutation: SidebarMutation) -> Result<(), String> {
        if self.pending_mutation.is_some() {
            return Err("a sidebar file operation is already pending".into());
        }
        let id = self.take_id();
        let job = Job::Mutation {
            id,
            root: self.root.clone(),
            trust: self.trust_store.clone(),
            mutation,
        };
        match self.jobs.try_send(job) {
            Ok(()) => {
                self.pending_mutation = Some(id);
                Ok(())
            }
            Err(TrySendError::Full(_)) => Err("sidebar worker is busy".into()),
            Err(TrySendError::Disconnected(_)) => Err("sidebar worker is unavailable".into()),
        }
    }

    pub fn take_mutation_completion(&mut self) -> Option<SidebarMutationCompletion> {
        self.mutation_completion.take()
    }

    /// Last Git worker error. Non-Git workspaces and command failures degrade to
    /// an undecorated tree rather than making filesystem browsing unavailable.
    pub fn git_error(&self) -> Option<&str> {
        self.git_error.as_deref()
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    fn ancestor_targets(&self, path: &Path) -> Vec<PathBuf> {
        let mut result = Vec::new();
        let mut cursor = path.parent();
        while let Some(parent) = cursor {
            if let Some(target) = self
                .nodes
                .get(parent)
                .and_then(|node| node.canonical_dir.clone())
            {
                result.push(target);
            }
            if parent == self.root {
                break;
            }
            cursor = parent.parent();
        }
        result
    }

    fn apply_response(&mut self, response: Response) -> bool {
        match response {
            Response::Load { id, path, result } => {
                if self.pending_loads.get(&path).copied() != Some(id) {
                    return false;
                }
                self.pending_loads.remove(&path);
                let top_path = self
                    .flatten_rows()
                    .get(self.scroll)
                    .map(|row| row.path.clone());
                let previous_scroll = self.scroll;
                let Some(parent) = self.nodes.get_mut(&path) else {
                    return false;
                };
                let was_loaded = parent.loaded;
                parent.loading = false;
                match result {
                    Err(error) => {
                        parent.error = Some(error);
                        parent.loaded = was_loaded;
                    }
                    Ok(loaded) => {
                        parent.loaded = true;
                        parent.canonical_dir = Some(loaded.canonical);
                        parent.error = loaded
                            .truncated
                            .then(|| format!("entry cap reached ({})", self.config.entry_cap));
                        let old_children = std::mem::take(&mut parent.children);
                        let new_paths: HashSet<_> = loaded
                            .entries
                            .iter()
                            .map(|entry| entry.path.clone())
                            .collect();
                        for old in old_children {
                            if !new_paths.contains(&old) {
                                self.remove_subtree(&old);
                            }
                        }
                        let mut children = Vec::with_capacity(loaded.entries.len());
                        for entry in loaded.entries {
                            children.push(entry.path.clone());
                            let old = self.nodes.remove(&entry.path);
                            let preserve_children = old.as_ref().is_some_and(|node| {
                                node.kind.is_directory()
                                    && entry.kind.is_directory()
                                    && node.canonical_dir == entry.canonical_dir
                            });
                            self.nodes.insert(
                                entry.path,
                                Node {
                                    name: entry.name,
                                    kind: entry.kind,
                                    expanded: preserve_children
                                        && old.as_ref().is_some_and(|node| node.expanded),
                                    loading: false,
                                    loaded: preserve_children
                                        && old.as_ref().is_some_and(|node| node.loaded),
                                    error: entry.error,
                                    children: if preserve_children {
                                        old.map_or_else(Vec::new, |node| node.children)
                                    } else {
                                        Vec::new()
                                    },
                                    canonical_dir: entry.canonical_dir,
                                    synthetic_deleted: false,
                                    git: GitDecoration::default(),
                                },
                            );
                        }
                        if let Some(parent) = self.nodes.get_mut(&path) {
                            parent.children = children;
                        }
                    }
                }
                self.reconcile_git();
                self.restore_selection();
                let rows = self.flatten_rows();
                self.scroll = top_path
                    .and_then(|top| rows.iter().position(|row| row.path == top))
                    .unwrap_or_else(|| previous_scroll.min(rows.len().saturating_sub(1)));
                if self.reload_after_pending.remove(&path) {
                    self.dirty_dirs.insert(path);
                    self.schedule_dirty_reloads();
                }
                true
            }
            Response::Git { id, result } => {
                if self.pending_git != Some(id) {
                    return false;
                }
                self.pending_git = None;
                match result {
                    Ok(changes) => {
                        self.git_error = None;
                        self.apply_git(changes);
                    }
                    Err(error) => {
                        self.git_error = Some(error);
                        self.git_changes.clear();
                        self.clear_git();
                    }
                }
                true
            }
            Response::Mutation { id, result } => {
                if self.pending_mutation != Some(id) {
                    return false;
                }
                self.pending_mutation = None;
                match result {
                    Ok(outcome) => {
                        self.selected = Some(outcome.selected);
                        self.dirty_dirs.insert(outcome.affected_parent);
                        self.schedule_dirty_reloads();
                        self.request_git_refresh();
                        self.mutation_completion =
                            Some(SidebarMutationCompletion { result: Ok(()) });
                    }
                    Err(error) => {
                        self.mutation_completion =
                            Some(SidebarMutationCompletion { result: Err(error) });
                    }
                }
                true
            }
        }
    }

    fn restore_selection(&mut self) {
        let Some(selected) = self.selected.clone() else {
            return;
        };
        if self.nodes.contains_key(&selected) {
            return;
        }
        let mut cursor = selected.parent();
        while let Some(path) = cursor {
            if self.nodes.contains_key(path) {
                self.selected = Some(path.to_path_buf());
                return;
            }
            cursor = path.parent();
        }
        self.selected = Some(self.root.clone());
    }

    fn remove_subtree(&mut self, path: &Path) {
        if let Some(node) = self.nodes.remove(path) {
            self.pending_loads.remove(path);
            for child in node.children {
                self.remove_subtree(&child);
            }
        }
    }

    fn clear_git(&mut self) {
        let synthetic: Vec<_> = self
            .nodes
            .iter()
            .filter(|(_, node)| node.synthetic_deleted)
            .map(|(path, _)| path.clone())
            .collect();
        for path in synthetic {
            if let Some(parent) = path.parent().and_then(|p| self.nodes.get_mut(p)) {
                parent.children.retain(|child| child != &path);
            }
            self.nodes.remove(&path);
        }
        for node in self.nodes.values_mut() {
            node.git = GitDecoration::default();
        }
    }

    fn apply_git(&mut self, changes: Vec<GitChange>) {
        self.git_changes = changes;
        self.reconcile_git();
    }

    fn reconcile_git(&mut self) {
        self.clear_git();
        for change in &self.git_changes {
            let Some(path) = safe_join(&self.root, &change.path) else {
                continue;
            };
            if change.status == GitStatus::Deleted
                && !self.nodes.contains_key(&path)
                && let Some(parent_path) = path.parent()
            {
                let visible_parent = self.nodes.get(parent_path).is_some_and(|node| node.loaded);
                if visible_parent {
                    let name = path.file_name().unwrap_or_default().to_os_string();
                    self.nodes.insert(
                        path.clone(),
                        Node {
                            name,
                            kind: EntryKind::Deleted,
                            expanded: false,
                            loading: false,
                            loaded: true,
                            error: None,
                            children: Vec::new(),
                            canonical_dir: None,
                            synthetic_deleted: true,
                            git: GitDecoration {
                                status: Some(GitStatus::Deleted),
                                dirty_descendant: false,
                            },
                        },
                    );
                    let mut children = self
                        .nodes
                        .get_mut(parent_path)
                        .map(|parent| {
                            parent.children.push(path.clone());
                            std::mem::take(&mut parent.children)
                        })
                        .unwrap_or_default();
                    sort_child_paths(&mut children, &self.nodes);
                    if let Some(parent) = self.nodes.get_mut(parent_path) {
                        parent.children = children;
                    }
                }
            }
            if let Some(node) = self.nodes.get_mut(&path) {
                node.git.status = Some(merge_status(node.git.status, change.status));
            }
            let mut ancestor = path.parent();
            while let Some(parent) = ancestor {
                if !parent.starts_with(&self.root) {
                    break;
                }
                if let Some(node) = self.nodes.get_mut(parent) {
                    node.git.dirty_descendant = true;
                }
                if parent == self.root {
                    break;
                }
                ancestor = parent.parent();
            }
        }
    }

    fn flatten_rows(&self) -> Vec<SidebarRow> {
        let mut rows = Vec::new();
        self.flatten_node(&self.root, 0, &mut rows);
        rows
    }

    fn flatten_node(&self, path: &Path, depth: usize, rows: &mut Vec<SidebarRow>) {
        let Some(node) = self.nodes.get(path) else {
            return;
        };
        rows.push(node.row(
            path.to_path_buf(),
            depth,
            self.selected.as_deref() == Some(path),
        ));
        if node.expanded {
            for child in &node.children {
                self.flatten_node(child, depth + 1, rows);
            }
        }
    }
}

fn worker(jobs: Receiver<Job>, responses: SyncSender<Response>) {
    while let Ok(job) = jobs.recv() {
        let response = match job {
            Job::Load {
                id,
                path,
                root,
                ancestor_targets,
                cap,
            } => Response::Load {
                id,
                path: path.clone(),
                result: load_directory(&root, &path, &ancestor_targets, cap),
            },
            Job::Git { id, root, cap } => Response::Git {
                id,
                result: load_git(&root, cap),
            },
            Job::Mutation {
                id,
                root,
                trust,
                mutation,
            } => Response::Mutation {
                id,
                result: perform_mutation(&root, trust.as_ref(), mutation),
            },
        };
        if responses.send(response).is_err() {
            break;
        }
    }
}

fn perform_mutation(
    root: &Path,
    trust: Option<&WorkspaceTrustStore>,
    mutation: SidebarMutation,
) -> Result<MutationOutcome, String> {
    let trust = trust.ok_or_else(|| "workspace trust is unavailable".to_owned())?;
    verify_mutation_trust(trust)?;
    let root = root
        .canonicalize()
        .map_err(|error| format!("failed to resolve workspace root: {error}"))?;
    let root_dir = Dir::open_ambient_dir(&root, ambient_authority())
        .map_err(|error| format!("failed to open workspace root: {error}"))?;
    verify_mutation_trust(trust)?;
    match mutation {
        SidebarMutation::CreateFile { parent, name } => {
            let parent = open_mutation_parent(&root, &root_dir, &parent)?;
            let name = validate_mutation_name(&name)?;
            let mut options = CapOpenOptions::new();
            options.write(true).create_new(true);
            parent.dir.open_with(name, &options).map_err(|error| {
                format!(
                    "failed to create {}: {error}",
                    parent.absolute.join(name).display()
                )
            })?;
            Ok(MutationOutcome {
                affected_parent: parent.absolute.clone(),
                selected: parent.absolute.join(name),
            })
        }
        SidebarMutation::CreateDirectory { parent, name } => {
            let parent = open_mutation_parent(&root, &root_dir, &parent)?;
            let name = validate_mutation_name(&name)?;
            parent.dir.create_dir(name).map_err(|error| {
                format!(
                    "failed to create {}: {error}",
                    parent.absolute.join(name).display()
                )
            })?;
            Ok(MutationOutcome {
                affected_parent: parent.absolute.clone(),
                selected: parent.absolute.join(name),
            })
        }
        SidebarMutation::Rename { source, name } => {
            let source = open_mutation_leaf(&root, &root_dir, &source)?;
            let destination = validate_mutation_name(&name)?;
            let target = source.parent.absolute.join(destination);
            secure_rename_noreplace(
                &source.parent.dir,
                &source.name,
                &source.parent.dir,
                destination,
            )
            .map_err(|error| {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    format!("{} already exists", target.display())
                } else {
                    format!(
                        "failed to rename {} to {}: {error}",
                        source.absolute.display(),
                        target.display()
                    )
                }
            })?;
            Ok(MutationOutcome {
                affected_parent: source.parent.absolute,
                selected: target,
            })
        }
        SidebarMutation::Trash { target } => {
            let target = open_mutation_leaf(&root, &root_dir, &target)?;
            if !root_path_matches_handle(&root, &root_dir) {
                return Err("workspace path changed before trash".into());
            }
            trash::delete(&target.absolute).map_err(|error| {
                format!(
                    "failed to move {} to trash; original was not permanently deleted: {error}",
                    target.absolute.display()
                )
            })?;
            Ok(MutationOutcome {
                affected_parent: target.parent.absolute.clone(),
                selected: target.parent.absolute,
            })
        }
    }
}

fn verify_mutation_trust(trust: &WorkspaceTrustStore) -> Result<(), String> {
    match trust.resolve() {
        Ok(WorkspaceTrustState::Trusted) => Ok(()),
        Ok(WorkspaceTrustState::Untrusted | WorkspaceTrustState::Stale) => {
            Err("workspace is not trusted".into())
        }
        Err(error) => Err(format!("failed to verify workspace trust: {error:#}")),
    }
}

struct MutationParent {
    dir: Dir,
    absolute: PathBuf,
}

struct MutationLeaf {
    parent: MutationParent,
    name: OsString,
    absolute: PathBuf,
}

fn open_mutation_parent(
    root: &Path,
    root_dir: &Dir,
    parent: &Path,
) -> Result<MutationParent, String> {
    let relative = parent
        .strip_prefix(root)
        .map_err(|_| "file operation escapes the workspace".to_owned())?;
    let mut dir = root_dir
        .try_clone()
        .map_err(|error| format!("failed to clone workspace handle: {error}"))?;
    let mut absolute = root.to_path_buf();
    for (index, component) in relative.components().enumerate() {
        let Component::Normal(name) = component else {
            return Err("invalid file operation path".into());
        };
        if index == 0 && name == OsStr::new(".git") {
            return Err("workspace .git cannot be modified".into());
        }
        let metadata = dir.symlink_metadata(name).map_err(|error| {
            format!(
                "failed to inspect {}: {error}",
                absolute.join(name).display()
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err("file operations inside symlink directories are not allowed".into());
        }
        let next = dir.open_dir(name).map_err(|error| {
            format!("failed to open {}: {error}", absolute.join(name).display())
        })?;
        if dir
            .symlink_metadata(name)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(true)
        {
            return Err("file operation path changed during validation".into());
        }
        absolute.push(name);
        dir = next;
    }
    Ok(MutationParent { dir, absolute })
}

fn open_mutation_leaf(root: &Path, root_dir: &Dir, target: &Path) -> Result<MutationLeaf, String> {
    if target == root {
        return Err("workspace root cannot be modified".into());
    }
    let relative = target
        .strip_prefix(root)
        .map_err(|_| "file operation escapes the workspace".to_owned())?;
    let mut components = relative.components().collect::<Vec<_>>();
    let Some(Component::Normal(name)) = components.pop() else {
        return Err("file operation target has no name".into());
    };
    if components
        .first()
        .is_some_and(|component| component.as_os_str() == OsStr::new(".git"))
    {
        return Err("workspace .git cannot be modified".into());
    }
    if components
        .iter()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err("invalid file operation path".into());
    }
    let parent_relative = components
        .iter()
        .fold(PathBuf::new(), |mut path, component| {
            path.push(component.as_os_str());
            path
        });
    let parent = open_mutation_parent(root, root_dir, &root.join(parent_relative))?;
    parent
        .dir
        .symlink_metadata(name)
        .map_err(|error| format!("failed to inspect {}: {error}", target.display()))?;
    Ok(MutationLeaf {
        parent,
        name: name.to_os_string(),
        absolute: target.to_path_buf(),
    })
}

fn validate_mutation_name(name: &str) -> Result<&OsStr, String> {
    let mut components = Path::new(name).components();
    let Some(Component::Normal(component)) = components.next() else {
        return Err("name must be one non-empty path component".into());
    };
    if components.next().is_some() || component == OsStr::new(".git") {
        return Err("name must be one non-.git path component".into());
    }
    Ok(component)
}

#[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
fn secure_rename_noreplace(
    source_dir: &Dir,
    source: &OsStr,
    destination_dir: &Dir,
    destination: &OsStr,
) -> io::Result<()> {
    Ok(rustix::fs::renameat_with(
        source_dir,
        source,
        destination_dir,
        destination,
        rustix::fs::RenameFlags::NOREPLACE,
    )?)
}

#[cfg(windows)]
fn secure_rename_noreplace(
    source_dir: &Dir,
    source: &OsStr,
    destination_dir: &Dir,
    destination: &OsStr,
) -> io::Result<()> {
    // Windows rename fails when the destination exists; unlike Unix it does
    // not need an explicit no-replace flag.
    source_dir.rename(source, destination_dir, destination)
}

#[cfg(not(any(
    target_vendor = "apple",
    target_os = "linux",
    target_os = "android",
    windows
)))]
fn secure_rename_noreplace(
    _source_dir: &Dir,
    _source: &OsStr,
    _destination_dir: &Dir,
    _destination: &OsStr,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic no-replace rename is unsupported on this platform",
    ))
}

#[cfg(unix)]
fn root_path_matches_handle(root: &Path, root_dir: &Dir) -> bool {
    use std::os::unix::fs::MetadataExt;

    let Ok(open) = rustix::fs::fstat(root_dir) else {
        return false;
    };
    let Ok(current) = fs::metadata(root) else {
        return false;
    };
    current.dev() == open.st_dev as u64 && current.ino() == open.st_ino
}

#[cfg(not(unix))]
fn root_path_matches_handle(root: &Path, _root_dir: &Dir) -> bool {
    root.canonicalize().is_ok_and(|canonical| canonical == root)
}

fn load_directory(
    root: &Path,
    path: &Path,
    ancestor_targets: &[PathBuf],
    cap: usize,
) -> Result<LoadedDirectory, String> {
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("failed to resolve {}: {error}", path.display()))?;
    if !canonical.starts_with(root) {
        return Err("symlink target escapes workspace".into());
    }
    if ancestor_targets
        .iter()
        .any(|ancestor| ancestor == &canonical)
    {
        return Err("symlink cycle through an ancestor".into());
    }

    let read = fs::read_dir(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    let mut entries = BinaryHeap::with_capacity(cap);
    let mut truncated = false;
    for item in read {
        let item = item.map_err(|error| format!("failed to read directory entry: {error}"))?;
        let name = item.file_name();
        if name == OsStr::new(".git") {
            continue;
        }
        let entry_path = item.path();
        let file_type = item
            .file_type()
            .map_err(|error| format!("failed to inspect {}: {error}", entry_path.display()))?;
        let (kind, canonical_dir, error) = if file_type.is_symlink() {
            match entry_path.canonicalize() {
                Err(error) => (EntryKind::Symlink, None, Some(error.to_string())),
                Ok(target) if !target.starts_with(root) => (
                    EntryKind::Symlink,
                    None,
                    Some("symlink target escapes workspace".into()),
                ),
                Ok(target) => match fs::metadata(&entry_path) {
                    Ok(metadata) if metadata.is_dir() => {
                        let cycle = target == canonical
                            || ancestor_targets.iter().any(|ancestor| ancestor == &target);
                        if cycle {
                            (
                                EntryKind::Symlink,
                                None,
                                Some("symlink cycle through an ancestor".into()),
                            )
                        } else {
                            (EntryKind::SymlinkDirectory, Some(target), None)
                        }
                    }
                    Ok(_) => (EntryKind::Symlink, None, None),
                    Err(error) => (EntryKind::Symlink, None, Some(error.to_string())),
                },
            }
        } else if file_type.is_dir() {
            (EntryKind::Directory, entry_path.canonicalize().ok(), None)
        } else if file_type.is_file() {
            (EntryKind::File, None, None)
        } else {
            (EntryKind::Other, None, None)
        };
        let candidate = RankedLoadedEntry(LoadedEntry {
            path: entry_path,
            name,
            kind,
            canonical_dir,
            error,
        });
        if entries.len() < cap {
            entries.push(candidate);
        } else {
            truncated = true;
            if entries
                .peek()
                .is_some_and(|largest| loaded_entry_cmp(&candidate.0, &largest.0).is_lt())
            {
                entries.pop();
                entries.push(candidate);
            }
        }
    }
    let mut entries = entries
        .into_iter()
        .map(|ranked| ranked.0)
        .collect::<Vec<_>>();
    entries.sort_by(loaded_entry_cmp);
    Ok(LoadedDirectory {
        canonical,
        entries,
        truncated,
    })
}

fn loaded_entry_cmp(left: &LoadedEntry, right: &LoadedEntry) -> Ordering {
    let left_group = usize::from(!left.kind.is_directory());
    let right_group = usize::from(!right.kind.is_directory());
    left_group
        .cmp(&right_group)
        .then_with(|| left.name.cmp(&right.name))
        .then_with(|| left.path.cmp(&right.path))
}

fn load_git(root: &Path, cap: usize) -> Result<Vec<GitChange>, String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain=v2", "-z", "--untracked-files=all"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to run git status: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to capture git status output".to_owned())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "failed to capture git status errors".to_owned())?;
    let (reader_tx, reader_rx) = mpsc::channel();
    let stdout_tx = reader_tx.clone();
    thread::spawn(move || {
        let _ = stdout_tx.send((true, read_capped(stdout, GIT_OUTPUT_CAP_BYTES)));
    });
    thread::spawn(move || {
        let _ = reader_tx.send((false, read_capped(stderr, GIT_OUTPUT_CAP_BYTES)));
    });

    let deadline = Instant::now() + GIT_TIMEOUT;
    let (status, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (status, false),
            Ok(None) if Instant::now() < deadline => thread::sleep(PROCESS_POLL_INTERVAL),
            Ok(None) => {
                terminate_git_processes(&mut child)
                    .map_err(|error| format!("failed to stop timed-out git status: {error}"))?;
                let status = child
                    .wait()
                    .map_err(|error| format!("failed to reap timed-out git status: {error}"))?;
                break (status, true);
            }
            Err(error) => {
                let _ = terminate_git_processes(&mut child);
                let _ = child.wait();
                return Err(format!("failed to wait for git status: {error}"));
            }
        }
    };

    let mut stdout_result = None;
    let mut stderr_result = None;
    let mut pipe_deadline = if timed_out {
        Instant::now() + PROCESS_POLL_INTERVAL * 10
    } else {
        deadline
    };
    let mut group_terminated = timed_out;
    while stdout_result.is_none() || stderr_result.is_none() {
        let remaining = pipe_deadline.saturating_duration_since(Instant::now());
        match reader_rx.recv_timeout(remaining) {
            Ok((is_stdout, result)) => {
                if is_stdout {
                    stdout_result = Some(result);
                } else {
                    stderr_result = Some(result);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) if !group_terminated => {
                terminate_git_processes(&mut child)
                    .map_err(|error| format!("failed to stop git status pipe holders: {error}"))?;
                group_terminated = true;
                pipe_deadline = Instant::now() + PROCESS_POLL_INTERVAL * 10;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(
                    "git status pipes remained open after process-group termination".into(),
                );
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("git status pipe readers disconnected".into());
            }
        }
    }
    let (stdout, stdout_truncated) = stdout_result
        .expect("stdout result checked")
        .map_err(|error| format!("failed to read git status stdout: {error}"))?;
    let (stderr, stderr_truncated) = stderr_result
        .expect("stderr result checked")
        .map_err(|error| format!("failed to read git status stderr: {error}"))?;
    if timed_out {
        return Err(format!(
            "git status timed out after {}s",
            GIT_TIMEOUT.as_secs()
        ));
    }
    if stdout_truncated || stderr_truncated {
        return Err(format!(
            "git status output exceeded {} bytes",
            GIT_OUTPUT_CAP_BYTES
        ));
    }
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        return Err(if stderr.trim().is_empty() {
            format!("git status exited with {status}")
        } else {
            stderr.trim().to_owned()
        });
    }
    parse_git_porcelain_v2_capped(&stdout, cap)
}

fn read_capped(mut reader: impl Read, cap: usize) -> io::Result<(Vec<u8>, bool)> {
    let mut kept = Vec::with_capacity(cap.min(64 * 1024));
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let remaining = cap.saturating_sub(kept.len());
        let keep = remaining.min(count);
        kept.extend_from_slice(&buffer[..keep]);
        truncated |= keep < count;
    }
    Ok((kept, truncated))
}

#[cfg(unix)]
fn terminate_git_processes(child: &mut Child) -> io::Result<()> {
    let group = format!("-{}", child.id());
    let status = Command::new("kill")
        .args(["-KILL", group.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        child.kill()
    }
}

#[cfg(not(unix))]
fn terminate_git_processes(child: &mut Child) -> io::Result<()> {
    child.kill()
}

/// Parses `git status --porcelain=v2 -z` output.
///
/// Rename/copy records consume the second NUL-delimited original-path field.
#[cfg(test)]
fn parse_git_porcelain_v2(bytes: &[u8]) -> Result<Vec<GitChange>, String> {
    parse_git_porcelain_v2_capped(bytes, DEFAULT_ENTRY_CAP)
}

fn parse_git_porcelain_v2_capped(bytes: &[u8], cap: usize) -> Result<Vec<GitChange>, String> {
    let mut records = bytes.split(|byte| *byte == 0);
    let mut changes = Vec::new();
    while let Some(record) = records.next() {
        if record.is_empty() || record.starts_with(b"# ") || record.starts_with(b"! ") {
            continue;
        }
        if changes.len() >= cap {
            return Err(format!("git change cap reached ({cap})"));
        }
        let change = match record[0] {
            b'1' => {
                let fields = split_n_fields(record, 9)?;
                GitChange {
                    status: status_from_xy(fields[1])?,
                    path: bytes_to_path(fields[8]),
                    original_path: None,
                }
            }
            b'2' => {
                let fields = split_n_fields(record, 10)?;
                let original = records
                    .next()
                    .ok_or_else(|| "rename record is missing original path".to_owned())?;
                GitChange {
                    status: status_from_xy(fields[1])?,
                    path: bytes_to_path(fields[9]),
                    original_path: Some(bytes_to_path(original)),
                }
            }
            b'u' => {
                let fields = split_n_fields(record, 11)?;
                GitChange {
                    status: GitStatus::Conflict,
                    path: bytes_to_path(fields[10]),
                    original_path: None,
                }
            }
            b'?' => {
                let path = record
                    .strip_prefix(b"? ")
                    .ok_or_else(|| "malformed untracked record".to_owned())?;
                GitChange {
                    status: GitStatus::Untracked,
                    path: bytes_to_path(path),
                    original_path: None,
                }
            }
            other => return Err(format!("unsupported porcelain v2 record type: {other}")),
        };
        changes.push(change);
    }
    Ok(changes)
}

fn split_n_fields(record: &[u8], count: usize) -> Result<Vec<&[u8]>, String> {
    let fields: Vec<_> = record.splitn(count, |byte| *byte == b' ').collect();
    if fields.len() != count || fields.iter().any(|field| field.is_empty()) {
        Err("malformed porcelain v2 record".into())
    } else {
        Ok(fields)
    }
}

fn status_from_xy(xy: &[u8]) -> Result<GitStatus, String> {
    if xy.len() != 2 {
        return Err("malformed porcelain v2 XY status".into());
    }
    if xy.contains(&b'U') || matches!(xy, b"AA" | b"DD") {
        Ok(GitStatus::Conflict)
    } else if xy.contains(&b'D') {
        Ok(GitStatus::Deleted)
    } else if xy.contains(&b'R') || xy.contains(&b'C') {
        Ok(GitStatus::Renamed)
    } else if xy.contains(&b'A') {
        Ok(GitStatus::Added)
    } else if xy.contains(&b'M') || xy.contains(&b'T') {
        Ok(GitStatus::Modified)
    } else {
        Err("porcelain record has no recognized status".into())
    }
}

fn merge_status(existing: Option<GitStatus>, incoming: GitStatus) -> GitStatus {
    fn priority(status: GitStatus) -> u8 {
        match status {
            GitStatus::Conflict => 6,
            GitStatus::Deleted => 5,
            GitStatus::Renamed => 4,
            GitStatus::Added => 3,
            GitStatus::Modified => 2,
            GitStatus::Untracked => 1,
        }
    }
    existing
        .filter(|status| priority(*status) >= priority(incoming))
        .unwrap_or(incoming)
}

fn safe_join(root: &Path, relative: &Path) -> Option<PathBuf> {
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return None;
    }
    Some(root.join(relative))
}

fn sort_child_paths(children: &mut [PathBuf], nodes: &HashMap<PathBuf, Node>) {
    children.sort_by(|left, right| {
        let left_node = nodes.get(left);
        let right_node = nodes.get(right);
        let left_group = usize::from(!left_node.is_some_and(|node| node.kind.is_directory()));
        let right_group = usize::from(!right_node.is_some_and(|node| node.kind.is_directory()));
        left_group.cmp(&right_group).then_with(|| {
            left_node
                .map(|node| &node.name)
                .cmp(&right_node.map(|node| &node.name))
        })
    });
}

#[cfg(unix)]
fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(OsString::from_vec(bytes.to_vec()))
}

#[cfg(not(unix))]
fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("sidebar-{label}-{}-{unique}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path.canonicalize().unwrap()
    }

    fn wait(sidebar: &mut Sidebar) {
        for _ in 0..200 {
            if sidebar.tick() {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("worker did not reply");
    }

    #[test]
    fn root_is_lazy_and_loading_sorts_shows_hidden_excludes_git_and_caps() {
        let root = temp_dir("tree");
        fs::create_dir(root.join("z-dir")).unwrap();
        fs::write(root.join("b"), b"").unwrap();
        fs::write(root.join("c"), b"").unwrap();
        fs::write(root.join(".hidden"), b"").unwrap();
        fs::create_dir(root.join(".git")).unwrap();
        let config = SidebarConfig {
            entry_cap: 3,
            ..SidebarConfig::default()
        };
        let mut sidebar = Sidebar::with_config(&root, config).unwrap();
        assert_eq!(sidebar.all_visible_rows().len(), 1);
        assert!(sidebar.request_expand(&root));
        wait(&mut sidebar);
        let rows = sidebar.all_visible_rows();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[1].name, OsStr::new("z-dir"));
        assert!(rows.iter().any(|row| row.name == OsStr::new(".hidden")));
        assert!(!rows.iter().any(|row| row.name == OsStr::new(".git")));
        assert!(rows[0].error.as_deref().unwrap().contains("entry cap"));
        assert_eq!(
            sidebar.select_visible_row(1, rows.len()),
            Some(root.join("z-dir"))
        );
        assert_eq!(
            sidebar.activate_selected(),
            Some(SidebarActivation::SelectOnly)
        );
        assert_eq!(sidebar.selected_path(), Some(root.join("z-dir").as_path()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn click_activation_only_emits_file_candidates() {
        let root = temp_dir("activation");
        fs::create_dir(root.join("dir")).unwrap();
        fs::write(root.join("file"), b"").unwrap();
        let mut sidebar = Sidebar::new(&root).unwrap();
        sidebar.request_expand(&root);
        wait(&mut sidebar);
        let rows = sidebar.all_visible_rows();
        let directory = rows
            .iter()
            .position(|row| row.path == root.join("dir"))
            .unwrap();
        let file = rows
            .iter()
            .position(|row| row.path == root.join("file"))
            .unwrap();

        assert_eq!(
            sidebar.select_visible_row(directory, rows.len()),
            Some(root.join("dir"))
        );
        assert_eq!(
            sidebar.activate_selected(),
            Some(SidebarActivation::SelectOnly)
        );
        assert_eq!(
            sidebar.select_visible_row(file, rows.len()),
            Some(root.join("file"))
        );
        assert_eq!(
            sidebar.activate_selected(),
            Some(SidebarActivation::OpenFile(root.join("file")))
        );
        assert_eq!(sidebar.select_visible_row(rows.len(), rows.len()), None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn porcelain_v2_parses_all_relevant_records_and_two_nul_rename() {
        let input = b"1 M. N... 100644 100644 100644 abc abc src/a file\0\
2 R. N... 100644 100644 100644 abc abc R100 new name\0old name\0\
u UU N... 100644 100644 100644 100644 a b c conflict\0\
? untracked\0\
1 .D N... 100644 100644 000000 abc abc deleted\0";
        let parsed = parse_git_porcelain_v2(input).unwrap();
        assert_eq!(parsed.len(), 5);
        assert_eq!(parsed[0].status, GitStatus::Modified);
        assert_eq!(parsed[0].path, Path::new("src/a file"));
        assert_eq!(parsed[1].status, GitStatus::Renamed);
        assert_eq!(parsed[1].path, Path::new("new name"));
        assert_eq!(
            parsed[1].original_path.as_deref(),
            Some(Path::new("old name"))
        );
        assert_eq!(parsed[2].status, GitStatus::Conflict);
        assert_eq!(parsed[3].status, GitStatus::Untracked);
        assert_eq!(parsed[4].status, GitStatus::Deleted);
    }

    #[test]
    fn parser_rejects_git_snapshots_over_the_change_cap() {
        let input = b"? first\0? second\0";
        let error = parse_git_porcelain_v2_capped(input, 1).unwrap_err();
        assert!(error.contains("change cap"));
    }

    #[test]
    fn stale_load_response_is_rejected_after_collapse() {
        let root = temp_dir("stale");
        fs::write(root.join("file"), b"").unwrap();
        let mut sidebar = Sidebar::new(&root).unwrap();
        sidebar.request_expand(&root);
        sidebar.collapse(&root);
        thread::sleep(Duration::from_millis(30));
        sidebar.tick();
        assert_eq!(sidebar.all_visible_rows().len(), 1);
        assert!(!sidebar.all_visible_rows()[0].expanded);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_and_ancestor_cycle_are_not_expandable() {
        use std::os::unix::fs::symlink;
        let root = temp_dir("links");
        let outside = temp_dir("outside");
        symlink(&outside, root.join("escape")).unwrap();
        symlink(&root, root.join("cycle")).unwrap();
        let mut sidebar = Sidebar::new(&root).unwrap();
        sidebar.request_expand(&root);
        wait(&mut sidebar);
        for row in sidebar.all_visible_rows().into_iter().skip(1) {
            assert_eq!(row.kind, EntryKind::Symlink);
            assert!(row.error.is_some());
        }
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn latest_git_snapshot_is_reconciled_after_lazy_directory_load() {
        let root = temp_dir("git-before-expand");
        fs::create_dir(root.join("dir")).unwrap();
        fs::write(root.join("dir/file"), b"").unwrap();
        let mut sidebar = Sidebar::new(&root).unwrap();
        sidebar.request_expand(&root);
        wait(&mut sidebar);
        sidebar.apply_git(vec![
            GitChange {
                path: PathBuf::from("dir/file"),
                original_path: None,
                status: GitStatus::Modified,
            },
            GitChange {
                path: PathBuf::from("dir/gone"),
                original_path: None,
                status: GitStatus::Deleted,
            },
        ]);

        sidebar.request_expand(&root.join("dir"));
        wait(&mut sidebar);
        let rows = sidebar.all_visible_rows();
        assert!(rows.iter().any(|row| {
            row.path == root.join("dir/file") && row.git.status == Some(GitStatus::Modified)
        }));
        assert!(
            rows.iter()
                .any(|row| { row.path == root.join("dir/gone") && row.kind == EntryKind::Deleted })
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dirty_ancestors_and_deleted_children_are_materialized() {
        let root = temp_dir("git-decoration");
        fs::create_dir(root.join("dir")).unwrap();
        let mut sidebar = Sidebar::new(&root).unwrap();
        sidebar.request_expand(&root);
        wait(&mut sidebar);
        sidebar.request_expand(&root.join("dir"));
        wait(&mut sidebar);
        sidebar.apply_git(vec![GitChange {
            path: PathBuf::from("dir/gone"),
            original_path: None,
            status: GitStatus::Deleted,
        }]);
        let rows = sidebar.all_visible_rows();
        assert!(rows[0].git.dirty_descendant);
        assert!(rows.iter().any(|row| row.kind == EntryKind::Deleted));
        fs::remove_dir_all(root).unwrap();
    }

    fn wait_until(sidebar: &mut Sidebar, mut predicate: impl FnMut(&Sidebar) -> bool) {
        for _ in 0..400 {
            sidebar.tick();
            if predicate(sidebar) {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("sidebar did not reach expected state");
    }

    fn fast_refresh_config() -> SidebarConfig {
        SidebarConfig {
            git_refresh_interval: Duration::from_secs(60),
            fs_debounce_interval: Duration::from_millis(20),
            fs_reconcile_interval: Duration::from_millis(100),
            ..SidebarConfig::default()
        }
    }

    #[test]
    fn watcher_refreshes_loaded_directories_and_preserves_hidden_entries() {
        let root = temp_dir("watch-refresh");
        fs::write(root.join("old"), b"").unwrap();
        fs::write(root.join(".hidden"), b"").unwrap();
        fs::create_dir(root.join(".git")).unwrap();
        let mut sidebar = Sidebar::with_config(&root, fast_refresh_config()).unwrap();
        sidebar.request_expand(&root);
        wait_until(&mut sidebar, |sidebar| {
            sidebar.nodes.get(&root).is_some_and(|node| node.loaded)
        });

        fs::rename(root.join("old"), root.join("new")).unwrap();
        wait_until(&mut sidebar, |sidebar| {
            let rows = sidebar.all_visible_rows();
            rows.iter().any(|row| row.path == root.join("new"))
                && !rows.iter().any(|row| row.path == root.join("old"))
        });
        let rows = sidebar.all_visible_rows();
        assert!(rows.iter().any(|row| row.path == root.join(".hidden")));
        assert!(!rows.iter().any(|row| row.path == root.join(".git")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reconciliation_restores_selection_and_viewport_anchor() {
        let root = temp_dir("watch-state");
        for name in ["a", "b", "c", "d"] {
            fs::write(root.join(name), b"").unwrap();
        }
        let mut sidebar = Sidebar::with_config(&root, fast_refresh_config()).unwrap();
        sidebar.request_expand(&root);
        wait_until(&mut sidebar, |sidebar| {
            sidebar.nodes.get(&root).is_some_and(|node| node.loaded)
        });
        sidebar.scroll(2, 2);
        let top = sidebar.visible_rows(2)[0].path.clone();
        let selected_row = sidebar
            .all_visible_rows()
            .iter()
            .position(|row| row.path == root.join("c"))
            .unwrap();
        sidebar.select_visible_row(selected_row.saturating_sub(sidebar.scroll), 2);

        fs::write(root.join("0"), b"").unwrap();
        wait_until(&mut sidebar, |sidebar| {
            sidebar.nodes.contains_key(&root.join("0"))
        });
        assert_eq!(sidebar.visible_rows(2)[0].path, top);
        assert_eq!(sidebar.selected_path(), Some(root.join("c").as_path()));

        fs::remove_file(root.join("c")).unwrap();
        wait_until(&mut sidebar, |sidebar| {
            !sidebar.nodes.contains_key(&root.join("c"))
        });
        assert_eq!(sidebar.selected_path(), Some(root.as_path()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn watcher_removes_a_deleted_loaded_directory_via_its_parent() {
        let root = temp_dir("watch-delete-directory");
        fs::create_dir(root.join("dir")).unwrap();
        fs::write(root.join("dir/file"), b"").unwrap();
        let mut sidebar = Sidebar::with_config(&root, fast_refresh_config()).unwrap();
        sidebar.request_expand(&root);
        wait_until(&mut sidebar, |sidebar| {
            sidebar.nodes.contains_key(&root.join("dir"))
        });
        sidebar.request_expand(&root.join("dir"));
        wait_until(&mut sidebar, |sidebar| {
            sidebar
                .nodes
                .get(&root.join("dir"))
                .is_some_and(|node| node.loaded)
        });

        fs::remove_dir_all(root.join("dir")).unwrap();
        wait_until(&mut sidebar, |sidebar| {
            !sidebar.nodes.contains_key(&root.join("dir"))
        });
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn transient_reload_failure_keeps_a_loaded_directory_retryable() {
        let root = temp_dir("watch-retry");
        let mut sidebar = Sidebar::with_config(&root, fast_refresh_config()).unwrap();
        sidebar.request_expand(&root);
        let pending = sidebar.pending_loads[&root];
        wait_until(&mut sidebar, |sidebar| {
            sidebar.nodes.get(&root).is_some_and(|node| node.loaded)
        });

        let retry = sidebar.take_id();
        sidebar.pending_loads.insert(root.clone(), retry);
        assert!(sidebar.apply_response(Response::Load {
            id: retry,
            path: root.clone(),
            result: Err("transient".into()),
        }));
        assert!(sidebar.nodes.get(&root).is_some_and(|node| node.loaded));
        assert_ne!(pending, retry);
        assert!(sidebar.request_reload(&root));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn viewport_reserves_space_for_a_new_entry_editor() {
        let root = temp_dir("edit-viewport");
        for name in ["a", "b", "c", "d"] {
            fs::write(root.join(name), b"").unwrap();
        }
        let mut sidebar = Sidebar::new(&root).unwrap();
        sidebar.request_expand(&root);
        wait_until(&mut sidebar, |sidebar| {
            sidebar.nodes.get(&root).is_some_and(|node| node.loaded)
        });

        sidebar.ensure_path_visible_with_trailing(&root.join("c"), 3, 1);
        let visible = sidebar.visible_rows(3);
        let position = visible
            .iter()
            .position(|row| row.path == root.join("c"))
            .unwrap();
        assert!(position + 1 < 3);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn periodic_reconciliation_recovers_without_a_watcher() {
        let root = temp_dir("watch-fallback");
        let mut sidebar = Sidebar::with_config(&root, fast_refresh_config()).unwrap();
        sidebar.request_expand(&root);
        wait_until(&mut sidebar, |sidebar| {
            sidebar.nodes.get(&root).is_some_and(|node| node.loaded)
        });
        sidebar._watcher = None;

        fs::write(root.join("eventually"), b"").unwrap();
        wait_until(&mut sidebar, |sidebar| {
            sidebar.nodes.contains_key(&root.join("eventually"))
        });
        fs::remove_dir_all(root).unwrap();
    }

    fn trusted_store(root: &Path, label: &str) -> (WorkspaceTrustStore, PathBuf) {
        let state = temp_dir(label);
        let store = WorkspaceTrustStore::new(root, &state).unwrap();
        store.trust().unwrap();
        (store, state)
    }

    fn wait_for_mutation(sidebar: &mut Sidebar) -> Result<(), String> {
        for _ in 0..400 {
            sidebar.tick();
            if let Some(completion) = sidebar.take_mutation_completion() {
                return completion.result;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("sidebar mutation did not complete");
    }

    #[test]
    fn trusted_worker_creates_and_renames_without_overwriting() {
        let root = temp_dir("mutation-worker");
        let (store, state) = trusted_store(&root, "mutation-state");
        let mut sidebar =
            Sidebar::with_config_and_trust(&root, fast_refresh_config(), Some(store)).unwrap();
        sidebar.request_expand(&root);
        wait_until(&mut sidebar, |sidebar| {
            sidebar.nodes.get(&root).is_some_and(|node| node.loaded)
        });

        sidebar
            .request_mutation(SidebarMutation::CreateFile {
                parent: root.clone(),
                name: "file.txt".into(),
            })
            .unwrap();
        wait_for_mutation(&mut sidebar).unwrap();
        assert!(root.join("file.txt").is_file());

        sidebar
            .request_mutation(SidebarMutation::Rename {
                source: root.join("file.txt"),
                name: "renamed.txt".into(),
            })
            .unwrap();
        wait_for_mutation(&mut sidebar).unwrap();
        assert!(root.join("renamed.txt").is_file());

        fs::write(root.join("taken"), b"").unwrap();
        sidebar
            .request_mutation(SidebarMutation::Rename {
                source: root.join("renamed.txt"),
                name: "taken".into(),
            })
            .unwrap();
        assert!(
            wait_for_mutation(&mut sidebar)
                .unwrap_err()
                .contains("already exists")
        );
        assert!(root.join("renamed.txt").exists());
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(state).unwrap();
    }

    #[test]
    fn worker_revalidates_trust_before_mutation() {
        let root = temp_dir("mutation-trust");
        let (store, state) = trusted_store(&root, "mutation-trust-state");
        store.revoke().unwrap();
        let error = perform_mutation(
            &root,
            Some(&store),
            SidebarMutation::CreateDirectory {
                parent: root.clone(),
                name: "blocked".into(),
            },
        )
        .unwrap_err();
        assert!(error.contains("not trusted"));
        assert!(!root.join("blocked").exists());
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(state).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn mutations_do_not_follow_symlink_leaves_or_directory_parents() {
        use std::os::unix::fs::symlink;

        let root = temp_dir("mutation-links");
        let outside = temp_dir("mutation-links-outside");
        let (store, state) = trusted_store(&root, "mutation-links-state");
        fs::write(outside.join("target"), b"outside").unwrap();
        symlink(outside.join("target"), root.join("link")).unwrap();
        perform_mutation(
            &root,
            Some(&store),
            SidebarMutation::Rename {
                source: root.join("link"),
                name: "renamed-link".into(),
            },
        )
        .unwrap();
        assert!(
            fs::symlink_metadata(root.join("renamed-link"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(outside.join("target")).unwrap(), b"outside");

        symlink(&outside, root.join("linked-dir")).unwrap();
        let error = perform_mutation(
            &root,
            Some(&store),
            SidebarMutation::CreateFile {
                parent: root.join("linked-dir"),
                name: "escape".into(),
            },
        )
        .unwrap_err();
        assert!(error.contains("symlink directories"));
        assert!(!outside.join("escape").exists());
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
        fs::remove_dir_all(state).unwrap();
    }

    #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
    #[test]
    fn atomic_rename_never_replaces_a_concurrent_destination() {
        use std::sync::{Arc, Barrier};

        let root = temp_dir("mutation-atomic-rename");
        fs::write(root.join("one"), b"one").unwrap();
        fs::write(root.join("two"), b"two").unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let mut threads = Vec::new();
        for source in ["one", "two"] {
            let root = root.clone();
            let barrier = barrier.clone();
            threads.push(thread::spawn(move || {
                let dir = Dir::open_ambient_dir(&root, ambient_authority()).unwrap();
                barrier.wait();
                secure_rename_noreplace(&dir, OsStr::new(source), &dir, OsStr::new("destination"))
            }));
        }
        barrier.wait();
        let results = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| result
                    .as_ref()
                    .is_err_and(|error| error.kind() == io::ErrorKind::AlreadyExists))
                .count(),
            1
        );
        assert!(root.join("destination").is_file());
        assert_eq!(
            [root.join("one"), root.join("two")]
                .iter()
                .filter(|path| path.exists())
                .count(),
            1
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn opened_parent_handle_cannot_be_redirected_by_a_symlink_swap() {
        use std::os::unix::fs::symlink;

        let root = temp_dir("mutation-parent-handle");
        let outside = temp_dir("mutation-parent-outside");
        fs::create_dir(root.join("dir")).unwrap();
        let root_dir = Dir::open_ambient_dir(&root, ambient_authority()).unwrap();
        let parent = open_mutation_parent(&root, &root_dir, &root.join("dir")).unwrap();

        fs::rename(root.join("dir"), root.join("held")).unwrap();
        symlink(&outside, root.join("dir")).unwrap();
        let mut options = CapOpenOptions::new();
        options.write(true).create_new(true);
        parent
            .dir
            .open_with("created", &options)
            .expect("open directory handle remains bound to the original directory");
        assert!(root.join("held/created").is_file());
        assert!(!outside.join("created").exists());
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn delete_moves_the_target_to_trash_without_a_permanent_fallback() {
        let root = temp_dir("mutation-trash");
        let (store, state) = trusted_store(&root, "mutation-trash-state");
        let target = root.join(format!("trash-{}", std::process::id()));
        fs::write(&target, b"recoverable").unwrap();
        perform_mutation(
            &root,
            Some(&store),
            SidebarMutation::Trash {
                target: target.clone(),
            },
        )
        .unwrap();
        assert!(!target.exists());
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(state).unwrap();
    }

    #[test]
    fn non_git_failure_degrades_to_error() {
        let root = temp_dir("nongit");
        let mut sidebar = Sidebar::new(&root).unwrap();
        assert!(sidebar.request_git_refresh());
        wait(&mut sidebar);
        assert!(sidebar.git_error().is_some());
        fs::remove_dir_all(root).unwrap();
    }
}
