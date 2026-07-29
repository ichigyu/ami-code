use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEvent,
    MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;

use super::LaunchMode;
use super::supervisor::{RestartDecision, RestartPolicy, SessionIdentity, SessionIds};
use crate::backend::{
    BackendKind, BackendSpec, ManagedNvimGeneration, ManagedNvimProfile, NativePiProfile,
    NvimController, ShellBackend,
};
use crate::terminal::{PasteError, ProcessSpec, TerminalSession, TerminalSize};
use crate::ui::{
    ContextMenu, ContextMenuAction, ShellTerminalPaneView, SidebarEditKind, SidebarEditView,
    SidebarStyle, SidebarTrustChrome, SidebarTrustTarget, SidebarView, TerminalPaneStyle,
    render_compact_workbench, render_context_menu, render_layout_controls,
    render_shell_terminal_pane, render_sidebar, render_terminal_pane,
    render_unavailable_terminal_pane, shell_terminal_content_size, sidebar_trust_hit,
    sidebar_trust_rows, terminal_content_size,
};
use crate::workbench::{
    LayoutDivider, LayoutHandle, LayoutStore, MIN_SHELL_PANE_HEIGHT, MIN_TERMINAL_HEIGHT,
    MIN_TERMINAL_WIDTH, MouseTarget, PaneId, ShellTabId, ShellTabTarget, WorkbenchLayout,
    WorkbenchLayoutConfig, WorkbenchState, hit_test, shell_tab_hit_test,
};
use crate::workspace::sidebar::{Sidebar, SidebarActivation, SidebarMutation};
use crate::workspace::{Workspace, WorkspaceTrustState, WorkspaceTrustStore};

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(16);
const SCROLLBACK_LINES: usize = 1_000;
const MOUSE_WHEEL_LINES: i32 = 3;
const EDGE_SCROLL_INTERVAL: Duration = Duration::from_millis(50);
const EDGE_SCROLL_LINES: i32 = 1;
const TRUST_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const SIDEBAR_DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(400);

pub fn run(mode: LaunchMode) -> Result<()> {
    let workspace = Workspace::discover(std::env::current_dir()?)?;
    let mut terminal = TerminalGuard::enter(mode.is_workbench())?;
    let area = terminal.terminal.size()?.into();
    let mut runtime = AppRuntime::new(mode, workspace, area)?;

    loop {
        runtime.poll_sessions_at(Instant::now())?;
        runtime.tick_mouse_selection();

        let area = terminal.terminal.size()?.into();
        runtime.resize(area)?;
        terminal.terminal.draw(|frame| runtime.render(frame))?;

        if event::poll(EVENT_POLL_INTERVAL)? && !runtime.handle_event(event::read()?)? {
            break;
        }
    }

    runtime.shutdown();
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct PendingMouseGesture {
    target: MouseTarget,
    down: MouseEvent,
    current: MouseEvent,
    dragging: bool,
    moved: bool,
    next_edge_scroll: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextMenuKeyAction {
    Copy,
    Paste,
    Dismiss,
}

#[derive(Debug, Clone)]
struct ContextMenuState {
    menu: ContextMenu,
    pane: PaneId,
    sidebar_target: Option<std::path::PathBuf>,
    direct_target: bool,
}

#[derive(Debug, Clone)]
struct SidebarEditState {
    kind: SidebarEditKind,
    target: std::path::PathBuf,
    value: String,
    error: Option<String>,
    pending: bool,
}

#[derive(Debug, Clone)]
struct SidebarDeleteState {
    target: std::path::PathBuf,
    pending: bool,
}

#[derive(Debug, Clone, Copy)]
enum PendingLayoutGesture {
    Handle {
        handle: LayoutHandle,
        origin: MouseEvent,
        moved: bool,
    },
    Drag {
        divider: LayoutDivider,
        origin: MouseEvent,
        layout: WorkbenchLayout,
        config: WorkbenchLayoutConfig,
    },
}

enum PasteDelivery {
    Sent,
    Unavailable,
    Rejected(String),
    BackendFailed,
}

#[derive(Debug, PartialEq, Eq)]
enum FileOpenDelivery {
    Opened,
    Unavailable,
    Failed(String),
}

enum SlotState {
    Starting,
    Running {
        identity: SessionIdentity,
        started_at: Instant,
        session: Box<TerminalSession>,
    },
    Backoff {
        until: Instant,
        message: String,
    },
    Paused {
        message: String,
    },
}

enum ProcessSource {
    Static(ProcessSpec),
    ManagedNvim {
        profile: ManagedNvimProfile,
        workspace: Workspace,
        current: Option<ManagedNvimGeneration>,
        controller: NvimController,
    },
    NativePi {
        workspace: Workspace,
        location: NativePiLocation,
        trust_store: Option<WorkspaceTrustStore>,
    },
}

#[derive(Clone)]
enum NativePiLocation {
    Environment,
    #[cfg(test)]
    Explicit {
        state_root: std::path::PathBuf,
    },
}

impl NativePiLocation {
    fn profile(&self) -> Result<NativePiProfile> {
        match self {
            Self::Environment => NativePiProfile::from_environment(),
            #[cfg(test)]
            Self::Explicit { state_root } => NativePiProfile::new(state_root),
        }
    }
}

impl ProcessSource {
    fn display_name(&self) -> &str {
        match self {
            Self::Static(spec) => &spec.display_name,
            Self::ManagedNvim { .. } => "nvim",
            Self::NativePi { .. } => "pi",
        }
    }

    fn next_spec(&mut self) -> Result<ProcessSpec> {
        match self {
            Self::Static(spec) => Ok(spec.clone()),
            Self::ManagedNvim {
                profile,
                workspace,
                current,
                controller,
            } => {
                // Dropping the replaced generation first removes its stale socket.
                current.take();
                controller.replace_endpoint(None);
                let generation = profile.generation(workspace)?;
                let spec = generation.process_spec().clone();
                controller.replace_endpoint(Some(generation.endpoint().to_path_buf()));
                *current = Some(generation);
                Ok(spec)
            }
            Self::NativePi {
                workspace,
                location,
                trust_store,
            } => {
                let trust = trust_store
                    .as_ref()
                    .and_then(|store| store.resolve().ok())
                    .unwrap_or(WorkspaceTrustState::Untrusted);
                location
                    .profile()?
                    .process_spec_with_trust(workspace, trust)
            }
        }
    }

    fn cleanup(&mut self) {
        if let Self::ManagedNvim {
            current,
            controller,
            ..
        } = self
        {
            current.take();
            controller.replace_endpoint(None);
        }
    }
}

struct SessionSlot {
    spec: ProcessSpec,
    source: ProcessSource,
    size: TerminalSize,
    generation: u64,
    policy: RestartPolicy,
    state: SlotState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SessionKey {
    Pane(PaneId),
    Shell(ShellTabId),
}

struct SessionRegistry {
    slots: HashMap<SessionKey, SessionSlot>,
    ids: SessionIds,
    shell_spec: Option<ProcessSpec>,
}

impl SessionRegistry {
    fn single(
        mode: LaunchMode,
        workspace: &Workspace,
        trust_store: Option<WorkspaceTrustStore>,
        area: Rect,
        now: Instant,
    ) -> Result<Self> {
        let source = match mode {
            LaunchMode::Shell => ProcessSource::Static(checked_process_spec(
                ShellBackend::system_default(),
                BackendKind::Shell,
                workspace,
            )),
            LaunchMode::Nvim => managed_nvim_source(workspace)?,
            LaunchMode::Pi => native_pi_source(workspace, trust_store),
            LaunchMode::Workbench => unreachable!("workbench uses multiple slots"),
        };
        Ok(Self::from_sources(
            vec![(
                SessionKey::Pane(PaneId::Editor),
                source,
                terminal_content_size(area),
            )],
            now,
        ))
    }

    fn workbench(
        workspace: &Workspace,
        trust_store: Option<WorkspaceTrustStore>,
        layout: WorkbenchLayout,
        now: Instant,
    ) -> Result<Self> {
        let shell_spec = checked_process_spec(
            ShellBackend::system_default(),
            BackendKind::Shell,
            workspace,
        );
        let mut registry = Self::from_sources(
            vec![
                (
                    SessionKey::Pane(PaneId::Editor),
                    managed_nvim_source(workspace)?,
                    terminal_content_size(layout.editor),
                ),
                (
                    SessionKey::Pane(PaneId::Agent),
                    native_pi_source(workspace, trust_store),
                    terminal_content_size(layout.agent),
                ),
                (
                    SessionKey::Shell(ShellTabId(1)),
                    ProcessSource::Static(shell_spec.clone()),
                    shell_terminal_content_size(layout.bottom),
                ),
            ],
            now,
        );
        registry.shell_spec = Some(shell_spec);
        Ok(registry)
    }

    #[cfg(test)]
    fn from_slots<const N: usize>(
        slots: [(SessionKey, ProcessSpec, TerminalSize); N],
        now: Instant,
    ) -> Self {
        Self::from_sources(
            slots
                .into_iter()
                .map(|(key, spec, size)| (key, ProcessSource::Static(spec), size))
                .collect(),
            now,
        )
    }

    fn from_sources(slots: Vec<(SessionKey, ProcessSource, TerminalSize)>, now: Instant) -> Self {
        let mut registry = Self {
            slots: slots
                .into_iter()
                .map(|(key, source, size)| {
                    let spec = match &source {
                        ProcessSource::Static(spec) => spec.clone(),
                        ProcessSource::ManagedNvim { .. } => ProcessSpec::new("nvim"),
                        ProcessSource::NativePi { .. } => ProcessSpec::new("pi"),
                    };
                    (
                        key,
                        SessionSlot {
                            spec,
                            source,
                            size,
                            generation: 0,
                            policy: RestartPolicy::default(),
                            state: SlotState::Starting,
                        },
                    )
                })
                .collect(),
            ids: SessionIds::new(),
            shell_spec: None,
        };
        let keys: Vec<_> = registry.slots.keys().copied().collect();
        for key in keys {
            registry.spawn(key, now);
        }
        registry
    }

    fn get(&self, key: &SessionKey) -> Option<&TerminalSession> {
        match &self.slots.get(key)?.state {
            SlotState::Running { session, .. } => Some(session),
            _ => None,
        }
    }

    fn get_mut(&mut self, key: &SessionKey) -> Option<&mut TerminalSession> {
        match &mut self.slots.get_mut(key)?.state {
            SlotState::Running { session, .. } => Some(session),
            _ => None,
        }
    }

    fn resize_slot(&mut self, key: SessionKey, size: TerminalSize, now: Instant) -> Result<()> {
        let failure = {
            let Some(slot) = self.slots.get_mut(&key) else {
                return Ok(());
            };
            slot.size = size;
            match &mut slot.state {
                SlotState::Running {
                    identity, session, ..
                } => session
                    .resize(size)
                    .err()
                    .map(|error| (*identity, format!("backend resize error: {error:#}"))),
                _ => None,
            }
        };
        if let Some((identity, message)) = failure {
            self.handle_failure(key, identity, now, message)?;
        }
        Ok(())
    }

    fn send_key(&mut self, slot_key: SessionKey, key: KeyEvent, now: Instant) -> Result<bool> {
        let event = {
            let Some(slot) = self.slots.get_mut(&slot_key) else {
                return Ok(false);
            };
            match &mut slot.state {
                SlotState::Running {
                    identity, session, ..
                } => Some((*identity, session.send_key(key))),
                _ => None,
            }
        };
        let Some((identity, result)) = event else {
            return Ok(false);
        };
        if let Err(error) = result {
            self.handle_failure(
                slot_key,
                identity,
                now,
                format!("backend input error: {error:#}"),
            )?;
        }
        Ok(true)
    }

    fn send_mouse(&mut self, key: SessionKey, event: MouseEvent, now: Instant) -> Result<bool> {
        let result = {
            let Some(slot) = self.slots.get_mut(&key) else {
                return Ok(false);
            };
            match &mut slot.state {
                SlotState::Running {
                    identity, session, ..
                } => Some((*identity, session.send_mouse(event))),
                _ => None,
            }
        };
        let Some((identity, result)) = result else {
            return Ok(false);
        };
        if let Err(error) = result {
            self.handle_failure(
                key,
                identity,
                now,
                format!("backend mouse error: {error:#}"),
            )?;
        }
        Ok(true)
    }

    fn send_paste(
        &mut self,
        key: SessionKey,
        contents: &str,
        now: Instant,
    ) -> Result<PasteDelivery> {
        let result = {
            let Some(slot) = self.slots.get_mut(&key) else {
                return Ok(PasteDelivery::Unavailable);
            };
            match &mut slot.state {
                SlotState::Running {
                    identity, session, ..
                } => Some((*identity, session.send_paste(contents))),
                _ => None,
            }
        };
        let Some((identity, result)) = result else {
            return Ok(PasteDelivery::Unavailable);
        };
        match result {
            Ok(()) => Ok(PasteDelivery::Sent),
            Err(PasteError::MultilineUnsupported) => Ok(PasteDelivery::Rejected(
                "multi-line paste requires backend bracketed-paste support".to_string(),
            )),
            Err(PasteError::Write(error)) => {
                self.handle_failure(
                    key,
                    identity,
                    now,
                    format!("backend paste error: {error:#}"),
                )?;
                Ok(PasteDelivery::BackendFailed)
            }
        }
    }

    fn open_editor_file(&self, path: &std::path::Path) -> FileOpenDelivery {
        self.open_editor_file_with(path, |controller, path| {
            controller
                .open_existing_file(path)
                .map_err(|error| error.to_string())
        })
    }

    /// The injected operation is a one-shot seam for proving that availability
    /// checks neither queue nor duplicate an open across managed generations.
    fn open_editor_file_with<F>(&self, path: &std::path::Path, open: F) -> FileOpenDelivery
    where
        F: FnOnce(&NvimController, &std::path::Path) -> Result<(), String>,
    {
        let Some(slot) = self.slots.get(&SessionKey::Pane(PaneId::Editor)) else {
            return FileOpenDelivery::Unavailable;
        };
        open_managed_nvim_source_file_with(
            &slot.source,
            matches!(slot.state, SlotState::Running { .. }),
            path,
            open,
        )
    }

    fn status(&self, key: SessionKey, now: Instant, click_retry: bool) -> Option<String> {
        let slot = self.slots.get(&key)?;
        match &slot.state {
            SlotState::Running { .. } => None,
            SlotState::Starting => Some("starting…".to_string()),
            SlotState::Backoff { until, message } => Some(format!(
                "{message}; retrying in {}ms ({})",
                until.saturating_duration_since(now).as_millis(),
                retry_hint(click_retry)
            )),
            SlotState::Paused { message } => {
                Some(format!("{message}; paused ({})", retry_hint(click_retry)))
            }
        }
    }

    fn poll(&mut self, now: Instant) -> Result<Vec<ShellTabId>> {
        if self.ids.is_shutting_down() {
            return Ok(Vec::new());
        }
        let mut exited_shells = Vec::new();
        let keys: Vec<_> = self.slots.keys().copied().collect();
        for key in keys.iter().copied() {
            let event = {
                let slot = self.slots.get_mut(&key).expect("known session slot");
                match &mut slot.state {
                    SlotState::Running {
                        identity, session, ..
                    } => {
                        let identity = *identity;
                        match session.poll_output().and_then(|()| session.has_exited()) {
                            Ok(true) => {
                                if let SessionKey::Shell(id) = key {
                                    exited_shells.push(id);
                                    None
                                } else {
                                    Some((identity, "backend exited".to_string()))
                                }
                            }
                            Ok(false) => None,
                            Err(error) => Some((identity, format!("backend error: {error:#}"))),
                        }
                    }
                    _ => None,
                }
            };
            if let Some((identity, message)) = event {
                self.handle_failure(key, identity, now, message)?;
            }
        }
        for key in keys {
            let due = self.slots.get(&key).is_some_and(
                |slot| matches!(slot.state, SlotState::Backoff { until, .. } if now >= until),
            );
            if due {
                self.spawn(key, now);
            }
        }
        Ok(exited_shells)
    }

    fn add_shell(&mut self, id: ShellTabId, size: TerminalSize, now: Instant) -> Result<()> {
        let spec = self
            .shell_spec
            .clone()
            .context("shell tab registry is missing its process spec")?;
        self.slots.insert(
            SessionKey::Shell(id),
            SessionSlot {
                source: ProcessSource::Static(spec.clone()),
                spec,
                size,
                generation: 0,
                policy: RestartPolicy::default(),
                state: SlotState::Starting,
            },
        );
        self.spawn(SessionKey::Shell(id), now);
        Ok(())
    }

    fn close_shell(&mut self, id: ShellTabId, replacement: Option<ShellTabId>, now: Instant) {
        let key = SessionKey::Shell(id);
        let Some(mut removed) = self.slots.remove(&key) else {
            return;
        };
        if let SlotState::Running { session, .. } = &mut removed.state {
            session.terminate();
        }
        if let Some(replacement) = replacement {
            self.slots.insert(
                SessionKey::Shell(replacement),
                SessionSlot {
                    source: ProcessSource::Static(removed.spec.clone()),
                    spec: removed.spec,
                    size: removed.size,
                    generation: 0,
                    policy: RestartPolicy::default(),
                    state: SlotState::Starting,
                },
            );
            self.spawn(SessionKey::Shell(replacement), now);
        }
    }

    fn handle_failure(
        &mut self,
        key: SessionKey,
        identity: SessionIdentity,
        now: Instant,
        message: String,
    ) -> Result<()> {
        if self.ids.is_shutting_down() {
            return Ok(());
        }
        let slot = self
            .slots
            .get_mut(&key)
            .context("lifecycle event for unknown session slot")?;
        let (current_identity, started_at) = match &slot.state {
            SlotState::Running {
                identity,
                started_at,
                ..
            } => (*identity, *started_at),
            _ => return Ok(()),
        };
        // Output-reader and wait notifications can race with a replacement. An old
        // process must never remove the new process occupying the same pane.
        if !current_identity.matches(identity) {
            return Ok(());
        }
        if let SlotState::Running { session, .. } = &mut slot.state {
            session.terminate();
        }
        slot.source.cleanup();
        let runtime = now.saturating_duration_since(started_at);
        slot.state = match slot.policy.failed(now, runtime) {
            RestartDecision::Backoff(delay) => SlotState::Backoff {
                until: now + delay,
                message,
            },
            RestartDecision::Paused => SlotState::Paused { message },
        };
        Ok(())
    }

    fn spawn(&mut self, key: SessionKey, now: Instant) {
        if self.ids.is_shutting_down() {
            return;
        }
        let Some(slot) = self.slots.get_mut(&key) else {
            return;
        };
        slot.generation = slot
            .generation
            .checked_add(1)
            .expect("session generation exhausted");
        let Some(identity) = self.ids.allocate(slot.generation) else {
            return;
        };
        match slot.source.next_spec() {
            Ok(spec) => slot.spec = spec,
            Err(error) => {
                let message = format!(
                    "failed to prepare {}: {error:#}",
                    slot.source.display_name()
                );
                slot.state = match slot.policy.failed(now, Duration::ZERO) {
                    RestartDecision::Backoff(delay) => SlotState::Backoff {
                        until: now + delay,
                        message,
                    },
                    RestartDecision::Paused => SlotState::Paused { message },
                };
                return;
            }
        }
        match TerminalSession::spawn(&slot.spec, slot.size, SCROLLBACK_LINES) {
            Ok(session) => {
                slot.state = SlotState::Running {
                    identity,
                    started_at: now,
                    session: Box::new(session),
                };
            }
            Err(error) => {
                slot.source.cleanup();
                let message = format!("failed to start {}: {error:#}", slot.spec.display_name);
                slot.state = match slot.policy.failed(now, Duration::ZERO) {
                    RestartDecision::Backoff(delay) => SlotState::Backoff {
                        until: now + delay,
                        message,
                    },
                    RestartDecision::Paused => SlotState::Paused { message },
                };
            }
        }
    }

    fn restart(&mut self, key: SessionKey, now: Instant) {
        if self.ids.is_shutting_down() {
            return;
        }
        let Some(slot) = self.slots.get_mut(&key) else {
            return;
        };
        if let SlotState::Running { session, .. } = &mut slot.state {
            session.terminate();
        }
        slot.source.cleanup();
        slot.policy.reset();
        slot.state = SlotState::Starting;
        self.spawn(key, now);
    }

    fn retry(&mut self, key: SessionKey, now: Instant) {
        if self.ids.is_shutting_down() || !self.slots.contains_key(&key) {
            return;
        }
        if let Some(slot) = self.slots.get_mut(&key) {
            if matches!(slot.state, SlotState::Running { .. }) {
                return;
            }
            slot.policy.reset();
            slot.state = SlotState::Starting;
        }
        self.spawn(key, now);
    }

    fn shutdown(&mut self) {
        self.ids.shutdown();
        for slot in self.slots.values_mut() {
            if let SlotState::Running { session, .. } = &mut slot.state {
                session.terminate();
            }
            slot.source.cleanup();
            slot.state = SlotState::Paused {
                message: "shut down".to_string(),
            };
        }
    }
}

fn retry_hint(click_retry: bool) -> &'static str {
    if click_retry {
        "click to retry"
    } else {
        "press any key to retry"
    }
}

struct AppRuntime {
    launch_mode: LaunchMode,
    workbench: WorkbenchState,
    layout_config: WorkbenchLayoutConfig,
    layout: Option<WorkbenchLayout>,
    sessions: SessionRegistry,
    sidebar: Option<Sidebar>,
    sidebar_error: Option<String>,
    status: Option<String>,
    viewport_area: Rect,
    pending_mouse: Option<PendingMouseGesture>,
    last_sidebar_click: Option<(std::path::PathBuf, Instant)>,
    pending_layout: Option<PendingLayoutGesture>,
    context_menu: Option<ContextMenuState>,
    sidebar_edit: Option<SidebarEditState>,
    sidebar_delete: Option<SidebarDeleteState>,
    layout_store: Option<LayoutStore>,
    trust_store: Option<WorkspaceTrustStore>,
    trust_state: WorkspaceTrustState,
    trust_confirming: bool,
    trust_status_message: Option<String>,
    next_trust_refresh: Instant,
}

impl AppRuntime {
    fn new(launch_mode: LaunchMode, workspace: Workspace, area: Rect) -> Result<Self> {
        let mut workbench = WorkbenchState::default();
        let mut layout_config = WorkbenchLayoutConfig::default();
        let mut status = None;
        let layout_store = if launch_mode.is_workbench() {
            match LayoutStore::from_environment(workspace.root()) {
                Ok(store) => {
                    match store.load() {
                        Ok(Some(intent)) => {
                            layout_config = intent.config;
                            workbench.set_manual_collapse(
                                intent.sidebar_collapsed,
                                intent.bottom_collapsed,
                            );
                        }
                        Ok(None) => {}
                        Err(error) => {
                            status = Some(format!("saved layout ignored: {error:#}"));
                        }
                    }
                    Some(store)
                }
                Err(error) => {
                    status = Some(format!("layout persistence unavailable: {error:#}"));
                    None
                }
            }
        } else {
            None
        };
        let trust_relevant = matches!(launch_mode, LaunchMode::Pi | LaunchMode::Workbench);
        let (trust_store, trust_state, trust_status_message) = if trust_relevant {
            match WorkspaceTrustStore::from_environment(workspace.root()) {
                Ok(store) => match store.resolve() {
                    Ok(state) => (Some(store), state, None),
                    Err(error) => {
                        let message = format!("workspace trust unavailable: {error:#}");
                        status = Some(message.clone());
                        (Some(store), WorkspaceTrustState::Untrusted, Some(message))
                    }
                },
                Err(error) => {
                    let message = format!("workspace trust unavailable: {error:#}");
                    status = Some(message.clone());
                    (None, WorkspaceTrustState::Untrusted, Some(message))
                }
            }
        } else {
            (None, WorkspaceTrustState::Untrusted, None)
        };

        // Loaded user intent is installed before this first automatic solve and
        // before SessionRegistry receives initial PTY dimensions.
        let layout = launch_mode.is_workbench().then(|| {
            workbench.update_auto_collapse(area, layout_config);
            WorkbenchLayout::calculate_visible(area, layout_config, workbench.visibility())
        });
        let sessions = if let Some(layout) = layout {
            SessionRegistry::workbench(&workspace, trust_store.clone(), layout, Instant::now())?
        } else {
            SessionRegistry::single(
                launch_mode,
                &workspace,
                trust_store.clone(),
                area,
                Instant::now(),
            )?
        };
        let (sidebar, sidebar_error) = if launch_mode.is_workbench() {
            match Sidebar::with_trust(workspace.root().to_path_buf(), trust_store.clone()) {
                Ok(mut sidebar) => {
                    debug_assert_eq!(sidebar.root(), workspace.root());
                    sidebar.request_initial();
                    (Some(sidebar), None)
                }
                Err(error) => (None, Some(format!("sidebar unavailable: {error}"))),
            }
        } else {
            (None, None)
        };

        Ok(Self {
            launch_mode,
            workbench,
            layout_config,
            layout,
            sessions,
            sidebar,
            sidebar_error,
            status,
            viewport_area: area,
            pending_mouse: None,
            last_sidebar_click: None,
            pending_layout: None,
            context_menu: None,
            sidebar_edit: None,
            sidebar_delete: None,
            layout_store,
            trust_store,
            trust_state,
            trust_confirming: false,
            trust_status_message,
            next_trust_refresh: Instant::now() + TRUST_REFRESH_INTERVAL,
        })
    }

    fn poll_sessions_at(&mut self, now: Instant) -> Result<()> {
        if now >= self.next_trust_refresh {
            self.next_trust_refresh = now + TRUST_REFRESH_INTERVAL;
            self.refresh_trust_state(now);
        }
        let mutation_completion = if let Some(sidebar) = &mut self.sidebar {
            sidebar.tick();
            sidebar.take_mutation_completion()
        } else {
            None
        };
        if let Some(completion) = mutation_completion {
            match completion.result {
                Ok(()) => {
                    self.sidebar_edit = None;
                    self.sidebar_delete = None;
                    self.status = None;
                }
                Err(error) => {
                    if let Some(edit) = &mut self.sidebar_edit {
                        edit.pending = false;
                        edit.error = Some(error.clone());
                    }
                    if self.sidebar_delete.is_some() {
                        self.sidebar_delete = None;
                    }
                    self.set_status(error);
                }
            }
        }
        let exited = self.sessions.poll(now)?;
        for id in exited {
            self.close_shell_tab(id, now);
        }
        Ok(())
    }

    fn close_shell_tab(&mut self, id: ShellTabId, now: Instant) {
        if self.workbench.shell_tabs().active() == id
            && self
                .workbench
                .selection()
                .is_some_and(|selection| selection.pane() == PaneId::Bottom)
        {
            // Reset the active session's frozen selection screen before the tab
            // reducer changes identity or the registry removes its process.
            self.clear_selection();
        }
        if let Some((removed, replacement)) = self.workbench.shell_tabs_mut().close(id) {
            self.sessions.close_shell(removed, replacement, now);
        }
    }

    fn shutdown(&mut self) {
        self.pending_mouse = None;
        self.cancel_layout_gesture();
        self.context_menu = None;
        self.sessions.shutdown();
    }

    fn resize(&mut self, area: Rect) -> Result<()> {
        self.resize_internal(area, true)
    }

    fn resize_preserving_selection(&mut self, area: Rect) -> Result<()> {
        self.resize_internal(area, false)
    }

    fn resize_internal(&mut self, area: Rect, clear_selection: bool) -> Result<()> {
        let now = Instant::now();
        if self.viewport_area != area {
            self.cancel_layout_gesture();
            self.context_menu = None;
            self.viewport_area = area;
        }
        if self.launch_mode.is_workbench() {
            self.workbench
                .update_auto_collapse(area, self.layout_config);
            let layout = WorkbenchLayout::calculate_visible(
                area,
                self.layout_config,
                self.workbench.visibility(),
            );
            if self.layout != Some(layout) {
                self.pending_mouse = None;
                self.context_menu = None;
                if clear_selection {
                    self.clear_selection();
                }
            }
            for (pane, pane_area) in [
                (PaneId::Editor, layout.editor),
                (PaneId::Agent, layout.agent),
            ] {
                if pane_area.width >= MIN_TERMINAL_WIDTH && pane_area.height >= MIN_TERMINAL_HEIGHT
                {
                    self.sessions.resize_slot(
                        SessionKey::Pane(pane),
                        terminal_content_size(pane_area),
                        now,
                    )?;
                }
            }
            if layout.bottom.width >= MIN_TERMINAL_WIDTH
                && layout.bottom.height >= MIN_SHELL_PANE_HEIGHT
            {
                let size = shell_terminal_content_size(layout.bottom);
                for id in self.workbench.shell_tabs().ids().collect::<Vec<_>>() {
                    self.sessions
                        .resize_slot(SessionKey::Shell(id), size, now)?;
                }
            }
            self.layout = Some(layout);
        } else {
            self.sessions.resize_slot(
                SessionKey::Pane(PaneId::Editor),
                terminal_content_size(area),
                now,
            )?;
        }
        Ok(())
    }

    fn cancel_layout_gesture(&mut self) {
        if let Some(config) = canceled_layout_config(&mut self.pending_layout) {
            self.layout_config = config;
        }
    }

    fn render(&self, frame: &mut ratatui::Frame<'_>) {
        if self.launch_mode.is_workbench() {
            self.render_workbench(frame);
        } else {
            self.render_slot(frame, PaneId::Editor, frame.area(), true, None);
        }
    }

    fn render_workbench(&self, frame: &mut ratatui::Frame<'_>) {
        let Some(layout) = self.layout else {
            return;
        };
        if layout.compact {
            render_compact_workbench(frame, frame.area());
            return;
        }
        if layout.sidebar.width > 0 && layout.sidebar.height > 0 {
            let trust = self.sidebar_trust_chrome();
            let viewport_rows = sidebar_tree_viewport_rows(layout.sidebar, trust);
            let rows = self
                .sidebar
                .as_ref()
                .map(|sidebar| sidebar.visible_rows(viewport_rows))
                .unwrap_or_default();
            let error = self
                .sidebar
                .as_ref()
                .and_then(Sidebar::git_error)
                .or(self.sidebar_error.as_deref());
            let edit = self.sidebar_edit.as_ref().map(|edit| SidebarEditView {
                target: &edit.target,
                value: &edit.value,
                kind: edit.kind,
                error: edit.error.as_deref(),
            });
            render_sidebar(
                frame,
                layout.sidebar,
                SidebarView {
                    rows: &rows,
                    edit,
                    trust,
                    error,
                    focused: self.workbench.is_focused(PaneId::Sidebar),
                    style: SidebarStyle::default(),
                },
            );
        }

        for (pane, area) in [
            (PaneId::Editor, layout.editor),
            (PaneId::Agent, layout.agent),
            (PaneId::Bottom, layout.bottom),
        ] {
            let minimum_height = if pane == PaneId::Bottom {
                MIN_SHELL_PANE_HEIGHT
            } else {
                MIN_TERMINAL_HEIGHT
            };
            if area.width >= MIN_TERMINAL_WIDTH && area.height >= minimum_height {
                self.render_slot(
                    frame,
                    pane,
                    area,
                    self.workbench.is_focused(pane),
                    self.workbench.selection_range(pane),
                );
            }
        }
        render_layout_controls(frame, layout);
        if let Some(context_menu) = &self.context_menu {
            render_context_menu(frame, &context_menu.menu);
        }
    }

    fn render_slot(
        &self,
        frame: &mut ratatui::Frame<'_>,
        pane: PaneId,
        area: Rect,
        focused: bool,
        selection: Option<crate::terminal::TerminalRange>,
    ) {
        let title_handle = self.launch_mode.is_workbench()
            && pane == PaneId::Bottom
            && self.layout.is_some_and(|layout| layout.bottom.height > 0);
        if self.launch_mode.is_workbench() && pane == PaneId::Bottom {
            let key = session_key(&self.workbench, pane);
            let session = self.sessions.get(&key);
            let status = self.sessions.status(key, Instant::now(), true);
            let title =
                session_title_with_handle("shell", status.as_deref(), area.width, title_handle);
            render_shell_terminal_pane(
                frame,
                area,
                ShellTerminalPaneView {
                    screen: session.map(TerminalSession::screen),
                    title: &title,
                    message: status.as_deref(),
                    focused,
                    selection,
                    tabs: self.workbench.shell_tabs(),
                    style: TerminalPaneStyle::default(),
                },
            );
            return;
        }
        if let Some(session) = self.sessions.get(&session_key(&self.workbench, pane)) {
            render_session(
                frame,
                area,
                session,
                focused,
                selection,
                self.status.as_deref(),
                title_handle,
            );
            return;
        }
        let lifecycle_status = self
            .sessions
            .status(
                session_key(&self.workbench, pane),
                Instant::now(),
                self.launch_mode.is_workbench(),
            )
            .unwrap_or_else(|| "unavailable".to_string());
        let status = unavailable_pane_status(pane, &lifecycle_status, self.status.as_deref());
        let display_name = self
            .sessions
            .slots
            .get(&session_key(&self.workbench, pane))
            .map(|slot| slot.spec.display_name.as_str())
            .unwrap_or("backend");
        let title =
            session_title_with_handle(display_name, Some(&status), area.width, title_handle);
        render_unavailable_terminal_pane(
            frame,
            area,
            &title,
            &status,
            focused,
            TerminalPaneStyle::default(),
        );
    }

    fn handle_event(&mut self, event: Event) -> Result<bool> {
        match event {
            Event::Key(key) => {
                self.pending_mouse = None;
                self.cancel_layout_gesture();
                if is_quit(key) {
                    self.context_menu = None;
                    self.shutdown();
                    return Ok(false);
                }
                if self.sidebar_delete.is_some() {
                    self.handle_sidebar_delete_key(key);
                    return Ok(true);
                }
                if self.sidebar_edit.is_some() {
                    self.handle_sidebar_edit_key(key);
                    return Ok(true);
                }
                if let Some(context) = self.context_menu.take() {
                    if context.sidebar_target.is_some() {
                        return Ok(true);
                    }
                    let copy_enabled = context_menu_copy_enabled(&self.workbench, context.pane);
                    match context_menu_key_action(key, copy_enabled) {
                        ContextMenuKeyAction::Copy => self.copy_selection(),
                        ContextMenuKeyAction::Paste => {
                            self.clear_selection();
                            self.paste_system_clipboard()?;
                        }
                        ContextMenuKeyAction::Dismiss => {}
                    }
                    return Ok(true);
                }
                if should_copy_selection(key, self.workbench.selection().is_some()) {
                    self.copy_selection();
                    return Ok(true);
                }
                if is_system_paste(key) {
                    self.clear_selection();
                    self.paste_system_clipboard()?;
                    return Ok(true);
                }

                self.clear_selection();
                let pane = if self.launch_mode.is_workbench() {
                    self.workbench.focused_pane()
                } else {
                    PaneId::Editor
                };
                if pane == PaneId::Sidebar && key.code == KeyCode::Enter {
                    self.last_sidebar_click = None;
                    self.activate_sidebar_selection();
                    return Ok(true);
                }
                let now = Instant::now();
                if !self
                    .sessions
                    .send_key(session_key(&self.workbench, pane), key, now)?
                {
                    self.sessions.retry(session_key(&self.workbench, pane), now);
                }
            }
            Event::Paste(contents) => {
                if let Some(edit) = &mut self.sidebar_edit {
                    if !edit.pending {
                        edit.value.push_str(&contents);
                        edit.error = None;
                    }
                    return Ok(true);
                }
                self.pending_mouse = None;
                self.cancel_layout_gesture();
                self.context_menu = None;
                self.clear_selection();
                self.paste_into_focused(&contents)?;
            }
            Event::Mouse(mouse) if self.launch_mode.is_workbench() => {
                self.handle_mouse_event(mouse)?;
            }
            _ => {}
        }

        Ok(true)
    }

    fn copy_selection(&mut self) {
        let Some(selection) = self.workbench.selection() else {
            return;
        };
        let Some(session) = self
            .sessions
            .get(&session_key(&self.workbench, selection.pane()))
        else {
            self.set_status("selected pane is unavailable");
            return;
        };
        let contents = session.selected_text(selection.range());
        match crate::clipboard::write_system(&contents) {
            Ok(()) => self.status = None,
            Err(error) => self.set_status(format!("clipboard error: {error}")),
        }
    }

    fn clear_selection(&mut self) {
        if let Some(selection) = self.workbench.selection()
            && let Some(session) = self
                .sessions
                .get_mut(&session_key(&self.workbench, selection.pane()))
        {
            session.reset_scrollback();
        }
        self.workbench.clear_selection();
    }

    fn handle_mouse_event(&mut self, event: MouseEvent) -> Result<()> {
        let Some(layout) = self.layout else {
            return Ok(());
        };
        if self.sidebar_edit.as_ref().is_some_and(|edit| edit.pending)
            || self
                .sidebar_delete
                .as_ref()
                .is_some_and(|delete| delete.pending)
        {
            return Ok(());
        }
        let clear_modal_status = self.sidebar_delete.is_some()
            || self
                .sidebar_edit
                .as_ref()
                .is_some_and(|edit| edit.error.is_some());
        if self.sidebar_edit.is_some() {
            self.sidebar_edit = None;
        }
        if self.sidebar_delete.is_some() {
            self.sidebar_delete = None;
        }
        if clear_modal_status {
            self.status = None;
        }
        if let Some(mut context) = self.context_menu.clone() {
            if event.kind == MouseEventKind::Moved {
                context.menu.update_hover(event.column, event.row);
                self.context_menu = Some(context);
                return Ok(());
            }
            if event.kind == MouseEventKind::Down(MouseButton::Left) {
                self.context_menu = None;
                match context.menu.action_at(event.column, event.row) {
                    Some(ContextMenuAction::Copy) => self.copy_selection(),
                    Some(ContextMenuAction::Paste) => {
                        self.workbench.focus_pane(context.pane);
                        self.clear_selection();
                        self.paste_system_clipboard()?;
                    }
                    Some(action) => self.begin_sidebar_action(&context, action),
                    None => {}
                }
                return Ok(());
            }
            if event.kind != MouseEventKind::Down(MouseButton::Right) {
                return Ok(());
            }
        }
        if event.kind == MouseEventKind::Down(MouseButton::Right) {
            self.pending_mouse = None;
            self.last_sidebar_click = None;
            self.cancel_layout_gesture();
            self.context_menu = None;
            match hit_test(layout, event.column, event.row) {
                Some(MouseTarget::Content { pane, .. }) => {
                    self.workbench.focus_pane(pane);
                    self.context_menu = Some(ContextMenuState {
                        menu: ContextMenu::new(
                            self.viewport_area,
                            event.column,
                            event.row,
                            context_menu_copy_enabled(&self.workbench, pane),
                        ),
                        pane,
                        sidebar_target: None,
                        direct_target: false,
                    });
                }
                Some(MouseTarget::Sidebar { row, .. }) => {
                    self.workbench.focus_pane(PaneId::Sidebar);
                    let trust = self.sidebar_trust_chrome();
                    let row = usize::from(row);
                    if sidebar_trust_hit(trust, row).is_some() {
                        return Ok(());
                    }
                    let viewport_rows = sidebar_tree_viewport_rows(layout.sidebar, trust);
                    let tree_row = row.saturating_sub(sidebar_trust_rows(trust));
                    let selected = self
                        .sidebar
                        .as_mut()
                        .and_then(|sidebar| sidebar.select_visible_row(tree_row, viewport_rows));
                    let direct_target = selected.is_some();
                    let target = selected.or_else(|| {
                        self.sidebar
                            .as_ref()
                            .and_then(Sidebar::selected_path)
                            .map(std::path::Path::to_path_buf)
                    });
                    let target = target.unwrap_or_else(|| {
                        self.sidebar
                            .as_ref()
                            .map(|sidebar| sidebar.root().to_path_buf())
                            .unwrap_or_default()
                    });
                    let kind = self
                        .sidebar
                        .as_ref()
                        .and_then(|sidebar| sidebar.entry_kind(&target));
                    let trusted = self.trust_state == WorkspaceTrustState::Trusted;
                    let create_enabled = trusted
                        && kind != Some(crate::workspace::sidebar::EntryKind::SymlinkDirectory);
                    let item_enabled = trusted
                        && direct_target
                        && self
                            .sidebar
                            .as_ref()
                            .is_some_and(|sidebar| target != sidebar.root())
                        && kind != Some(crate::workspace::sidebar::EntryKind::Deleted);
                    self.context_menu = Some(ContextMenuState {
                        menu: ContextMenu::sidebar(
                            self.viewport_area,
                            event.column,
                            event.row,
                            create_enabled,
                            item_enabled,
                        ),
                        pane: PaneId::Sidebar,
                        sidebar_target: Some(target),
                        direct_target,
                    });
                }
                _ => {}
            }
            return Ok(());
        }
        // The complete tab row is frontend chrome. Consume even overflow/blank
        // cells so no gesture can focus, select, paste into, or reach a PTY.
        if layout.bottom.height > 0
            && event.row == layout.bottom.y.saturating_add(1)
            && event.column > layout.bottom.x
            && event.column < layout.bottom.right().saturating_sub(1)
        {
            self.pending_mouse = None;
            if event.kind == MouseEventKind::Down(MouseButton::Left) {
                self.workbench.focus_pane(PaneId::Bottom);
                match shell_tab_hit_test(
                    layout.bottom,
                    self.workbench.shell_tabs(),
                    event.column,
                    event.row,
                ) {
                    Some(ShellTabTarget::Body(id)) => {
                        if self.workbench.shell_tabs().active() != id {
                            self.clear_selection();
                            self.workbench.shell_tabs_mut().select(id);
                        }
                    }
                    Some(ShellTabTarget::Close(id)) => self.close_shell_tab(id, Instant::now()),
                    Some(ShellTabTarget::Plus) => {
                        let id = self.workbench.shell_tabs_mut().new_tab();
                        let size = shell_terminal_content_size(layout.bottom);
                        self.sessions.add_shell(id, size, Instant::now())?;
                    }
                    None => {}
                }
            }
            return Ok(());
        }
        let target = hit_test(layout, event.column, event.row);

        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.cancel_layout_gesture();
                let Some(target) = target else {
                    return Ok(());
                };
                if !matches!(target, MouseTarget::Sidebar { .. }) {
                    self.last_sidebar_click = None;
                }
                match target {
                    MouseTarget::Handle(handle) => {
                        self.pending_mouse = None;
                        self.pending_layout = Some(PendingLayoutGesture::Handle {
                            handle,
                            origin: event,
                            moved: false,
                        });
                    }
                    MouseTarget::Divider(divider) => {
                        self.pending_mouse = None;
                        self.pending_layout = Some(PendingLayoutGesture::Drag {
                            divider,
                            origin: event,
                            layout,
                            config: self.layout_config,
                        });
                    }
                    _ => {
                        self.pending_layout = None;
                        self.clear_selection();
                        if let Some(pane) = target.pane() {
                            self.workbench.focus_pane(pane);
                        }
                        self.pending_mouse = Some(PendingMouseGesture {
                            target,
                            down: event,
                            current: event,
                            dragging: false,
                            moved: false,
                            next_edge_scroll: Instant::now() + EDGE_SCROLL_INTERVAL,
                        });
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(gesture) = &mut self.pending_layout {
                    if let PendingLayoutGesture::Handle { origin, moved, .. } = gesture {
                        *moved |= event.column != origin.column || event.row != origin.row;
                    } else {
                        self.update_layout_drag(event)?;
                    }
                    return Ok(());
                }
                let Some(mut pending) = self.pending_mouse else {
                    return Ok(());
                };
                pending.moved |=
                    event.column != pending.down.column || event.row != pending.down.row;
                if !pending.dragging {
                    pending.dragging = self.begin_mouse_selection(pending.target);
                }
                pending.current = event;
                if pending.dragging {
                    self.update_mouse_selection(event);
                }
                self.pending_mouse = Some(pending);
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(gesture) = self.pending_layout.take() {
                    match gesture {
                        PendingLayoutGesture::Handle { .. } => {
                            if let Some(handle) = activated_layout_handle(gesture, target) {
                                match handle {
                                    LayoutHandle::Sidebar => self.workbench.toggle_sidebar(),
                                    LayoutHandle::Bottom => self.workbench.toggle_bottom(),
                                }
                                self.resize_preserving_selection(self.viewport_area)?;
                                self.save_layout_intent();
                            }
                        }
                        PendingLayoutGesture::Drag { .. } => {
                            // Drag motion updates the preview only. Mouse-up is
                            // the sole divider commit point.
                            self.save_layout_intent();
                        }
                    }
                    return Ok(());
                }
                let Some(mut pending) = self.pending_mouse.take() else {
                    return Ok(());
                };
                pending.current = event;
                if pending.dragging {
                    self.update_mouse_selection(event);
                } else {
                    self.finish_mouse_click(pending, event)?;
                }
            }
            MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => self.route_mouse_wheel(target, event)?,
            MouseEventKind::Moved => self.forward_mouse_motion(target, event)?,
            _ => {}
        }

        Ok(())
    }

    fn update_layout_drag(&mut self, event: MouseEvent) -> Result<()> {
        let Some(PendingLayoutGesture::Drag {
            divider,
            origin,
            layout,
            config,
        }) = self.pending_layout
        else {
            return Ok(());
        };
        self.layout_config = layout_drag_config(divider, origin, event, layout, config);
        self.resize_preserving_selection(self.viewport_area)
    }

    fn begin_mouse_selection(&mut self, target: MouseTarget) -> bool {
        let MouseTarget::Content { pane, row, col } = target else {
            return false;
        };
        let Some(session) = self.sessions.get_mut(&session_key(&self.workbench, pane)) else {
            return false;
        };
        session.begin_selection_view();
        self.workbench
            .begin_selection(session.viewport_point(row, col));
        self.status = None;
        true
    }

    fn update_mouse_selection(&mut self, event: MouseEvent) {
        let Some(layout) = self.layout else {
            return;
        };
        let Some(selection) = self.workbench.selection() else {
            return;
        };
        let pane = selection.pane();
        let Some((row, col)) = pointer_content_position(layout, pane, event) else {
            return;
        };
        let Some(session) = self.sessions.get(&session_key(&self.workbench, pane)) else {
            return;
        };
        self.workbench
            .set_selection_head(session.viewport_point(row, col));
    }

    fn tick_mouse_selection(&mut self) {
        let Some(mut pending) = self.pending_mouse else {
            return;
        };
        let now = Instant::now();
        if !pending.dragging || now < pending.next_edge_scroll {
            return;
        }
        pending.next_edge_scroll = now + EDGE_SCROLL_INTERVAL;
        self.pending_mouse = Some(pending);

        let Some(layout) = self.layout else {
            return;
        };
        let Some(selection) = self.workbench.selection() else {
            return;
        };
        let pane = selection.pane();
        let direction = edge_scroll_direction(layout, pane, pending.current);
        if direction == 0 {
            return;
        }
        let Some(session) = self.sessions.get_mut(&session_key(&self.workbench, pane)) else {
            return;
        };
        if !session.scroll_viewport(direction * EDGE_SCROLL_LINES) {
            return;
        }
        let Some((row, col)) = pointer_content_position(layout, pane, pending.current) else {
            return;
        };
        let head = session.viewport_point(row, col);
        self.workbench.set_selection_head(head);
    }

    fn finish_mouse_click(
        &mut self,
        pending: PendingMouseGesture,
        release: MouseEvent,
    ) -> Result<()> {
        if matches!(pending.target, MouseTarget::Sidebar { .. }) {
            if let Some(row) = activated_sidebar_row(pending, release) {
                let trust = self.sidebar_trust_chrome();
                if let Some(target) = sidebar_trust_hit(trust, row) {
                    self.last_sidebar_click = None;
                    self.activate_trust_target(target);
                    return Ok(());
                }
                let viewport_rows = self
                    .layout
                    .map(|layout| sidebar_tree_viewport_rows(layout.sidebar, trust))
                    .unwrap_or(0);
                let tree_row = row.saturating_sub(sidebar_trust_rows(trust));
                let selected = self
                    .sidebar
                    .as_mut()
                    .and_then(|sidebar| sidebar.select_visible_row(tree_row, viewport_rows));
                if let Some(path) = selected {
                    let now = Instant::now();
                    let activate =
                        is_sidebar_double_click(self.last_sidebar_click.as_ref(), &path, now);
                    self.last_sidebar_click = Some((path, now));
                    if activate {
                        self.last_sidebar_click = None;
                        self.activate_sidebar_selection();
                    }
                } else {
                    self.last_sidebar_click = None;
                }
            }
            return Ok(());
        }
        let MouseTarget::Content { pane, row, col } = pending.target else {
            return Ok(());
        };
        if self
            .sessions
            .get(&session_key(&self.workbench, pane))
            .is_none()
        {
            self.sessions
                .retry(session_key(&self.workbench, pane), Instant::now());
            return Ok(());
        }
        let (release_row, release_col) = self
            .layout
            .and_then(|layout| pointer_content_position(layout, pane, release))
            .unwrap_or((row, col));
        let now = Instant::now();
        self.sessions.send_mouse(
            session_key(&self.workbench, pane),
            mouse_at(pending.down, row, col),
            now,
        )?;
        self.sessions.send_mouse(
            session_key(&self.workbench, pane),
            mouse_at(release, release_row, release_col),
            now,
        )?;
        Ok(())
    }

    fn begin_sidebar_action(&mut self, context: &ContextMenuState, action: ContextMenuAction) {
        let Some(target) = context.sidebar_target.clone() else {
            return;
        };
        match action {
            ContextMenuAction::NewFile | ContextMenuAction::NewFolder => {
                let parent = self
                    .sidebar
                    .as_ref()
                    .and_then(|sidebar| {
                        sidebar
                            .entry_kind(&target)
                            .filter(|kind| kind.is_directory())
                            .map(|_| target.clone())
                    })
                    .or_else(|| target.parent().map(std::path::Path::to_path_buf));
                let Some(parent) = parent else {
                    return;
                };
                let trust = self.sidebar_trust_chrome();
                let viewport_rows = self
                    .layout
                    .map(|layout| sidebar_tree_viewport_rows(layout.sidebar, trust))
                    .unwrap_or(0);
                if let Some(sidebar) = &mut self.sidebar {
                    sidebar.request_expand(&parent);
                    sidebar.ensure_path_visible_with_trailing(&parent, viewport_rows, 1);
                }
                self.status = None;
                self.sidebar_edit = Some(SidebarEditState {
                    kind: if action == ContextMenuAction::NewFolder {
                        SidebarEditKind::NewFolder
                    } else {
                        SidebarEditKind::NewFile
                    },
                    target: parent,
                    value: String::new(),
                    error: None,
                    pending: false,
                });
            }
            ContextMenuAction::Rename if context.direct_target => {
                let Some(name) = target
                    .file_name()
                    .and_then(std::ffi::OsStr::to_str)
                    .map(ToOwned::to_owned)
                else {
                    self.set_status("non-UTF-8 names cannot be renamed in the sidebar");
                    return;
                };
                self.status = None;
                self.sidebar_edit = Some(SidebarEditState {
                    kind: SidebarEditKind::Rename,
                    target,
                    value: name,
                    error: None,
                    pending: false,
                });
            }
            ContextMenuAction::Delete if context.direct_target => {
                self.set_status(format!(
                    "move {} to trash? Enter confirm, Esc cancel",
                    target.display()
                ));
                self.sidebar_delete = Some(SidebarDeleteState {
                    target,
                    pending: false,
                });
            }
            _ => {}
        }
    }

    fn handle_sidebar_edit_key(&mut self, key: KeyEvent) {
        let Some(edit) = &mut self.sidebar_edit else {
            return;
        };
        if edit.pending {
            return;
        }
        match key.code {
            KeyCode::Esc => {
                self.sidebar_edit = None;
                self.status = None;
            }
            KeyCode::Enter => self.submit_sidebar_edit(),
            KeyCode::Backspace => {
                edit.value.pop();
                edit.error = None;
                self.status = None;
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                edit.value.push(character);
                edit.error = None;
                self.status = None;
            }
            _ => {}
        }
    }

    fn submit_sidebar_edit(&mut self) {
        let Some(edit) = self.sidebar_edit.clone() else {
            return;
        };
        let mutation = match edit.kind {
            SidebarEditKind::NewFile => SidebarMutation::CreateFile {
                parent: edit.target,
                name: edit.value,
            },
            SidebarEditKind::NewFolder => SidebarMutation::CreateDirectory {
                parent: edit.target,
                name: edit.value,
            },
            SidebarEditKind::Rename => SidebarMutation::Rename {
                source: edit.target,
                name: edit.value,
            },
        };
        let result = self
            .sidebar
            .as_mut()
            .ok_or_else(|| "sidebar is unavailable".to_owned())
            .and_then(|sidebar| sidebar.request_mutation(mutation));
        match result {
            Ok(()) => {
                if let Some(edit) = &mut self.sidebar_edit {
                    edit.pending = true;
                    edit.error = None;
                }
                self.set_status("sidebar file operation pending");
            }
            Err(error) => {
                if let Some(edit) = &mut self.sidebar_edit {
                    edit.error = Some(error.clone());
                }
                self.set_status(error);
            }
        }
    }

    fn handle_sidebar_delete_key(&mut self, key: KeyEvent) {
        let Some(delete) = self.sidebar_delete.clone() else {
            return;
        };
        if delete.pending {
            return;
        }
        match key.code {
            KeyCode::Esc => {
                self.sidebar_delete = None;
                self.status = None;
            }
            KeyCode::Enter => {
                let result = self
                    .sidebar
                    .as_mut()
                    .ok_or_else(|| "sidebar is unavailable".to_owned())
                    .and_then(|sidebar| {
                        sidebar.request_mutation(SidebarMutation::Trash {
                            target: delete.target.clone(),
                        })
                    });
                match result {
                    Ok(()) => {
                        if let Some(delete) = &mut self.sidebar_delete {
                            delete.pending = true;
                        }
                        self.set_status(format!("moving {} to trash...", delete.target.display()));
                    }
                    Err(error) => self.set_status(error),
                }
            }
            _ => {}
        }
    }

    fn activate_sidebar_selection(&mut self) {
        let activation = self.sidebar.as_mut().and_then(Sidebar::activate_selected);
        if let Some(SidebarActivation::OpenFile(path)) = activation {
            apply_sidebar_open_result(
                &mut self.workbench,
                &mut self.status,
                self.sessions.open_editor_file(&path),
            );
        }
    }

    fn forward_mouse_motion(
        &mut self,
        target: Option<MouseTarget>,
        event: MouseEvent,
    ) -> Result<()> {
        let Some(MouseTarget::Content { pane, row, col }) = target else {
            return Ok(());
        };
        self.sessions.send_mouse(
            session_key(&self.workbench, pane),
            mouse_at(event, row, col),
            Instant::now(),
        )?;
        Ok(())
    }

    fn route_mouse_wheel(&mut self, target: Option<MouseTarget>, event: MouseEvent) -> Result<()> {
        if matches!(
            target,
            Some(MouseTarget::Sidebar { .. } | MouseTarget::Border(PaneId::Sidebar))
        ) {
            let delta = match event.kind {
                MouseEventKind::ScrollUp => -isize::try_from(MOUSE_WHEEL_LINES).unwrap_or(0),
                MouseEventKind::ScrollDown => isize::try_from(MOUSE_WHEEL_LINES).unwrap_or(0),
                MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => 0,
                _ => return Ok(()),
            };
            let trust = self.sidebar_trust_chrome();
            let viewport_rows = self
                .layout
                .map(|layout| sidebar_tree_viewport_rows(layout.sidebar, trust))
                .unwrap_or(0);
            if let Some(sidebar) = &mut self.sidebar {
                sidebar.scroll(delta, viewport_rows);
            }
            return Ok(());
        }
        let Some(MouseTarget::Content { pane, row, col }) = target else {
            return Ok(());
        };
        let selection_owns_wheel = self
            .workbench
            .selection()
            .is_some_and(|selection| selection.pane() == pane);
        let Some(session) = self.sessions.get_mut(&session_key(&self.workbench, pane)) else {
            return Ok(());
        };
        let lines = match event.kind {
            MouseEventKind::ScrollUp => MOUSE_WHEEL_LINES,
            MouseEventKind::ScrollDown => -MOUSE_WHEEL_LINES,
            MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => 0,
            _ => return Ok(()),
        };

        if selection_owns_wheel && lines != 0 {
            session.scroll_viewport(lines);
        } else if session.mouse_reporting() {
            self.sessions.send_mouse(
                session_key(&self.workbench, pane),
                mouse_at(event, row, col),
                Instant::now(),
            )?;
        } else if lines != 0 {
            session.scroll_viewport(lines);
        }
        Ok(())
    }

    fn paste_system_clipboard(&mut self) -> Result<()> {
        match crate::clipboard::read_system() {
            Ok(contents) => self.paste_into_focused(&contents)?,
            Err(error) => self.set_status(format!("clipboard error: {error}")),
        }
        Ok(())
    }

    fn paste_into_focused(&mut self, contents: &str) -> Result<()> {
        let pane = if self.launch_mode.is_workbench() {
            self.workbench.focused_pane()
        } else {
            PaneId::Editor
        };
        match self.sessions.send_paste(
            session_key(&self.workbench, pane),
            contents,
            Instant::now(),
        )? {
            PasteDelivery::Sent => self.status = None,
            PasteDelivery::Unavailable => {
                self.set_status("focused pane does not accept paste");
            }
            PasteDelivery::Rejected(error) => {
                self.set_status(format!("paste rejected: {error}"));
            }
            PasteDelivery::BackendFailed => {
                self.set_status("paste failed; backend will restart");
            }
        }
        Ok(())
    }

    fn sidebar_trust_chrome(&self) -> SidebarTrustChrome {
        SidebarTrustChrome {
            state: self.trust_state,
            confirming: self.trust_confirming,
        }
    }

    fn refresh_trust_state(&mut self, now: Instant) {
        let Some(store) = self.trust_store.clone() else {
            self.trust_state = WorkspaceTrustState::Untrusted;
            return;
        };
        let previous = self.trust_state;
        match store.resolve() {
            Ok(state) => {
                self.trust_state = state;
                self.trust_confirming =
                    trust_confirmation_after_refresh(self.trust_confirming, previous, state, false);
                self.clear_trust_status();
            }
            Err(error) => {
                self.trust_state = WorkspaceTrustState::Untrusted;
                self.trust_confirming = trust_confirmation_after_refresh(
                    self.trust_confirming,
                    previous,
                    WorkspaceTrustState::Untrusted,
                    true,
                );
                self.set_trust_status(format!("workspace trust unavailable: {error:#}"));
            }
        }
        if trust_capability_changed(previous, self.trust_state)
            && let Some(key) = trust_restart_key(self.launch_mode)
        {
            self.sessions.restart(key, now);
        }
    }

    fn activate_trust_target(&mut self, target: SidebarTrustTarget) {
        match target {
            SidebarTrustTarget::Review => self.trust_confirming = true,
            SidebarTrustTarget::Cancel => self.trust_confirming = false,
            SidebarTrustTarget::Chrome => {}
            SidebarTrustTarget::ConfirmTrust => self.persist_trust_change(true),
            SidebarTrustTarget::Revoke => self.persist_trust_change(false),
        }
    }

    fn persist_trust_change(&mut self, trust: bool) {
        let Some(store) = self.trust_store.clone() else {
            self.set_status("workspace trust persistence is unavailable");
            return;
        };
        if let Err(error) = persist_workspace_trust(&store, trust) {
            self.set_status(format!("failed to save workspace trust: {error:#}"));
            return;
        }
        self.trust_confirming = false;
        match store.resolve() {
            Ok(state) => {
                self.trust_state = state;
                self.clear_trust_status();
                self.status = None;
            }
            Err(error) => {
                self.trust_state = WorkspaceTrustState::Untrusted;
                self.set_trust_status(format!("workspace trust unavailable: {error:#}"));
            }
        }
        // Persistence already changed. Even if post-write verification failed,
        // restart Pi so its next generation resolves fail-closed instead of
        // retaining capabilities from the previous trusted process.
        if let Some(key) = trust_restart_key(self.launch_mode) {
            self.sessions.restart(key, Instant::now());
        }
    }

    fn save_layout_intent(&mut self) {
        let Some(store) = &self.layout_store else {
            return;
        };
        let intent = self.workbench.layout_intent(self.layout_config);
        match store.save(intent) {
            Ok(())
                if self.status.as_deref().is_some_and(|status| {
                    status.starts_with("failed to save layout:")
                        || status.starts_with("saved layout ignored:")
                }) =>
            {
                self.status = None;
            }
            Ok(()) => {}
            Err(error) => self.set_status(format!("failed to save layout: {error:#}")),
        }
    }

    fn set_trust_status(&mut self, message: String) {
        self.status = Some(message.clone());
        self.trust_status_message = Some(message);
    }

    fn clear_trust_status(&mut self) {
        if self.status.as_ref() == self.trust_status_message.as_ref() {
            self.status = None;
        }
        self.trust_status_message = None;
    }

    fn set_status(&mut self, message: impl Into<String>) {
        self.status = Some(message.into());
    }
}

impl Drop for AppRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn persist_workspace_trust(store: &WorkspaceTrustStore, trust: bool) -> Result<()> {
    if trust { store.trust() } else { store.revoke() }
}

fn open_managed_nvim_source_file_with<F>(
    source: &ProcessSource,
    running: bool,
    path: &std::path::Path,
    open: F,
) -> FileOpenDelivery
where
    F: FnOnce(&NvimController, &std::path::Path) -> Result<(), String>,
{
    if !running {
        return FileOpenDelivery::Unavailable;
    }
    let ProcessSource::ManagedNvim {
        current: Some(_),
        controller,
        ..
    } = source
    else {
        return FileOpenDelivery::Unavailable;
    };
    match open(controller, path) {
        Ok(()) => FileOpenDelivery::Opened,
        Err(error) => FileOpenDelivery::Failed(error),
    }
}

fn unavailable_pane_status(
    pane: PaneId,
    lifecycle_status: &str,
    action_status: Option<&str>,
) -> String {
    if pane == PaneId::Editor
        && let Some(action_status) = action_status
    {
        format!("{action_status}; {lifecycle_status}")
    } else {
        lifecycle_status.to_owned()
    }
}

fn apply_sidebar_open_result(
    workbench: &mut WorkbenchState,
    status: &mut Option<String>,
    result: FileOpenDelivery,
) {
    match result {
        FileOpenDelivery::Opened => {
            workbench.focus_pane(PaneId::Editor);
            *status = None;
        }
        FileOpenDelivery::Unavailable => {
            *status = Some("editor unavailable; file was not opened".to_string());
        }
        FileOpenDelivery::Failed(error) => {
            *status = Some(format!("failed to open file in editor: {error}"));
        }
    }
}

fn native_pi_source(
    workspace: &Workspace,
    trust_store: Option<WorkspaceTrustStore>,
) -> ProcessSource {
    ProcessSource::NativePi {
        workspace: workspace.clone(),
        location: NativePiLocation::Environment,
        trust_store,
    }
}

fn managed_nvim_source(workspace: &Workspace) -> Result<ProcessSource> {
    let profile = ManagedNvimProfile::from_environment()?;
    Ok(ProcessSource::ManagedNvim {
        profile,
        workspace: workspace.clone(),
        current: None,
        controller: NvimController::new(workspace.root().to_path_buf(), None),
    })
}

fn checked_process_spec(
    backend: impl BackendSpec,
    expected_kind: BackendKind,
    workspace: &Workspace,
) -> ProcessSpec {
    assert_eq!(
        backend.kind(),
        expected_kind,
        "backend {} has unexpected kind",
        backend.display_name()
    );
    backend.process_spec(workspace)
}

fn render_session(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    session: &TerminalSession,
    focused: bool,
    selection: Option<crate::terminal::TerminalRange>,
    status: Option<&str>,
    title_handle: bool,
) {
    let title = session_title_with_handle(session.display_name(), status, area.width, title_handle);
    render_terminal_pane(
        frame,
        area,
        session.screen(),
        &title,
        focused,
        selection,
        TerminalPaneStyle::default(),
    );
}

fn activated_layout_handle(
    gesture: PendingLayoutGesture,
    release_target: Option<MouseTarget>,
) -> Option<LayoutHandle> {
    let PendingLayoutGesture::Handle { handle, moved, .. } = gesture else {
        return None;
    };
    (!moved && release_target == Some(MouseTarget::Handle(handle))).then_some(handle)
}

fn canceled_layout_config(
    pending: &mut Option<PendingLayoutGesture>,
) -> Option<WorkbenchLayoutConfig> {
    match pending.take() {
        Some(PendingLayoutGesture::Drag { config, .. }) => Some(config),
        Some(PendingLayoutGesture::Handle { .. }) | None => None,
    }
}

fn layout_drag_config(
    divider: LayoutDivider,
    origin: MouseEvent,
    current: MouseEvent,
    layout: WorkbenchLayout,
    config: WorkbenchLayoutConfig,
) -> WorkbenchLayoutConfig {
    let dx = i32::from(current.column) - i32::from(origin.column);
    let dy = i32::from(current.row) - i32::from(origin.row);
    let mut next = config;
    match divider {
        LayoutDivider::SidebarMain => {
            let total = layout
                .sidebar
                .width
                .saturating_add(layout.editor.width)
                .saturating_add(layout.agent.width);
            let max = total.saturating_sub(MIN_TERMINAL_WIDTH.saturating_mul(2));
            next.sidebar_width = offset_clamped(config.sidebar_width, dx, MIN_TERMINAL_WIDTH, max);
        }
        LayoutDivider::EditorAgent => {
            let main = layout.editor.width.saturating_add(layout.agent.width);
            let width = offset_clamped(
                layout.editor.width,
                dx,
                MIN_TERMINAL_WIDTH,
                main.saturating_sub(MIN_TERMINAL_WIDTH),
            );
            next.editor_width = Some(width);
        }
        LayoutDivider::EditorBottom => {
            let max = layout
                .editor
                .height
                .saturating_add(layout.bottom.height)
                .saturating_sub(MIN_TERMINAL_HEIGHT);
            next.bottom_height =
                offset_clamped(config.bottom_height, -dy, MIN_SHELL_PANE_HEIGHT, max);
        }
    }
    next
}

fn offset_clamped(start: u16, delta: i32, min: u16, max: u16) -> u16 {
    (i32::from(start) + delta).clamp(i32::from(min), i32::from(max.max(min))) as u16
}

fn mouse_at(mut event: MouseEvent, row: u16, col: u16) -> MouseEvent {
    event.row = row;
    event.column = col;
    event
}

fn trust_confirmation_after_refresh(
    confirming: bool,
    previous: WorkspaceTrustState,
    current: WorkspaceTrustState,
    resolve_failed: bool,
) -> bool {
    confirming && !resolve_failed && previous == current
}

fn trust_capability_changed(previous: WorkspaceTrustState, current: WorkspaceTrustState) -> bool {
    (previous == WorkspaceTrustState::Trusted) != (current == WorkspaceTrustState::Trusted)
}

fn trust_restart_key(launch_mode: LaunchMode) -> Option<SessionKey> {
    match launch_mode {
        LaunchMode::Workbench => Some(SessionKey::Pane(PaneId::Agent)),
        LaunchMode::Pi => Some(SessionKey::Pane(PaneId::Editor)),
        LaunchMode::Shell | LaunchMode::Nvim => None,
    }
}

fn session_key(workbench: &WorkbenchState, pane: PaneId) -> SessionKey {
    if pane == PaneId::Bottom {
        SessionKey::Shell(workbench.shell_tabs().active())
    } else {
        SessionKey::Pane(pane)
    }
}

fn sidebar_viewport_rows(area: Rect) -> usize {
    const BORDER_ROWS: u16 = 2;
    usize::from(area.height.saturating_sub(BORDER_ROWS))
}

fn sidebar_tree_viewport_rows(area: Rect, trust: SidebarTrustChrome) -> usize {
    sidebar_viewport_rows(area).saturating_sub(sidebar_trust_rows(trust))
}

fn is_sidebar_double_click(
    previous: Option<&(std::path::PathBuf, Instant)>,
    path: &std::path::Path,
    now: Instant,
) -> bool {
    previous.is_some_and(|(previous_path, clicked_at)| {
        previous_path == path
            && now.saturating_duration_since(*clicked_at) <= SIDEBAR_DOUBLE_CLICK_INTERVAL
    })
}

fn activated_sidebar_row(pending: PendingMouseGesture, release: MouseEvent) -> Option<usize> {
    let MouseTarget::Sidebar { row, .. } = pending.target else {
        return None;
    };
    (!pending.moved && release.column == pending.down.column && release.row == pending.down.row)
        .then_some(usize::from(row))
}

fn pane_area(layout: WorkbenchLayout, pane: PaneId) -> Option<Rect> {
    match pane {
        PaneId::Editor => Some(layout.editor),
        PaneId::Agent => Some(layout.agent),
        PaneId::Bottom => Some(layout.bottom),
        PaneId::Sidebar => None,
    }
}

fn pointer_content_position(
    layout: WorkbenchLayout,
    pane: PaneId,
    event: MouseEvent,
) -> Option<(u16, u16)> {
    let area = pane_area(layout, pane)?;
    let bottom = pane == PaneId::Bottom;
    let size = if bottom {
        shell_terminal_content_size(area)
    } else {
        terminal_content_size(area)
    };
    let row = event
        .row
        .saturating_sub(area.y.saturating_add(if bottom { 2 } else { 1 }))
        .min(size.rows.saturating_sub(1));
    let col = event
        .column
        .saturating_sub(area.x.saturating_add(1))
        .min(size.cols.saturating_sub(1));
    Some((row, col))
}

fn edge_scroll_direction(layout: WorkbenchLayout, pane: PaneId, event: MouseEvent) -> i32 {
    let Some(area) = pane_area(layout, pane) else {
        return 0;
    };
    let content_top = area
        .y
        .saturating_add(if pane == PaneId::Bottom { 2 } else { 1 });
    let content_bottom = area.bottom().saturating_sub(2);
    if event.row <= content_top {
        1
    } else if event.row >= content_bottom {
        -1
    } else {
        0
    }
}

fn session_title_with_handle(
    display_name: &str,
    status: Option<&str>,
    pane_width: u16,
    title_handle: bool,
) -> String {
    let title_width = if title_handle {
        pane_width.saturating_sub(2)
    } else {
        pane_width
    };
    let title = session_title(display_name, status, title_width);
    if title_handle {
        format!("  {title}")
    } else {
        title
    }
}

fn session_title(display_name: &str, status: Option<&str>, pane_width: u16) -> String {
    let base_title = display_name.to_string();
    let available = usize::from(pane_width.saturating_sub(2));
    let remaining = available.saturating_sub(base_title.chars().count() + 3);
    match status.filter(|status| !status.is_empty() && remaining > 0) {
        Some(status) => format!("{base_title} — {}", truncate(status, remaining)),
        None => base_title,
    }
}

fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    if max_chars == 1 {
        return "…".to_string();
    }

    let mut truncated: String = value.chars().take(max_chars - 1).collect();
    truncated.push('…');
    truncated
}

fn context_menu_key_action(key: KeyEvent, copy_enabled: bool) -> ContextMenuKeyAction {
    if copy_enabled && key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::SUPER)
    {
        ContextMenuKeyAction::Copy
    } else if is_system_paste(key) {
        ContextMenuKeyAction::Paste
    } else {
        ContextMenuKeyAction::Dismiss
    }
}

fn context_menu_copy_enabled(workbench: &WorkbenchState, pane: PaneId) -> bool {
    workbench
        .selection()
        .is_some_and(|selection| selection.pane() == pane)
}

fn is_quit(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('q') && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn should_copy_selection(key: KeyEvent, has_selection: bool) -> bool {
    key.code == KeyCode::Char('c')
        && (key.modifiers.contains(KeyModifiers::SUPER)
            || (has_selection && key.modifiers.contains(KeyModifiers::CONTROL)))
}

fn is_system_paste(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('v') && key.modifiers.contains(KeyModifiers::SUPER)
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
    _state: TerminalStateGuard,
}

impl TerminalGuard {
    fn enter(capture_mouse: bool) -> Result<Self> {
        let state = TerminalStateGuard::enter(capture_mouse)?;
        let backend = CrosstermBackend::new(std::io::stdout());
        let terminal = Terminal::new(backend).context("failed to create ratatui terminal")?;
        Ok(Self {
            terminal,
            _state: state,
        })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.terminal.show_cursor();
    }
}

struct TerminalStateGuard {
    alternate_screen: bool,
    bracketed_paste: bool,
    keyboard_enhancement: bool,
    mouse_capture: bool,
}

impl TerminalStateGuard {
    fn enter(capture_mouse: bool) -> Result<Self> {
        enable_raw_mode().context("failed to enable raw mode")?;
        let mut state = Self {
            alternate_screen: false,
            bracketed_paste: false,
            keyboard_enhancement: false,
            mouse_capture: false,
        };
        let mut stdout = std::io::stdout();
        execute!(stdout, EnterAlternateScreen).context("failed to enter alternate screen")?;
        state.alternate_screen = true;
        execute!(stdout, EnableBracketedPaste).context("failed to enable bracketed paste")?;
        state.bracketed_paste = true;
        if capture_mouse {
            execute!(
                stdout,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,)
            )
            .context("failed to enable keyboard enhancement")?;
            state.keyboard_enhancement = true;
            execute!(stdout, EnableMouseCapture).context("failed to enable mouse capture")?;
            state.mouse_capture = true;
        }
        Ok(state)
    }
}

impl Drop for TerminalStateGuard {
    fn drop(&mut self) {
        if self.mouse_capture {
            let _ = execute!(std::io::stdout(), DisableMouseCapture);
        }
        if self.keyboard_enhancement {
            let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
        }
        if self.bracketed_paste {
            let _ = execute!(std::io::stdout(), DisableBracketedPaste);
        }
        if self.alternate_screen {
            let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
        }
        let _ = disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUICK_FAILURES_TO_PAUSE: usize = 6;
    const SIDEBAR_OPEN_SOAK_ITERATIONS: usize = 100;

    fn sleeping_shell_spec() -> ProcessSpec {
        let mut spec = ProcessSpec::new("/bin/sh").display_name("shell");
        spec.args = vec!["-c".to_string(), "sleep 30".to_string()];
        spec
    }

    #[test]
    fn stale_generation_does_not_stop_current_session() {
        let now = Instant::now();
        let mut registry = SessionRegistry::from_slots(
            [(
                SessionKey::Pane(PaneId::Editor),
                sleeping_shell_spec(),
                TerminalSize::new(20, 5),
            )],
            now,
        );
        let current = match &registry.slots[&SessionKey::Pane(PaneId::Editor)].state {
            SlotState::Running { identity, .. } => *identity,
            _ => panic!("session did not start"),
        };
        let stale = SessionIdentity {
            generation: current.generation.saturating_sub(1),
            ..current
        };

        registry
            .handle_failure(
                SessionKey::Pane(PaneId::Editor),
                stale,
                now,
                "stale exit".to_string(),
            )
            .unwrap();

        assert!(matches!(
            registry.slots[&SessionKey::Pane(PaneId::Editor)].state,
            SlotState::Running { identity, .. } if identity == current
        ));
        registry.shutdown();
    }

    #[test]
    fn shutdown_suppresses_retry_and_due_respawn() {
        let now = Instant::now();
        let mut registry = SessionRegistry::from_slots(
            [(
                SessionKey::Pane(PaneId::Editor),
                sleeping_shell_spec(),
                TerminalSize::new(20, 5),
            )],
            now,
        );
        registry.shutdown();
        registry.retry(
            SessionKey::Pane(PaneId::Editor),
            now + Duration::from_secs(1),
        );
        registry.poll(now + Duration::from_secs(60)).unwrap();

        assert!(matches!(
            registry.slots[&SessionKey::Pane(PaneId::Editor)].state,
            SlotState::Paused { .. }
        ));
    }

    #[test]
    fn nvim_and_pi_crash_loops_pause_independently() {
        fn crash_until_paused(
            registry: &mut SessionRegistry,
            key: SessionKey,
            mut now: Instant,
        ) -> Instant {
            for failure in 0..QUICK_FAILURES_TO_PAUSE {
                let identity = match &registry.slots[&key].state {
                    SlotState::Running { identity, .. } => *identity,
                    _ => panic!("expected running slot before failure {failure}"),
                };
                now += Duration::from_millis(1);
                registry
                    .handle_failure(key, identity, now, format!("quick failure {failure}"))
                    .unwrap();
                if failure + 1 == QUICK_FAILURES_TO_PAUSE {
                    assert!(matches!(
                        registry.slots[&key].state,
                        SlotState::Paused { .. }
                    ));
                    break;
                }
                let until = match registry.slots[&key].state {
                    SlotState::Backoff { until, .. } => until,
                    _ => panic!("expected backoff after failure {failure}"),
                };
                registry.poll(until).unwrap();
                assert!(matches!(
                    registry.slots[&key].state,
                    SlotState::Running { .. }
                ));
                now = until;
            }
            now
        }

        let now = Instant::now();
        let mut spec = ProcessSpec::new("/bin/sleep");
        spec.args = vec!["30".to_string()];
        let editor = SessionKey::Pane(PaneId::Editor);
        let agent = SessionKey::Pane(PaneId::Agent);
        let mut registry = SessionRegistry::from_slots(
            [
                (editor, spec.clone(), TerminalSize::new(20, 5)),
                (agent, spec, TerminalSize::new(20, 5)),
            ],
            now,
        );

        let now = crash_until_paused(&mut registry, agent, now);
        assert!(matches!(
            registry.slots[&editor].state,
            SlotState::Running { .. }
        ));
        crash_until_paused(&mut registry, editor, now);
        assert!(matches!(
            registry.slots[&agent].state,
            SlotState::Paused { .. }
        ));
        registry.shutdown();
    }

    #[test]
    fn unavailable_status_uses_reachable_retry_hint() {
        assert_eq!(retry_hint(true), "click to retry");
        assert_eq!(retry_hint(false), "press any key to retry");
    }

    #[test]
    fn bounds_status_to_available_title_width() {
        assert_eq!(
            session_title("pi", Some("clipboard unavailable"), 20),
            "pi — clipboard un…"
        );
        assert_eq!(session_title("nvim", None, 20), "nvim");
        assert_eq!(
            session_title_with_handle("shell", None, 20, true),
            "  shell"
        );
        assert!(!session_title("shell", None, 20).contains("Ctrl+Q"));
    }

    #[test]
    fn context_menu_keys_are_consumed_and_copy_is_pane_owned() {
        let copy = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::SUPER);
        let paste = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::SUPER);
        let control_copy = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let ordinary = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(
            context_menu_key_action(copy, true),
            ContextMenuKeyAction::Copy
        );
        assert_eq!(
            context_menu_key_action(copy, false),
            ContextMenuKeyAction::Dismiss
        );
        assert_eq!(
            context_menu_key_action(control_copy, true),
            ContextMenuKeyAction::Dismiss
        );
        assert_eq!(
            context_menu_key_action(paste, false),
            ContextMenuKeyAction::Paste
        );
        assert_eq!(
            context_menu_key_action(escape, true),
            ContextMenuKeyAction::Dismiss
        );
        assert_eq!(
            context_menu_key_action(ordinary, true),
            ContextMenuKeyAction::Dismiss
        );

        let mut workbench = WorkbenchState::default();
        workbench.begin_selection(crate::terminal::TerminalPoint::new(0, 0));
        assert!(context_menu_copy_enabled(&workbench, PaneId::Editor));
        let agent_copy_enabled = context_menu_copy_enabled(&workbench, PaneId::Agent);
        assert!(!agent_copy_enabled);
        assert_eq!(
            context_menu_key_action(copy, agent_copy_enabled),
            ContextMenuKeyAction::Dismiss
        );
    }

    #[test]
    fn recognizes_system_clipboard_keys() {
        assert!(should_copy_selection(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::SUPER),
            false,
        ));
        assert!(should_copy_selection(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            true,
        ));
        assert!(is_system_paste(KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::SUPER
        )));
        assert!(!should_copy_selection(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            false,
        ));
    }

    fn mouse(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn layout_drags_use_snapshot_delta_capture_and_clamp_all_terminals() {
        let config = WorkbenchLayoutConfig::default();
        let layout = WorkbenchLayout::calculate(Rect::new(0, 0, 120, 40), config);

        // A move is calculated from the down snapshot, not a prior move.
        let sidebar = layout_drag_config(
            LayoutDivider::SidebarMain,
            mouse(layout.editor.x, 2),
            mouse(layout.editor.x + 10, 2),
            layout,
            config,
        );
        assert_eq!(sidebar.sidebar_width, config.sidebar_width + 10);

        // The editor/agent divider follows the captured pointer cell exactly.
        let split = layout_drag_config(
            LayoutDivider::EditorAgent,
            mouse(layout.agent.x, 2),
            mouse(layout.agent.x + 7, 2),
            layout,
            config,
        );
        assert_eq!(split.editor_width, Some(layout.editor.width + 7));
        let split_layout = WorkbenchLayout::calculate(Rect::new(0, 0, 120, 40), split);
        assert_eq!(split_layout.editor.width, layout.editor.width + 7);

        // Coordinates far outside the pane remain captured and clamp both sides
        // to a bordered-terminal minimum.
        let clamped_split = layout_drag_config(
            LayoutDivider::EditorAgent,
            mouse(layout.agent.x, 2),
            mouse(u16::MAX, 2),
            layout,
            config,
        );
        let clamped_layout = WorkbenchLayout::calculate(Rect::new(0, 0, 120, 40), clamped_split);
        assert!(clamped_layout.editor.width >= MIN_TERMINAL_WIDTH);
        assert!(clamped_layout.agent.width >= MIN_TERMINAL_WIDTH);

        let bottom = layout_drag_config(
            LayoutDivider::EditorBottom,
            mouse(30, layout.bottom.y),
            mouse(30, 0),
            layout,
            config,
        );
        let bottom_layout = WorkbenchLayout::calculate(Rect::new(0, 0, 120, 40), bottom);
        assert!(bottom_layout.editor.height >= MIN_TERMINAL_HEIGHT);
        assert!(bottom_layout.bottom.height >= MIN_SHELL_PANE_HEIGHT);
    }

    #[test]
    fn sidebar_click_requires_an_unmoved_pointer_on_the_same_cell() {
        let down = mouse(4, 3);
        let pending = PendingMouseGesture {
            target: MouseTarget::Sidebar { row: 2, col: 3 },
            down,
            current: down,
            dragging: false,
            moved: false,
            next_edge_scroll: Instant::now(),
        };
        assert_eq!(activated_sidebar_row(pending, down), Some(2));
        assert_eq!(activated_sidebar_row(pending, mouse(5, 3)), None);
        assert_eq!(
            activated_sidebar_row(
                PendingMouseGesture {
                    moved: true,
                    ..pending
                },
                down
            ),
            None
        );
    }

    #[test]
    fn sidebar_double_click_requires_same_path_within_interval() {
        let now = Instant::now();
        let path = std::path::PathBuf::from("src/main.rs");
        let previous = (path.clone(), now);
        assert!(is_sidebar_double_click(
            Some(&previous),
            &path,
            now + Duration::from_millis(399)
        ));
        assert!(!is_sidebar_double_click(
            Some(&previous),
            std::path::Path::new("src/lib.rs"),
            now + Duration::from_millis(20)
        ));
        assert!(!is_sidebar_double_click(
            Some(&previous),
            &path,
            now + Duration::from_millis(401)
        ));
        assert!(!is_sidebar_double_click(None, &path, now));
    }

    #[test]
    fn handle_release_requires_an_unmoved_pointer_on_the_same_handle() {
        let origin = mouse(24, 0);
        let handle = PendingLayoutGesture::Handle {
            handle: LayoutHandle::Sidebar,
            origin,
            moved: false,
        };
        assert_eq!(
            activated_layout_handle(handle, Some(MouseTarget::Handle(LayoutHandle::Sidebar))),
            Some(LayoutHandle::Sidebar)
        );

        let moved = PendingLayoutGesture::Handle {
            handle: LayoutHandle::Sidebar,
            origin,
            moved: true,
        };
        assert_eq!(
            activated_layout_handle(moved, Some(MouseTarget::Handle(LayoutHandle::Sidebar))),
            None
        );
        assert_eq!(
            activated_layout_handle(handle, Some(MouseTarget::Sidebar { row: 0, col: 0 })),
            None
        );
    }

    #[test]
    fn canceled_drag_returns_its_snapshot_for_rollback() {
        let snapshot = WorkbenchLayoutConfig::default();
        let layout = WorkbenchLayout::calculate(Rect::new(0, 0, 120, 40), snapshot);
        let mut pending = Some(PendingLayoutGesture::Drag {
            divider: LayoutDivider::EditorAgent,
            origin: mouse(layout.agent.x, 2),
            layout,
            config: snapshot,
        });

        assert_eq!(canceled_layout_config(&mut pending), Some(snapshot));
        assert!(pending.is_none());

        let mut handle = Some(PendingLayoutGesture::Handle {
            handle: LayoutHandle::Sidebar,
            origin: mouse(1, 0),
            moved: false,
        });
        assert_eq!(canceled_layout_config(&mut handle), None);
        assert!(handle.is_none());
    }

    #[test]
    fn bottom_terminal_coordinates_skip_tab_chrome() {
        let layout =
            WorkbenchLayout::calculate(Rect::new(0, 0, 120, 40), WorkbenchLayoutConfig::default());
        let event = MouseEvent {
            kind: MouseEventKind::Moved,
            column: layout.bottom.x + 3,
            row: layout.bottom.y + 2,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            pointer_content_position(layout, PaneId::Bottom, event),
            Some((0, 2))
        );
        assert_eq!(edge_scroll_direction(layout, PaneId::Bottom, event), 1);
    }

    #[test]
    fn managed_nvim_restart_replaces_controller_endpoint() {
        let root = std::env::temp_dir().join(format!("ami-runtime-nvim-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let workspace = Workspace::discover(&root).unwrap();
        let profile = ManagedNvimProfile::materialize(root.join("state")).unwrap();
        let mut source = ProcessSource::ManagedNvim {
            profile,
            workspace: workspace.clone(),
            current: None,
            controller: NvimController::new(workspace.root().to_path_buf(), None),
        };
        source.next_spec().unwrap();
        let first = match &source {
            ProcessSource::ManagedNvim { controller, .. } => {
                controller.endpoint().unwrap().to_path_buf()
            }
            ProcessSource::Static(_) | ProcessSource::NativePi { .. } => unreachable!(),
        };
        source.next_spec().unwrap();
        let second = match &source {
            ProcessSource::ManagedNvim { controller, .. } => {
                controller.endpoint().unwrap().to_path_buf()
            }
            ProcessSource::Static(_) | ProcessSource::NativePi { .. } => unreachable!(),
        };
        assert_ne!(first, second);
        assert!(!first.exists());
        std::fs::write(&second, b"ready").unwrap();
        let remote = match &source {
            ProcessSource::ManagedNvim { controller, .. } => {
                controller.remote_open_spec("replacement.txt").unwrap()
            }
            ProcessSource::Static(_) | ProcessSource::NativePi { .. } => unreachable!(),
        };
        assert_eq!(remote.args[1], second.to_string_lossy());
        source.cleanup();
        assert!(!second.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unavailable_editor_composes_file_action_and_lifecycle_status() {
        assert_eq!(
            unavailable_pane_status(
                PaneId::Editor,
                "starting…",
                Some("editor unavailable; file was not opened")
            ),
            "editor unavailable; file was not opened; starting…"
        );
        assert_eq!(
            unavailable_pane_status(PaneId::Agent, "starting…", Some("clipboard error")),
            "starting…"
        );
    }

    #[test]
    fn sidebar_open_focuses_editor_only_on_success_and_reports_failures() {
        let mut workbench = WorkbenchState::default();
        workbench.focus_pane(PaneId::Sidebar);
        let mut status = Some("old".to_string());

        apply_sidebar_open_result(&mut workbench, &mut status, FileOpenDelivery::Unavailable);
        assert_eq!(workbench.focused_pane(), PaneId::Sidebar);
        assert_eq!(
            status.as_deref(),
            Some("editor unavailable; file was not opened")
        );

        apply_sidebar_open_result(
            &mut workbench,
            &mut status,
            FileOpenDelivery::Failed("remote failed".to_string()),
        );
        assert_eq!(workbench.focused_pane(), PaneId::Sidebar);
        assert!(status.as_deref().unwrap().contains("remote failed"));

        apply_sidebar_open_result(&mut workbench, &mut status, FileOpenDelivery::Opened);
        assert_eq!(workbench.focused_pane(), PaneId::Editor);
        assert_eq!(status, None);
    }

    #[test]
    fn managed_nvim_file_open_loop_delivers_once_per_activation_only_while_running() {
        use std::cell::Cell;

        let temp = tempfile::TempDir::new().unwrap();
        let workspace_dir = temp.path().join("workspace");
        std::fs::create_dir(&workspace_dir).unwrap();
        let file = workspace_dir.join("file");
        std::fs::write(&file, b"").unwrap();
        let workspace = Workspace::discover(&workspace_dir).unwrap();
        let profile = ManagedNvimProfile::materialize(temp.path().join("state")).unwrap();
        let mut source = ProcessSource::ManagedNvim {
            profile,
            workspace: workspace.clone(),
            current: None,
            controller: NvimController::new(workspace.root().to_path_buf(), None),
        };
        source.next_spec().unwrap();
        let calls = Cell::new(0);
        for _ in 0..SIDEBAR_OPEN_SOAK_ITERATIONS {
            let result = open_managed_nvim_source_file_with(&source, true, &file, |_, opened| {
                calls.set(calls.get() + 1);
                assert_eq!(opened, file);
                Ok(())
            });
            assert_eq!(result, FileOpenDelivery::Opened);
        }
        assert_eq!(calls.get(), SIDEBAR_OPEN_SOAK_ITERATIONS);

        let result = open_managed_nvim_source_file_with(&source, false, &file, |_, _| {
            calls.set(calls.get() + 1);
            Ok(())
        });
        assert_eq!(result, FileOpenDelivery::Unavailable);
        assert_eq!(calls.get(), SIDEBAR_OPEN_SOAK_ITERATIONS);
        source.cleanup();
    }

    #[test]
    fn managed_nvim_file_open_failure_is_not_retried() {
        use std::cell::Cell;

        let temp = tempfile::TempDir::new().unwrap();
        let workspace_dir = temp.path().join("workspace");
        std::fs::create_dir(&workspace_dir).unwrap();
        let workspace = Workspace::discover(&workspace_dir).unwrap();
        let profile = ManagedNvimProfile::materialize(temp.path().join("state")).unwrap();
        let mut source = ProcessSource::ManagedNvim {
            profile,
            workspace: workspace.clone(),
            current: None,
            controller: NvimController::new(workspace.root().to_path_buf(), None),
        };
        source.next_spec().unwrap();
        let calls = Cell::new(0);
        let result = open_managed_nvim_source_file_with(&source, true, workspace.root(), |_, _| {
            calls.set(calls.get() + 1);
            Err("remote failed".to_string())
        });
        assert_eq!(
            result,
            FileOpenDelivery::Failed("remote failed".to_string())
        );
        assert_eq!(calls.get(), 1);
        source.cleanup();
    }

    #[test]
    fn registry_keeps_independent_shell_slots_and_resizes_inactive_slot() {
        let spec = sleeping_shell_spec();
        let mut registry = SessionRegistry {
            slots: [ShellTabId(1), ShellTabId(2)]
                .into_iter()
                .map(|id| {
                    (
                        SessionKey::Shell(id),
                        SessionSlot {
                            source: ProcessSource::Static(spec.clone()),
                            spec: spec.clone(),
                            size: TerminalSize::new(10, 4),
                            generation: 0,
                            policy: RestartPolicy::default(),
                            state: SlotState::Starting,
                        },
                    )
                })
                .collect(),
            ids: SessionIds::new(),
            shell_spec: Some(spec),
        };
        registry
            .resize_slot(
                SessionKey::Shell(ShellTabId(1)),
                TerminalSize::new(40, 12),
                Instant::now(),
            )
            .unwrap();
        registry
            .resize_slot(
                SessionKey::Shell(ShellTabId(2)),
                TerminalSize::new(40, 12),
                Instant::now(),
            )
            .unwrap();
        assert!(
            registry
                .slots
                .values()
                .all(|slot| slot.size == TerminalSize::new(40, 12))
        );
    }

    #[test]
    fn failed_trust_write_returns_before_any_restart_decision() {
        let temp = tempfile::TempDir::new().unwrap();
        let workspace = temp.path().join("workspace");
        let state = temp.path().join("state");
        std::fs::create_dir(&workspace).unwrap();
        let store = WorkspaceTrustStore::new(&workspace, &state).unwrap();
        let trust_directory = state.join("trust");
        std::fs::remove_dir(&trust_directory).unwrap();
        std::fs::write(&trust_directory, b"not a directory").unwrap();
        assert!(persist_workspace_trust(&store, true).is_err());
    }

    #[test]
    fn trust_refresh_cancels_stale_confirmation_prompts() {
        assert!(!trust_confirmation_after_refresh(
            true,
            WorkspaceTrustState::Untrusted,
            WorkspaceTrustState::Trusted,
            false
        ));
        assert!(!trust_confirmation_after_refresh(
            true,
            WorkspaceTrustState::Untrusted,
            WorkspaceTrustState::Untrusted,
            true
        ));
        assert!(trust_confirmation_after_refresh(
            true,
            WorkspaceTrustState::Untrusted,
            WorkspaceTrustState::Untrusted,
            false
        ));
        assert!(!trust_confirmation_after_refresh(
            false,
            WorkspaceTrustState::Untrusted,
            WorkspaceTrustState::Untrusted,
            false
        ));
    }

    #[test]
    fn trust_capability_transitions_restart_only_the_agent_slot() {
        assert!(trust_capability_changed(
            WorkspaceTrustState::Trusted,
            WorkspaceTrustState::Untrusted
        ));
        assert!(trust_capability_changed(
            WorkspaceTrustState::Stale,
            WorkspaceTrustState::Trusted
        ));
        assert!(!trust_capability_changed(
            WorkspaceTrustState::Untrusted,
            WorkspaceTrustState::Stale
        ));
        assert_eq!(
            trust_restart_key(LaunchMode::Workbench),
            Some(SessionKey::Pane(PaneId::Agent))
        );
        assert_eq!(
            trust_restart_key(LaunchMode::Pi),
            Some(SessionKey::Pane(PaneId::Editor))
        );
        assert_eq!(trust_restart_key(LaunchMode::Nvim), None);
        assert_eq!(trust_restart_key(LaunchMode::Shell), None);
    }

    #[test]
    fn native_pi_generation_resolves_trust_each_time_and_keeps_global_resources() {
        let temp = tempfile::TempDir::new().unwrap();
        let workspace_path = temp.path().join("workspace");
        std::fs::create_dir(&workspace_path).unwrap();
        let workspace = Workspace::discover(&workspace_path).unwrap();
        let store =
            WorkspaceTrustStore::new(&workspace_path, temp.path().join("trust-state")).unwrap();
        let mut source = ProcessSource::NativePi {
            workspace,
            location: NativePiLocation::Explicit {
                state_root: temp.path().join("pi-state"),
            },
            trust_store: Some(store.clone()),
        };

        let first = source.next_spec().unwrap();
        assert!(first.args.iter().any(|arg| arg == "--no-approve"));
        assert!(!first.args.iter().any(|arg| arg == "--no-extensions"));
        store.trust().unwrap();
        let trusted = source.next_spec().unwrap();
        assert!(trusted.args.iter().any(|arg| arg == "--approve"));
        assert!(!trusted.args.iter().any(|arg| arg.starts_with("--no-")));

        let old = temp.path().join("old-workspace");
        std::fs::rename(&workspace_path, old).unwrap();
        std::fs::create_dir(&workspace_path).unwrap();
        let stale = source.next_spec().unwrap();
        assert!(stale.args.iter().any(|arg| arg == "--no-approve"));
        assert!(!stale.args.iter().any(|arg| arg == "--no-extensions"));
    }

    #[test]
    fn sidebar_tree_viewport_excludes_normal_and_prompt_chrome() {
        let area = Rect::new(0, 0, 24, 12);
        let normal = SidebarTrustChrome {
            state: WorkspaceTrustState::Untrusted,
            confirming: false,
        };
        let prompt = SidebarTrustChrome {
            confirming: true,
            ..normal
        };
        assert_eq!(sidebar_tree_viewport_rows(area, normal), 9);
        assert_eq!(sidebar_tree_viewport_rows(area, prompt), 6);
    }

    #[cfg(unix)]
    #[test]
    fn native_pi_session_preparation_failure_is_lazy_and_retryable() {
        let temp = tempfile::TempDir::new().unwrap();
        let workspace_dir = temp.path().join("workspace");
        std::fs::create_dir(&workspace_dir).unwrap();
        let workspace = Workspace::discover(workspace_dir).unwrap();
        let state_root = temp.path().join("state");
        let profile = NativePiProfile::new(&state_root).unwrap();
        let session_dir = profile.session_dir(&workspace).unwrap();
        std::fs::create_dir_all(session_dir.parent().unwrap()).unwrap();
        std::fs::write(&session_dir, b"not a directory").unwrap();
        let mut source = ProcessSource::NativePi {
            workspace,
            location: NativePiLocation::Explicit { state_root },
            trust_store: None,
        };

        assert!(source.next_spec().is_err());
        std::fs::remove_file(session_dir).unwrap();
        let spec = source.next_spec().unwrap();
        assert_eq!(spec.display_name, "pi");
    }

    #[test]
    fn detects_vertical_selection_edges() {
        let layout =
            WorkbenchLayout::calculate(Rect::new(0, 0, 120, 40), WorkbenchLayoutConfig::default());
        let event = |row| MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: layout.editor.x + 2,
            row,
            modifiers: KeyModifiers::NONE,
        };

        assert_eq!(edge_scroll_direction(layout, PaneId::Editor, event(1)), 1);
        assert_eq!(edge_scroll_direction(layout, PaneId::Editor, event(10)), 0);
        assert_eq!(
            edge_scroll_direction(layout, PaneId::Editor, event(layout.editor.bottom() - 2)),
            -1
        );
    }
}
