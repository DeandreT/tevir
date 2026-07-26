use std::{
    collections::BTreeMap,
    net::SocketAddr,
    num::NonZeroU32,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use discovery::{DiscoveredNode, DiscoveryService, NearbyNodes};
use domain::{
    Edge, GridSlot, NodeId, Point, ScreenPlacement, Size, TOPOLOGY_COLUMNS, TOPOLOGY_ROWS, Topology,
};
use eframe::egui::{
    self, Align, Button, Color32, ComboBox, CornerRadius, FontFamily, FontId, Frame, Layout,
    Margin, RichText, ScrollArea, Sense, Stroke, TextEdit, TextStyle, Ui, Vec2, ViewportBuilder,
};
use identity::{IdentityStore, LocalIdentity, PairingBundle, TrustStore};
use platform::{EnvironmentStatus, PlatformReport};
use protocol::Capabilities;
use telemetry::{LogBuffer, LogEntry, LogLevel};

use crate::{
    config::{Config, Role},
    runtime::{NativeInputHost, RuntimeEvent, RuntimeRole, SessionRuntime},
    settings::{DesktopSettings, SettingsError},
};

const ACCENT: Color32 = Color32::from_rgb(50, 185, 164);
const ACCENT_MUTED: Color32 = Color32::from_rgb(25, 91, 82);
const SUCCESS: Color32 = Color32::from_rgb(67, 190, 120);
const WARNING: Color32 = Color32::from_rgb(225, 164, 67);
const DANGER: Color32 = Color32::from_rgb(224, 91, 91);
const TEXT: Color32 = Color32::from_rgb(231, 234, 232);
const MUTED: Color32 = Color32::from_rgb(151, 158, 155);
const CANVAS: Color32 = Color32::from_rgb(22, 24, 25);
const PANEL: Color32 = Color32::from_rgb(28, 31, 32);
const ELEVATED: Color32 = Color32::from_rgb(36, 39, 40);
const BORDER: Color32 = Color32::from_rgb(58, 63, 62);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Page {
    Status,
    Configuration,
    Pairing,
    Diagnostics,
    Logs,
}

impl Page {
    const ALL: [(Self, &'static str); 5] = [
        (Self::Status, "Status"),
        (Self::Configuration, "Configuration"),
        (Self::Pairing, "Pairing"),
        (Self::Diagnostics, "Diagnostics"),
        (Self::Logs, "Logs"),
    ];
}

pub struct DesktopApp {
    data_directory: PathBuf,
    settings: DesktopSettings,
    identity: Option<LocalIdentity>,
    trust: Option<TrustStore>,
    discovery: Option<DiscoveryService>,
    nearby: NearbyNodes,
    discovery_error: Option<String>,
    page: Page,
    node_input: String,
    pairing_bundle_input: String,
    pairing_code_input: String,
    config_path_input: String,
    config_editor: ConfigEditor,
    saved_config: Option<Config>,
    startup_config: Option<Config>,
    native_input: Option<NativeInputHost>,
    native_retry_at: Option<Instant>,
    session_runtime: Option<SessionRuntime>,
    session_state: SessionState,
    report: PlatformReport,
    notice: Option<Notice>,
    config_summary: Option<String>,
    confirm_remove: Option<NodeId>,
    logs: LogBuffer,
    local_desktop: Option<platform::DesktopGeometry>,
}

impl DesktopApp {
    pub fn load(
        data_directory: PathBuf,
        node_override: Option<NodeId>,
        logs: LogBuffer,
    ) -> Result<Self, AppError> {
        let mut settings = DesktopSettings::load(&data_directory)?;
        if let Some(node) = node_override {
            settings.node = Some(node);
            settings.save(&data_directory)?;
        }

        let (identity, trust, notice) = if let Some(node) = settings.node.as_ref() {
            match load_identity(&data_directory, node) {
                Ok((identity, trust)) => (Some(identity), Some(trust), None),
                Err(error) => (
                    None,
                    None,
                    Some({
                        tracing::error!(node = %node, error = %error, "identity unavailable");
                        Notice::error(format!("Identity unavailable: {error}"))
                    }),
                ),
            }
        } else {
            (None, None, None)
        };

        let report = platform::probe_host();
        let config_path_input = settings
            .config_path
            .clone()
            .unwrap_or_else(|| data_directory.join("config.toml"))
            .display()
            .to_string();
        let config_editor = ConfigEditor::for_node(settings.node.as_ref());
        let load_saved_config = settings.config_path.is_some();
        let mut app = Self {
            data_directory,
            node_input: settings
                .node
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            settings,
            identity,
            trust,
            discovery: None,
            nearby: NearbyNodes::default(),
            discovery_error: None,
            page: initial_page(),
            pairing_bundle_input: String::new(),
            pairing_code_input: String::new(),
            config_path_input,
            config_editor,
            saved_config: None,
            startup_config: None,
            native_input: None,
            native_retry_at: None,
            session_runtime: None,
            session_state: SessionState::default(),
            report,
            notice,
            config_summary: None,
            confirm_remove: None,
            logs,
            local_desktop: None,
        };
        app.start_discovery();
        if load_saved_config && app.identity.is_some() {
            app.restore_config();
        }
        Ok(app)
    }

    fn create_identity(&mut self) {
        let node = match NodeId::new(self.node_input.trim()) {
            Ok(node) => node,
            Err(error) => {
                tracing::warn!(error = %error, "invalid node identity");
                self.notice = Some(Notice::error(error.to_string()));
                return;
            }
        };
        match load_identity(&self.data_directory, &node) {
            Ok((identity, trust)) => {
                self.settings.node = Some(node.clone());
                if let Err(error) = self.settings.save(&self.data_directory) {
                    tracing::error!(node = %node, error = %error, "could not save desktop settings");
                    self.notice = Some(Notice::error(error.to_string()));
                    return;
                }
                self.identity = Some(identity);
                self.trust = Some(trust);
                self.config_editor = ConfigEditor::for_node(Some(&node));
                self.stop_session();
                self.saved_config = None;
                self.start_discovery();
                tracing::info!(node = %node, "local identity ready");
                self.notice = Some(Notice::success("Local identity ready"));
            }
            Err(error) => {
                tracing::error!(node = %node, error, "could not initialize local identity");
                self.notice = Some(Notice::error(error));
            }
        }
    }

    fn start_discovery(&mut self) {
        self.discovery = None;
        self.nearby = NearbyNodes::default();
        self.discovery_error = None;
        let Some(identity) = self.identity.as_ref() else {
            return;
        };
        match DiscoveryService::start(
            identity.pairing_bundle(),
            self.report.platform,
            advertised_capabilities(),
            self.config_editor.discovery_port(),
        ) {
            Ok(discovery) => self.discovery = Some(discovery),
            Err(error) => {
                tracing::warn!(error = %error, "local network discovery unavailable");
                self.discovery_error = Some(error.to_string());
            }
        }
    }

    fn poll_discovery(&mut self) {
        let Some(discovery) = self.discovery.as_ref() else {
            return;
        };
        let result = discovery.poll(&mut self.nearby);
        if let Some(error) = result.error {
            self.discovery_error = Some(error);
        }
    }

    fn select_discovered(&mut self, node: &DiscoveredNode) {
        self.pairing_bundle_input = node.pairing_bundle().encode();
        self.pairing_code_input.clear();
        tracing::info!(peer = %node.node(), "nearby node selected for pairing");
        self.notice = Some(Notice::info(format!(
            "{} selected; verification required",
            node.node()
        )));
    }

    fn import_pairing(&mut self) {
        let bundle = match PairingBundle::decode(&self.pairing_bundle_input) {
            Ok(bundle) => bundle,
            Err(error) => {
                tracing::warn!(error = %error, "pairing bundle rejected");
                self.notice = Some(Notice::error(error.to_string()));
                return;
            }
        };
        let node = bundle.node().clone();
        let Some(trust) = self.trust.as_mut() else {
            self.notice = Some(Notice::error("Local identity is not ready"));
            return;
        };
        match trust.trust(bundle, &self.pairing_code_input) {
            Ok(()) => {
                tracing::info!(peer = %node, "peer trusted");
                self.pairing_bundle_input.clear();
                self.pairing_code_input.clear();
                if self.config_editor.role == ConfigRole::Agent
                    && self.config_editor.controller_node == "peer-node"
                {
                    self.config_editor.controller_node = node.to_string();
                    self.use_discovered_controller_address();
                }
                self.notice = Some(Notice::success(format!("Paired with {node}")));
                if (self.session_runtime.is_some()
                    || self.session_state.phase == SessionPhase::Failed)
                    && let Some(config) = self.saved_config.clone()
                {
                    self.start_session(config);
                }
            }
            Err(error) => {
                tracing::warn!(peer = %node, error = %error, "peer trust rejected");
                self.notice = Some(Notice::error(error.to_string()));
            }
        }
    }

    fn remove_peer(&mut self, node: &NodeId) {
        let Some(trust) = self.trust.as_mut() else {
            return;
        };
        match trust.remove(node) {
            Ok(true) => {
                tracing::info!(peer = %node, "peer trust removed");
                self.stop_session();
                self.notice = Some(Notice::success(format!("Removed {node}")));
            }
            Ok(false) => {
                tracing::warn!(peer = %node, "peer was not trusted");
                self.notice = Some(Notice::error(format!("{node} is not paired")));
            }
            Err(error) => {
                tracing::error!(peer = %node, error = %error, "could not remove peer trust");
                self.notice = Some(Notice::error(error.to_string()));
            }
        }
        self.confirm_remove = None;
    }

    fn load_config(&mut self) {
        self.load_config_from_path(true);
    }

    fn restore_config(&mut self) {
        self.load_config_from_path(false);
    }

    fn load_config_from_path(&mut self, start_session: bool) {
        let path = PathBuf::from(self.config_path_input.trim());
        match Config::load(&path) {
            Ok(mut config) => {
                let Some(local_node) = self.identity.as_ref().map(LocalIdentity::node).cloned()
                else {
                    self.notice = Some(Notice::error("Local identity is not ready"));
                    return;
                };
                if config.node != local_node {
                    let message = format!(
                        "Configuration belongs to `{}`, not `{local_node}`",
                        config.node
                    );
                    tracing::warn!(
                        path = %path.display(),
                        configured_node = %config.node,
                        local_node = %local_node,
                        "configuration node mismatch"
                    );
                    self.notice = Some(Notice::error(message));
                    return;
                }
                self.config_editor = ConfigEditor::from_config(&config);
                if let Some(geometry) = self.local_desktop {
                    let _ = self.apply_local_desktop_to_editor(geometry);
                    if let Ok(updated) = self.config_editor.build(local_node.clone()) {
                        config = updated;
                    }
                }
                self.config_summary = Some(config_summary(&config));
                self.saved_config = Some(config.clone());
                self.remember_config_path(&path);
                self.start_discovery();
                tracing::info!(path = %path.display(), "configuration loaded");
                self.notice = Some(Notice::success("Configuration loaded"));
                if start_session {
                    self.start_session(config);
                } else {
                    self.startup_config = Some(config);
                }
            }
            Err(error) => {
                self.config_summary = None;
                self.saved_config = None;
                tracing::warn!(path = %path.display(), error = %error, "configuration load failed");
                self.notice = Some(Notice::error(error.to_string()));
            }
        }
    }

    fn start_deferred_session(&mut self) {
        let Some(mut config) = self.startup_config.take() else {
            return;
        };
        if self.local_desktop.is_some()
            && let Some(local_node) = self.identity.as_ref().map(LocalIdentity::node)
            && let Ok(updated) = self.config_editor.build(local_node.clone())
        {
            config = updated;
            self.saved_config = Some(config.clone());
            self.config_summary = Some(config_summary(&config));
        }
        self.start_session(config);
    }

    fn sync_desktop_geometry(&mut self, frame: &eframe::Frame, context: &egui::Context) {
        let geometry = desktop_geometry(frame)
            .or_else(|| current_monitor_geometry(context))
            .filter(|geometry| Some(*geometry) != self.local_desktop);
        let Some(geometry) = geometry else {
            return;
        };
        self.local_desktop = Some(geometry);
        let result = self.apply_local_desktop_to_editor(geometry);
        match result {
            Ok(()) => tracing::info!(
                width = geometry.size.width.get(),
                height = geometry.size.height.get(),
                monitors = geometry.monitor_count,
                "local desktop geometry detected"
            ),
            Err(error) => tracing::warn!(error, "local desktop autofill failed"),
        }
    }

    fn apply_local_desktop_to_editor(
        &mut self,
        geometry: platform::DesktopGeometry,
    ) -> Result<(), String> {
        let Some(local_node) = self.identity.as_ref().map(LocalIdentity::node) else {
            return Ok(());
        };
        match self.config_editor.role {
            ConfigRole::Controller => self.config_editor.set_local_desktop(local_node, geometry),
            ConfigRole::Agent => {
                self.config_editor.set_agent_desktop(geometry);
                Ok(())
            }
        }
    }

    fn save_config(&mut self) {
        let Some(local_node) = self.identity.as_ref().map(LocalIdentity::node) else {
            self.notice = Some(Notice::error("Local identity is not ready"));
            return;
        };
        let config = match self.config_editor.build(local_node.clone()) {
            Ok(config) => config,
            Err(error) => {
                tracing::warn!(error, "configuration editor is invalid");
                self.notice = Some(Notice::error(error));
                return;
            }
        };
        let path = PathBuf::from(self.config_path_input.trim());
        if path.as_os_str().is_empty() {
            self.notice = Some(Notice::error("Configuration path is required"));
            return;
        }
        match config.save(&path) {
            Ok(()) => {
                self.config_summary = Some(config_summary(&config));
                self.saved_config = Some(config.clone());
                self.remember_config_path(&path);
                self.start_discovery();
                tracing::info!(path = %path.display(), "configuration saved");
                self.notice = Some(Notice::success("Configuration saved"));
                self.start_session(config);
            }
            Err(error) => {
                tracing::error!(path = %path.display(), error = %error, "configuration save failed");
                self.notice = Some(Notice::error(error.to_string()));
            }
        }
    }

    fn remember_config_path(&mut self, path: &Path) {
        self.settings.config_path = Some(path.to_path_buf());
        if let Err(error) = self.settings.save(&self.data_directory) {
            tracing::warn!(error = %error, "could not remember configuration path");
        }
    }

    fn start_session(&mut self, config: Config) {
        self.stop_session();
        if !self.report.is_available() {
            self.notice = Some(Notice::error(
                "Native desktop input prerequisites are unavailable",
            ));
            return;
        }
        let Some(identity) = self.identity.clone() else {
            self.notice = Some(Notice::error("Local identity is not ready"));
            return;
        };
        let Some(trust) = self.trust.clone() else {
            self.notice = Some(Notice::error("Trust store is not ready"));
            return;
        };
        let native_input = match self.prepare_native_input(runtime_role(&config.role)) {
            Ok(native_input) => native_input,
            Err(error) => {
                self.notice = Some(Notice::error(error));
                return;
            }
        };
        match SessionRuntime::start(config, identity, trust, native_input) {
            Ok(runtime) => {
                self.session_state = SessionState::default();
                self.session_runtime = Some(runtime);
                self.notice = Some(Notice::info("Session starting"));
            }
            Err(error) => {
                tracing::warn!(error = %error, "session start rejected");
                self.session_state.record_error(error.to_string());
                self.notice = Some(Notice::error(error.to_string()));
            }
        }
    }

    fn stop_session(&mut self) {
        if let Some(runtime) = self.session_runtime.take() {
            runtime.stop();
        }
        self.session_state.stop();
    }

    fn prepare_native_input(&mut self, role: RuntimeRole) -> Result<NativeInputHost, String> {
        if let Some(native_input) = self
            .native_input
            .as_ref()
            .filter(|native_input| native_input.role() == role && !native_input.is_finished())
        {
            return Ok(native_input.clone());
        }
        if !self.report.is_available() {
            return Err(String::from(
                "Native desktop input prerequisites are unavailable",
            ));
        }

        self.native_input.take();
        let native_input = NativeInputHost::start(role).map_err(|error| error.to_string())?;
        tracing::info!(?role, "process native input service started");
        self.native_input = Some(native_input.clone());
        self.native_retry_at = None;
        Ok(native_input)
    }

    fn poll_native_input(&mut self) {
        let Some(native_input) = self.native_input.as_ref() else {
            return;
        };
        if self.session_runtime.is_some()
            || !native_input.is_finished()
            || !native_input.should_restart_after_close()
            || self
                .native_retry_at
                .is_some_and(|retry_at| retry_at > Instant::now())
        {
            return;
        }

        let role = native_input.role();
        match NativeInputHost::start(role) {
            Ok(native_input) => {
                tracing::warn!(?role, "restarted closed native input service");
                self.native_input = Some(native_input);
                self.native_retry_at = None;
                let config = self
                    .saved_config
                    .as_ref()
                    .filter(|config| runtime_role(&config.role) == role)
                    .cloned();
                if let Some(config) = config {
                    self.start_session(config);
                }
            }
            Err(error) => {
                tracing::error!(?role, error = %error, "native input service restart failed");
                self.native_retry_at = Some(Instant::now() + Duration::from_secs(2));
                self.notice = Some(Notice::error(error.to_string()));
            }
        }
    }

    fn poll_session(&mut self) {
        let mut stopped = false;
        let pending = self
            .session_runtime
            .as_ref()
            .map_or_else(Vec::new, |runtime| {
                std::iter::from_fn(|| runtime.try_recv().ok()).collect()
            });
        for event in pending {
            match &event {
                RuntimeEvent::Connected { peer, .. } => {
                    self.notice = Some(Notice::success(format!("Connected to {peer}")));
                }
                RuntimeEvent::AgentControl {
                    controller,
                    active: true,
                } => {
                    self.notice = Some(Notice::info(format!("Controlled by {controller}")));
                }
                RuntimeEvent::FocusChanged { node }
                    if self.session_state.connected.contains_key(node) =>
                {
                    self.notice = Some(Notice::info(format!("Controlling {node}")));
                }
                RuntimeEvent::LocalDesktopChanged { geometry } => {
                    self.local_desktop = Some(*geometry);
                    let _ = self.apply_local_desktop_to_editor(*geometry);
                }
                RuntimeEvent::DisplayChanged {
                    screen,
                    monitor_count,
                } => {
                    self.config_editor.update_screen(screen, *monitor_count);
                    self.notice = Some(Notice::info(format!(
                        "{} desktop updated to {} x {} across {} display{}",
                        screen.node,
                        screen.size.width,
                        screen.size.height,
                        monitor_count,
                        if *monitor_count == 1 { "" } else { "s" }
                    )));
                }
                RuntimeEvent::Error { message } => {
                    self.notice = Some(Notice::error(message));
                }
                RuntimeEvent::Starting { .. }
                | RuntimeEvent::Listening { .. }
                | RuntimeEvent::Connecting { .. }
                | RuntimeEvent::Disconnected { .. }
                | RuntimeEvent::NativeReady { .. }
                | RuntimeEvent::FocusChanged { .. }
                | RuntimeEvent::AgentControl { .. }
                | RuntimeEvent::Stopped => {}
            }
            stopped |= matches!(event, RuntimeEvent::Stopped);
            self.session_state.apply(event);
        }
        if stopped && let Some(runtime) = self.session_runtime.take() {
            runtime.stop();
        }
    }

    fn setup_view(&mut self, root: &mut Ui) {
        egui::CentralPanel::default()
            .frame(Frame::new().fill(CANVAS))
            .show(root, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space((ui.available_height() - 260.0).max(36.0) * 0.42);
                    ui.label(RichText::new("TEVIR").size(30.0).strong().color(TEXT));
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new("Create local identity")
                            .size(18.0)
                            .color(MUTED),
                    );
                    ui.add_space(28.0);
                    ui.set_max_width(390.0);
                    ui.add(
                        singleline_text(&mut self.node_input)
                            .hint_text("node-id")
                            .desired_width(390.0),
                    );
                    ui.add_space(10.0);
                    if ui
                        .add_sized([390.0, 36.0], Button::new("Create identity"))
                        .clicked()
                    {
                        self.create_identity();
                    }
                    if let Some(notice) = self.notice.as_ref() {
                        ui.add_space(12.0);
                        notice.show(ui);
                    }
                });
            });
    }

    fn navigation(&mut self, root: &mut Ui) {
        egui::Panel::left("navigation")
            .exact_size(184.0)
            .frame(
                Frame::new()
                    .fill(PANEL)
                    .inner_margin(Margin::symmetric(14, 18))
                    .stroke(Stroke::new(1.0, BORDER)),
            )
            .show(root, |ui| {
                ui.label(RichText::new("TEVIR").size(23.0).strong().color(TEXT));
                if let Some(identity) = self.identity.as_ref() {
                    ui.label(RichText::new(identity.node().as_str()).color(MUTED));
                }
                ui.add_space(28.0);

                for (page, label) in Page::ALL {
                    let selected = self.page == page;
                    let text = RichText::new(label).color(if selected { TEXT } else { MUTED });
                    let response = ui.add_sized(
                        [156.0, 38.0],
                        Button::new(text)
                            .selected(selected)
                            .corner_radius(CornerRadius::same(4)),
                    );
                    if response.clicked() {
                        self.page = page;
                        self.notice = None;
                    }
                    ui.add_space(4.0);
                }

                ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
                    ui.label(RichText::new(format!("v{}", env!("CARGO_PKG_VERSION"))).color(MUTED));
                });
            });
    }

    fn top_bar(&self, root: &mut Ui) {
        egui::Panel::top("top_bar")
            .exact_size(54.0)
            .frame(
                Frame::new()
                    .fill(CANVAS)
                    .inner_margin(Margin::symmetric(24, 10))
                    .stroke(Stroke::new(1.0, BORDER)),
            )
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    ui.heading(match self.page {
                        Page::Status => "Status",
                        Page::Configuration => "Configuration",
                        Page::Pairing => "Pairing",
                        Page::Diagnostics => "Diagnostics",
                        Page::Logs => "Logs",
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let ready = self.session_state.is_ready();
                        status_label(
                            ui,
                            if ready { "Ready" } else { "Setup required" },
                            if ready { SUCCESS } else { WARNING },
                        );
                    });
                });
            });
    }

    fn content(&mut self, root: &mut Ui) {
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(CANVAS)
                    .inner_margin(Margin::symmetric(28, 22)),
            )
            .show(root, |ui| {
                if let Some(notice) = self.notice.as_ref() {
                    notice.show(ui);
                    ui.add_space(16.0);
                }
                ScrollArea::vertical().show(ui, |ui| match self.page {
                    Page::Status => self.status_view(ui),
                    Page::Configuration => self.configuration_view(ui),
                    Page::Pairing => self.pairing_view(ui),
                    Page::Diagnostics => self.diagnostics_view(ui),
                    Page::Logs => self.logs_view(ui),
                });
            });
    }

    fn status_view(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            section_heading(ui, "Live session", self.session_state.role_label());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if self.session_runtime.is_some() {
                    if ui
                        .add_sized([112.0, 34.0], Button::new("Stop session"))
                        .clicked()
                    {
                        self.stop_session();
                        self.notice = Some(Notice::info("Session stopped"));
                    }
                } else if ui
                    .add_enabled(
                        self.saved_config.is_some(),
                        Button::new("Start session").min_size(Vec2::new(112.0, 34.0)),
                    )
                    .clicked()
                    && let Some(config) = self.saved_config.clone()
                {
                    self.start_session(config);
                }
            });
        });
        ui.add_space(14.0);
        let (state_label, state_color) = self.session_state.status();
        metric_row(ui, "State", state_label, state_color);
        metric_row(
            ui,
            "Native input",
            if self.session_state.native_ready {
                "Ready"
            } else {
                "Waiting"
            },
            if self.session_state.native_ready {
                SUCCESS
            } else {
                WARNING
            },
        );
        match self.session_state.role {
            Some(RuntimeRole::Controller) => {
                metric_row(
                    ui,
                    "Connected agents",
                    &self.session_state.connected.len().to_string(),
                    if self.session_state.connected.is_empty() {
                        WARNING
                    } else {
                        SUCCESS
                    },
                );
                metric_row(
                    ui,
                    "Control target",
                    if self.session_state.is_controlling_remote() {
                        self.session_state
                            .focus
                            .as_ref()
                            .map_or("Local desktop", NodeId::as_str)
                    } else {
                        "Local desktop"
                    },
                    if self.session_state.is_controlling_remote() {
                        ACCENT
                    } else {
                        TEXT
                    },
                );
                for peer in self.session_state.connected.keys() {
                    metric_row(ui, "Agent", peer.as_str(), SUCCESS);
                    if let Some(display) = self.session_state.displays.get(peer) {
                        metric_row(
                            ui,
                            &format!("{peer} desktop"),
                            &format!(
                                "{} x {} | {} display{}",
                                display.size.width,
                                display.size.height,
                                display.monitor_count,
                                if display.monitor_count == 1 { "" } else { "s" }
                            ),
                            TEXT,
                        );
                    }
                }
            }
            Some(RuntimeRole::Agent) => {
                let controlled = self.session_state.agent_controlled;
                metric_row(
                    ui,
                    "Controller",
                    self.session_state
                        .connected
                        .keys()
                        .next()
                        .map_or("Not connected", NodeId::as_str),
                    if self.session_state.connected.is_empty() {
                        WARNING
                    } else {
                        SUCCESS
                    },
                );
                metric_row(
                    ui,
                    "Remote input",
                    if controlled {
                        "Being controlled"
                    } else if self.session_state.connected.is_empty() {
                        "Disconnected"
                    } else {
                        "Connected, idle"
                    },
                    if controlled {
                        ACCENT
                    } else if self.session_state.connected.is_empty() {
                        WARNING
                    } else {
                        SUCCESS
                    },
                );
            }
            None => {}
        }
        if let Some(error) = self.session_state.last_error.as_ref() {
            ui.add_space(8.0);
            status_label(ui, error, DANGER);
        }

        ui.add_space(30.0);
        section_heading(
            ui,
            "Session readiness",
            "Local prerequisites and trusted nodes",
        );
        ui.add_space(14.0);
        let platform_ready = self.report.is_available();
        metric_row(
            ui,
            "Desktop input",
            if platform_ready {
                "Available"
            } else {
                "Unavailable"
            },
            if platform_ready { SUCCESS } else { DANGER },
        );
        metric_row(
            ui,
            "Trusted nodes",
            &self.peer_count().to_string(),
            if self.peer_count() > 0 {
                SUCCESS
            } else {
                WARNING
            },
        );
        metric_row(
            ui,
            "Nearby nodes",
            &self.nearby.len().to_string(),
            if self.discovery.is_some() {
                ACCENT
            } else {
                DANGER
            },
        );
        metric_row(
            ui,
            "Secure transport",
            if self.session_state.connected.is_empty() {
                "Disconnected"
            } else {
                "Authenticated"
            },
            if self.session_state.connected.is_empty() {
                WARNING
            } else {
                SUCCESS
            },
        );

        ui.add_space(30.0);
        section_heading(ui, "Configuration", "Controller or agent");
        ui.add_space(14.0);
        if let Some(summary) = self.config_summary.as_ref() {
            ui.label(RichText::new(summary).color(SUCCESS));
            ui.add_space(10.0);
        } else {
            ui.label(RichText::new("Not saved").color(WARNING));
            ui.add_space(10.0);
        }
        if ui
            .add_sized([156.0, 34.0], Button::new("Edit configuration"))
            .clicked()
        {
            self.page = Page::Configuration;
            self.notice = None;
        }
    }

    fn configuration_view(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            section_heading(ui, "Session configuration", "Validated TOML");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add_sized([96.0, 34.0], Button::new("Save")).clicked() {
                    self.save_config();
                }
            });
        });
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            let available = (ui.available_width() - 110.0).max(160.0);
            ui.add_sized(
                [available, 34.0],
                singleline_text(&mut self.config_path_input).hint_text("Configuration path"),
            );
            if ui.add_sized([96.0, 34.0], Button::new("Load")).clicked() {
                self.load_config();
            }
        });

        ui.add_space(26.0);
        ui.label(RichText::new("Role").color(MUTED));
        ui.add_space(6.0);
        let previous_role = self.config_editor.role;
        ui.horizontal(|ui| {
            ui.selectable_value(
                &mut self.config_editor.role,
                ConfigRole::Controller,
                "Controller",
            );
            ui.selectable_value(&mut self.config_editor.role, ConfigRole::Agent, "Agent");
        });
        if previous_role != self.config_editor.role {
            self.stop_session();
            if let Some(geometry) = self.local_desktop {
                let _ = self.apply_local_desktop_to_editor(geometry);
            }
            if let Err(error) = self.prepare_native_input(self.config_editor.role.runtime_role()) {
                tracing::warn!(%error, "native input role change failed");
                self.notice = Some(Notice::error(error));
            }
        }

        ui.add_space(22.0);
        match self.config_editor.role {
            ConfigRole::Controller => self.controller_configuration(ui),
            ConfigRole::Agent => self.agent_configuration(ui),
        }
    }

    fn agent_configuration(&mut self, ui: &mut Ui) {
        let trusted_nodes = self
            .trust
            .as_ref()
            .map(|trust| {
                trust
                    .peers()
                    .map(|peer| peer.node().to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if self.config_editor.controller_node == "peer-node" && trusted_nodes.len() == 1 {
            self.config_editor.controller_node = trusted_nodes[0].clone();
        }

        section_heading(ui, "Controller endpoint", "Trusted node and address");
        ui.add_space(10.0);
        ui.label(RichText::new("Controller node").color(MUTED));
        let previous_node = self.config_editor.controller_node.clone();
        ComboBox::from_id_salt("agent-controller-node")
            .selected_text(&self.config_editor.controller_node)
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                for node in &trusted_nodes {
                    ui.selectable_value(
                        &mut self.config_editor.controller_node,
                        node.clone(),
                        node,
                    );
                }
            });
        if trusted_nodes.is_empty() {
            status_label(ui, "No trusted controller", WARNING);
        }
        if previous_node != self.config_editor.controller_node {
            self.use_discovered_controller_address();
        }
        ui.add_space(8.0);
        labeled_text_field(
            ui,
            "Controller address",
            &mut self.config_editor.controller_address,
            "192.0.2.10:24800",
        );
        if self.discovered_controller_address().is_some()
            && ui.button("Use discovered address").clicked()
        {
            self.use_discovered_controller_address();
        }

        ui.add_space(26.0);
        section_heading(
            ui,
            "Local desktop",
            &format!(
                "{} connected display{}",
                self.config_editor.agent_monitor_count,
                if self.config_editor.agent_monitor_count == 1 {
                    ""
                } else {
                    "s"
                }
            ),
        );
        ui.add_space(10.0);
        ui.columns(2, |columns| {
            compact_text_field(
                &mut columns[0],
                "Width",
                &mut self.config_editor.agent_width,
                "1920",
            );
            compact_text_field(
                &mut columns[1],
                "Height",
                &mut self.config_editor.agent_height,
                "1080",
            );
        });
    }

    fn discovered_controller_address(&self) -> Option<SocketAddr> {
        let discovered = self
            .nearby
            .iter()
            .find(|node| node.node().as_str() == self.config_editor.controller_node)?;
        let port = discovered.session_port();
        if port == 0 {
            return None;
        }
        let address = discovered
            .addresses()
            .iter()
            .find(|address| address.is_ipv4())
            .or_else(|| discovered.addresses().iter().next())?;
        Some(SocketAddr::new(*address, port))
    }

    fn use_discovered_controller_address(&mut self) {
        if let Some(address) = self.discovered_controller_address() {
            self.config_editor.controller_address = address.to_string();
            tracing::info!(
                controller = %self.config_editor.controller_node,
                %address,
                "controller address filled from discovery"
            );
        }
    }

    fn controller_configuration(&mut self, ui: &mut Ui) {
        section_heading(ui, "Listen endpoint", "IP address and port");
        ui.add_space(10.0);
        labeled_text_field(
            ui,
            "Listen address",
            &mut self.config_editor.listen_address,
            "0.0.0.0:24800",
        );

        ui.add_space(26.0);
        let mut add_screen = false;
        ui.horizontal(|ui| {
            section_heading(
                ui,
                "Screen topology",
                &format!(
                    "{} machine{}",
                    self.config_editor.screens.len(),
                    if self.config_editor.screens.len() == 1 {
                        ""
                    } else {
                        "s"
                    }
                ),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add_enabled(
                        self.config_editor.screens.len()
                            < usize::from(TOPOLOGY_COLUMNS) * usize::from(TOPOLOGY_ROWS),
                        Button::new("Add machine"),
                    )
                    .clicked()
                {
                    add_screen = true;
                }
            });
        });
        ui.add_space(10.0);

        let suggested_node = self
            .trust
            .as_ref()
            .and_then(|trust| {
                trust
                    .peers()
                    .find(|peer| !self.config_editor.contains_node(peer.node()))
            })
            .map(|peer| peer.node().to_string())
            .unwrap_or_else(|| String::from("peer-node"));
        if add_screen {
            self.config_editor.add_screen(suggested_node);
        }
        let local_node = self.identity.as_ref().map(LocalIdentity::node).cloned();
        topology_grid(ui, &mut self.config_editor, local_node.as_ref());
        ui.add_space(12.0);

        let selected = self
            .config_editor
            .selected_screen
            .min(self.config_editor.screens.len().saturating_sub(1));
        self.config_editor.selected_screen = selected;
        let can_remove = self.config_editor.can_remove_screen(selected)
            && local_node.as_ref().is_none_or(|local| {
                self.config_editor.screens[selected].node.trim() != local.as_str()
            });
        let mut remove_selected = false;
        Frame::new()
            .fill(ELEVATED)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(5))
            .inner_margin(Margin::symmetric(14, 12))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Selected machine").strong().color(TEXT));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.add_enabled(can_remove, Button::new("Remove")).clicked() {
                            remove_selected = true;
                        }
                    });
                });
                ui.add_space(8.0);
                let screen = &mut self.config_editor.screens[selected];
                labeled_text_field(ui, "Node", &mut screen.node, "node-id");
                ui.add_space(8.0);
                ui.columns(2, |columns| {
                    compact_text_field(&mut columns[0], "Width", &mut screen.width, "1920");
                    compact_text_field(&mut columns[1], "Height", &mut screen.height, "1080");
                });
                ui.add_space(6.0);
                ui.label(
                    RichText::new(format!(
                        "Grid {}, {}  |  {} display{}",
                        screen.slot.column + 1,
                        screen.slot.row + 1,
                        screen.monitor_count,
                        if screen.monitor_count == 1 { "" } else { "s" }
                    ))
                    .small()
                    .color(MUTED),
                );
            });
        if remove_selected {
            self.config_editor.remove_selected();
        }
    }

    fn pairing_view(&mut self, ui: &mut Ui) {
        let Some(identity) = self.identity.as_ref() else {
            return;
        };
        let bundle = identity.pairing_bundle();
        let encoded = bundle.encode();
        let code = bundle.code().to_string();

        section_heading(ui, "This node", identity.node().as_str());
        ui.add_space(12.0);
        ui.label(RichText::new("Verification code").color(MUTED));
        ui.label(
            RichText::new(&code)
                .family(FontFamily::Monospace)
                .size(20.0)
                .color(TEXT),
        );
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui.button("Copy bundle").clicked() {
                ui.ctx().copy_text(encoded.clone());
                self.notice = Some(Notice::success("Pairing bundle copied"));
            }
            if ui.button("Copy code").clicked() {
                ui.ctx().copy_text(code.clone());
                self.notice = Some(Notice::success("Verification code copied"));
            }
        });

        ui.add_space(30.0);
        section_heading(ui, "Nearby nodes", &format!("{} found", self.nearby.len()));
        ui.add_space(10.0);
        if let Some(error) = self.discovery_error.as_ref() {
            status_label(ui, error, DANGER);
            ui.add_space(8.0);
        }
        let nearby = self.nearby.iter().cloned().collect::<Vec<_>>();
        if nearby.is_empty() {
            empty_state(
                ui,
                if self.discovery.is_some() {
                    "Searching the local network"
                } else {
                    "Local network discovery unavailable"
                },
            );
        }
        for node in nearby {
            let paired = self
                .trust
                .as_ref()
                .is_some_and(|trust| trust.peers().any(|peer| peer.node() == node.node()));
            let mut selected = false;
            Frame::new()
                .fill(ELEVATED)
                .stroke(Stroke::new(1.0, BORDER))
                .corner_radius(CornerRadius::same(5))
                .inner_margin(Margin::symmetric(14, 12))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(RichText::new(node.node().as_str()).strong().color(TEXT));
                            ui.label(
                                RichText::new(format_discovered_node(&node))
                                    .family(FontFamily::Monospace)
                                    .color(MUTED),
                            );
                            ui.label(
                                RichText::new(format_fingerprint(node.fingerprint()))
                                    .family(FontFamily::Monospace)
                                    .color(MUTED),
                            );
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui
                                .add_enabled(
                                    !paired,
                                    Button::new(if paired { "Paired" } else { "Pair" }),
                                )
                                .clicked()
                            {
                                selected = true;
                            }
                        });
                    });
                });
            if selected {
                self.select_discovered(&node);
            }
            ui.add_space(8.0);
        }

        ui.add_space(22.0);
        section_heading(
            ui,
            "Add trusted node",
            "Pairing bundle and verification code",
        );
        ui.add_space(12.0);
        ui.add_sized(
            [ui.available_width(), 82.0],
            TextEdit::multiline(&mut self.pairing_bundle_input)
                .hint_text("Pairing bundle")
                .font(TextStyle::Monospace),
        );
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let available = (ui.available_width() - 112.0).max(160.0);
            ui.add_sized(
                [available, 34.0],
                singleline_text(&mut self.pairing_code_input)
                    .hint_text("Verification code")
                    .font(TextStyle::Monospace),
            );
            let enabled = !self.pairing_bundle_input.trim().is_empty()
                && !self.pairing_code_input.trim().is_empty();
            if ui
                .add_enabled_ui(enabled, |ui| {
                    ui.add_sized([98.0, 34.0], Button::new("Trust node"))
                })
                .inner
                .clicked()
            {
                self.import_pairing();
            }
        });

        ui.add_space(30.0);
        section_heading(
            ui,
            "Trusted nodes",
            &format!("{} stored on this node", self.peer_count()),
        );
        ui.add_space(10.0);
        let peers = self
            .trust
            .as_ref()
            .map(|trust| {
                trust
                    .peers()
                    .map(|peer| (peer.node().clone(), format_fingerprint(peer.fingerprint())))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if peers.is_empty() {
            empty_state(ui, "No trusted nodes");
        }
        for (node, fingerprint) in peers {
            let mut remove = false;
            Frame::new()
                .fill(ELEVATED)
                .stroke(Stroke::new(1.0, BORDER))
                .corner_radius(CornerRadius::same(5))
                .inner_margin(Margin::symmetric(14, 12))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(RichText::new(node.as_str()).strong().color(TEXT));
                            ui.label(
                                RichText::new(&fingerprint)
                                    .family(FontFamily::Monospace)
                                    .color(MUTED),
                            );
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if self.confirm_remove.as_ref() == Some(&node) {
                                if ui.button("Confirm").clicked() {
                                    remove = true;
                                }
                                if ui.button("Cancel").clicked() {
                                    self.confirm_remove = None;
                                }
                            } else if ui.button("Remove").clicked() {
                                self.confirm_remove = Some(node.clone());
                            }
                        });
                    });
                });
            if remove {
                self.remove_peer(&node);
            }
            ui.add_space(8.0);
        }
    }

    fn diagnostics_view(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            section_heading(ui, "Desktop environment", "Native input prerequisites");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Refresh").clicked() {
                    self.report = platform::probe_host();
                    tracing::info!(
                        available = self.report.is_available(),
                        issues = self.report.issues.len(),
                        "platform diagnostics refreshed"
                    );
                    self.notice = Some(Notice::success("Diagnostics refreshed"));
                }
            });
        });
        ui.add_space(16.0);
        metric_row(
            ui,
            "Platform",
            match self.report.platform {
                domain::HostPlatform::LinuxWayland => "Linux Wayland",
                domain::HostPlatform::Windows => "Windows",
            },
            ACCENT,
        );
        metric_row(
            ui,
            "Environment",
            match self.report.status {
                EnvironmentStatus::Available => "Available",
                EnvironmentStatus::Unavailable => "Unavailable",
            },
            if self.report.is_available() {
                SUCCESS
            } else {
                DANGER
            },
        );
        ui.add_space(24.0);
        section_heading(
            ui,
            "Issues",
            &format!("{} detected", self.report.issues.len()),
        );
        ui.add_space(10.0);
        if self.report.issues.is_empty() {
            empty_state(ui, "No issues detected");
        }
        for issue in &self.report.issues {
            Frame::new()
                .fill(ELEVATED)
                .stroke(Stroke::new(1.0, BORDER))
                .corner_radius(CornerRadius::same(5))
                .inner_margin(Margin::symmetric(14, 12))
                .show(ui, |ui| {
                    status_label(ui, &issue.to_string(), DANGER);
                });
            ui.add_space(8.0);
        }
    }

    fn logs_view(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            section_heading(ui, "Application events", "Current process");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Clear").clicked() {
                    self.logs.clear();
                }
            });
        });
        ui.add_space(12.0);

        let entries = self.logs.snapshot();
        if entries.is_empty() {
            empty_state(ui, "No events recorded");
            return;
        }
        for entry in entries {
            log_row(ui, &entry);
        }
    }

    fn peer_count(&self) -> usize {
        self.trust.as_ref().map_or(0, |trust| trust.peers().len())
    }
}

impl eframe::App for DesktopApp {
    fn ui(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        self.sync_desktop_geometry(frame, ui.ctx());
        self.start_deferred_session();
        self.poll_discovery();
        self.poll_session();
        self.poll_native_input();
        if self.identity.is_none() {
            self.setup_view(ui);
            return;
        }
        self.navigation(ui);
        self.top_bar(ui);
        self.content(ui);
        ui.ctx()
            .request_repaint_after(if self.session_runtime.is_some() {
                Duration::from_millis(50)
            } else if self.page == Page::Logs {
                Duration::from_millis(500)
            } else {
                Duration::from_secs(1)
            });
    }
}

pub fn run(data_directory: PathBuf, node: Option<NodeId>, logs: LogBuffer) -> Result<(), AppError> {
    let mut app = DesktopApp::load(data_directory, node, logs)?;
    if app.native_input.is_none()
        && let Err(error) = app.prepare_native_input(app.config_editor.role.runtime_role())
    {
        tracing::warn!(%error, "native input startup failed");
        app.notice = Some(Notice::error(error));
    }
    let options = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_title("Tevir")
            .with_app_id("tevir")
            .with_inner_size([1080.0, 760.0])
            .with_min_inner_size([760.0, 520.0]),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "Tevir",
        options,
        Box::new(move |creation| {
            configure_style(&creation.egui_ctx);
            Ok(Box::new(app))
        }),
    )
    .map_err(|error| AppError::Desktop(error.to_string()))
}

const fn runtime_role(role: &Role) -> RuntimeRole {
    match role {
        Role::Controller { .. } => RuntimeRole::Controller,
        Role::Agent { .. } => RuntimeRole::Agent,
    }
}

fn load_identity(
    data_directory: &Path,
    node: &NodeId,
) -> Result<(LocalIdentity, TrustStore), String> {
    let store = IdentityStore::new(data_directory);
    let identity = store
        .load_or_create(node)
        .map_err(|error| error.to_string())?;
    let trust = store.trust_store().map_err(|error| error.to_string())?;
    Ok((identity, trust))
}

fn configure_style(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = CANVAS;
    visuals.window_fill = PANEL;
    visuals.extreme_bg_color = PANEL;
    visuals.faint_bg_color = ELEVATED;
    visuals.selection.bg_fill = ACCENT_MUTED;
    visuals.selection.stroke = Stroke::new(1.0, ACCENT);
    visuals.widgets.inactive.bg_fill = ELEVATED;
    visuals.widgets.inactive.weak_bg_fill = ELEVATED;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(46, 51, 50);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT);
    visuals.widgets.active.bg_fill = ACCENT_MUTED;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT);
    visuals.window_corner_radius = CornerRadius::same(6);
    visuals.menu_corner_radius = CornerRadius::same(6);
    ctx.set_visuals(visuals);
    ctx.global_style_mut(|style| {
        style.spacing.item_spacing = Vec2::new(8.0, 8.0);
        style.spacing.button_padding = Vec2::new(12.0, 7.0);
        style.visuals.widgets.inactive.corner_radius = CornerRadius::same(4);
        style.visuals.widgets.hovered.corner_radius = CornerRadius::same(4);
        style.visuals.widgets.active.corner_radius = CornerRadius::same(4);
        style.text_styles.insert(
            TextStyle::Heading,
            FontId::new(20.0, FontFamily::Proportional),
        );
        style
            .text_styles
            .insert(TextStyle::Body, FontId::new(14.0, FontFamily::Proportional));
        style.text_styles.insert(
            TextStyle::Button,
            FontId::new(14.0, FontFamily::Proportional),
        );
        style.text_styles.insert(
            TextStyle::Monospace,
            FontId::new(13.0, FontFamily::Monospace),
        );
    });
}

fn singleline_text(text: &mut String) -> TextEdit<'_> {
    TextEdit::singleline(text)
        .vertical_align(Align::Center)
        .margin(Margin::symmetric(8, 6))
}

fn labeled_text_field(ui: &mut Ui, label: &str, text: &mut String, hint: &str) {
    ui.label(RichText::new(label).color(MUTED));
    ui.add_sized(
        [ui.available_width(), 34.0],
        singleline_text(text).hint_text(hint),
    );
}

fn compact_text_field(ui: &mut Ui, label: &str, text: &mut String, hint: &str) {
    ui.label(RichText::new(label).color(MUTED));
    ui.add_sized(
        [ui.available_width(), 34.0],
        singleline_text(text).hint_text(hint),
    );
}

fn section_heading(ui: &mut Ui, title: &str, detail: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).size(17.0).strong().color(TEXT));
        ui.label(RichText::new(detail).color(MUTED));
    });
}

fn metric_row(ui: &mut Ui, label: &str, value: &str, color: Color32) {
    let response = ui.allocate_response(Vec2::new(ui.available_width(), 42.0), Sense::hover());
    let painter = ui.painter_at(response.rect);
    painter.line_segment(
        [response.rect.left_bottom(), response.rect.right_bottom()],
        Stroke::new(1.0, BORDER),
    );
    painter.text(
        response.rect.left_center(),
        egui::Align2::LEFT_CENTER,
        label,
        FontId::new(14.0, FontFamily::Proportional),
        MUTED,
    );
    painter.circle_filled(
        egui::pos2(response.rect.right() - 110.0, response.rect.center().y),
        4.0,
        color,
    );
    painter.text(
        response.rect.right_center(),
        egui::Align2::RIGHT_CENTER,
        value,
        FontId::new(14.0, FontFamily::Proportional),
        TEXT,
    );
}

fn status_label(ui: &mut Ui, label: &str, color: Color32) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
        ui.painter().circle_filled(rect.center(), 4.0, color);
        ui.label(RichText::new(label).color(TEXT));
    });
}

fn empty_state(ui: &mut Ui, label: &str) {
    Frame::new()
        .fill(ELEVATED)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(5))
        .inner_margin(Margin::symmetric(14, 16))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label(RichText::new(label).color(MUTED));
        });
}

fn log_row(ui: &mut Ui, entry: &LogEntry) {
    let color = match entry.level {
        LogLevel::Trace | LogLevel::Debug => MUTED,
        LogLevel::Info => ACCENT,
        LogLevel::Warn => WARNING,
        LogLevel::Error => DANGER,
    };
    ui.horizontal_top(|ui| {
        ui.add_sized(
            [82.0, 20.0],
            egui::Label::new(
                RichText::new(format_elapsed(entry.elapsed_millis))
                    .family(FontFamily::Monospace)
                    .color(MUTED),
            ),
        );
        ui.add_sized(
            [48.0, 20.0],
            egui::Label::new(
                RichText::new(entry.level.as_str())
                    .family(FontFamily::Monospace)
                    .color(color),
            ),
        );
        ui.add_sized(
            [96.0, 20.0],
            egui::Label::new(
                RichText::new(component_target(&entry.target))
                    .family(FontFamily::Monospace)
                    .color(MUTED),
            ),
        );
        ui.vertical(|ui| {
            ui.set_width(ui.available_width());
            ui.add(
                egui::Label::new(RichText::new(&entry.message).color(TEXT))
                    .halign(Align::LEFT)
                    .wrap(),
            );
        });
    });
    ui.separator();
}

fn format_elapsed(elapsed_millis: u128) -> String {
    let minutes = elapsed_millis / 60_000;
    let seconds = (elapsed_millis / 1_000) % 60;
    let millis = elapsed_millis % 1_000;
    format!("+{minutes:02}:{seconds:02}.{millis:03}")
}

fn component_target(target: &str) -> &str {
    target.split("::").next().unwrap_or(target)
}

fn format_fingerprint(fingerprint: [u8; 32]) -> String {
    fingerprint[..12]
        .chunks_exact(2)
        .map(|chunk| format!("{:02X}{:02X}", chunk[0], chunk[1]))
        .collect::<Vec<_>>()
        .join("-")
}

fn format_discovered_node(node: &DiscoveredNode) -> String {
    let platform = match node.platform() {
        domain::HostPlatform::LinuxWayland => "Linux Wayland",
        domain::HostPlatform::Windows => "Windows",
    };
    let addresses = node
        .addresses()
        .iter()
        .map(|address| {
            if node.session_port() == 0 {
                address.to_string()
            } else {
                SocketAddr::new(*address, node.session_port()).to_string()
            }
        })
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        format!("{platform} | address pending")
    } else {
        format!("{platform} | {}", addresses.join(", "))
    }
}

const fn advertised_capabilities() -> Capabilities {
    Capabilities {
        keyboard: true,
        relative_pointer: true,
        absolute_pointer: false,
        clipboard_text: false,
    }
}

#[cfg(feature = "screenshot-tests")]
fn initial_page() -> Page {
    match std::env::var("TEVIR_SCREENSHOT_PAGE").as_deref() {
        Ok("configuration") => Page::Configuration,
        Ok("pairing") => Page::Pairing,
        Ok("diagnostics") => Page::Diagnostics,
        Ok("logs") => Page::Logs,
        _ => Page::Status,
    }
}

#[cfg(not(feature = "screenshot-tests"))]
const fn initial_page() -> Page {
    Page::Status
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum SessionPhase {
    #[default]
    Stopped,
    Starting,
    Listening,
    Connecting,
    Connected,
    Failed,
}

#[derive(Default)]
struct SessionState {
    phase: SessionPhase,
    role: Option<RuntimeRole>,
    connected: BTreeMap<NodeId, u128>,
    displays: BTreeMap<NodeId, RemoteDisplay>,
    focus: Option<NodeId>,
    agent_controlled: bool,
    native_ready: bool,
    last_error: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RemoteDisplay {
    size: Size,
    monitor_count: u32,
}

impl SessionState {
    fn apply(&mut self, event: RuntimeEvent) {
        match event {
            RuntimeEvent::Starting { role } => {
                self.phase = SessionPhase::Starting;
                self.role = Some(role);
                self.connected.clear();
                self.displays.clear();
                self.focus = None;
                self.agent_controlled = false;
                self.native_ready = false;
                self.last_error = None;
            }
            RuntimeEvent::Listening { address } => {
                tracing::info!(%address, "session listening");
                self.phase = SessionPhase::Listening;
            }
            RuntimeEvent::Connecting {
                peer,
                address,
                attempt,
            } => {
                tracing::info!(%peer, %address, attempt, "session connecting");
                self.phase = SessionPhase::Connecting;
            }
            RuntimeEvent::Connected { peer, session_id } => {
                self.connected.insert(peer, session_id);
                self.phase = SessionPhase::Connected;
                self.last_error = None;
            }
            RuntimeEvent::Disconnected { peer, reason } => {
                self.connected.remove(&peer);
                self.displays.remove(&peer);
                self.agent_controlled = false;
                self.last_error = Some(format!("{peer}: {reason}"));
                self.phase = match self.role {
                    Some(RuntimeRole::Controller) => SessionPhase::Listening,
                    Some(RuntimeRole::Agent) => SessionPhase::Connecting,
                    None => SessionPhase::Stopped,
                };
            }
            RuntimeEvent::NativeReady { backend } => {
                tracing::info!(?backend, "native session backend ready");
                self.native_ready = true;
            }
            RuntimeEvent::FocusChanged { node } => {
                self.focus = Some(node);
            }
            RuntimeEvent::AgentControl { controller, active } => {
                tracing::info!(%controller, active, "agent control state changed");
                self.agent_controlled = active;
            }
            RuntimeEvent::LocalDesktopChanged { .. } => {}
            RuntimeEvent::DisplayChanged {
                screen,
                monitor_count,
            } => {
                self.displays.insert(
                    screen.node,
                    RemoteDisplay {
                        size: screen.size,
                        monitor_count,
                    },
                );
            }
            RuntimeEvent::Error { message } => self.record_error(message),
            RuntimeEvent::Stopped => {
                self.connected.clear();
                self.displays.clear();
                self.agent_controlled = false;
                self.native_ready = false;
                self.phase = if self.last_error.is_some() {
                    SessionPhase::Failed
                } else {
                    SessionPhase::Stopped
                };
            }
        }
    }

    fn record_error(&mut self, message: String) {
        self.last_error = Some(message);
        if self.phase == SessionPhase::Stopped {
            self.phase = SessionPhase::Failed;
        }
    }

    fn stop(&mut self) {
        *self = Self::default();
    }

    const fn role_label(&self) -> &'static str {
        match self.role {
            Some(RuntimeRole::Controller) => "Controller",
            Some(RuntimeRole::Agent) => "Agent",
            None => "Not running",
        }
    }

    const fn status(&self) -> (&'static str, Color32) {
        match self.phase {
            SessionPhase::Stopped => ("Stopped", MUTED),
            SessionPhase::Starting => ("Starting", WARNING),
            SessionPhase::Listening => ("Listening", ACCENT),
            SessionPhase::Connecting => ("Connecting", WARNING),
            SessionPhase::Connected => ("Connected", SUCCESS),
            SessionPhase::Failed => ("Failed", DANGER),
        }
    }

    fn is_controlling_remote(&self) -> bool {
        self.focus
            .as_ref()
            .is_some_and(|focus| self.connected.contains_key(focus))
    }

    fn is_ready(&self) -> bool {
        self.native_ready && !self.connected.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfigRole {
    Controller,
    Agent,
}

impl ConfigRole {
    const fn runtime_role(self) -> RuntimeRole {
        match self {
            Self::Controller => RuntimeRole::Controller,
            Self::Agent => RuntimeRole::Agent,
        }
    }
}

struct ConfigEditor {
    role: ConfigRole,
    listen_address: String,
    controller_node: String,
    controller_address: String,
    agent_width: String,
    agent_height: String,
    agent_monitor_count: u32,
    screens: Vec<ScreenEditor>,
    selected_screen: usize,
    dragged_screen: Option<usize>,
}

impl ConfigEditor {
    fn for_node(node: Option<&NodeId>) -> Self {
        Self {
            role: ConfigRole::Controller,
            listen_address: String::from("0.0.0.0:24800"),
            controller_node: String::from("peer-node"),
            controller_address: String::from("127.0.0.1:24800"),
            agent_width: String::from("1920"),
            agent_height: String::from("1080"),
            agent_monitor_count: 1,
            screens: vec![ScreenEditor::from_node(
                node.map_or_else(|| String::from("local-node"), ToString::to_string),
                GridSlot::new(TOPOLOGY_COLUMNS / 2, TOPOLOGY_ROWS / 2),
            )],
            selected_screen: 0,
            dragged_screen: None,
        }
    }

    fn from_config(config: &Config) -> Self {
        match &config.role {
            Role::Controller { listen, topology } => Self {
                role: ConfigRole::Controller,
                listen_address: listen.to_string(),
                controller_node: String::from("peer-node"),
                controller_address: String::from("127.0.0.1:24800"),
                agent_width: String::from("1920"),
                agent_height: String::from("1080"),
                agent_monitor_count: 1,
                screens: topology
                    .screens()
                    .iter()
                    .map(ScreenEditor::from_placement)
                    .collect(),
                selected_screen: 0,
                dragged_screen: None,
            },
            Role::Agent {
                controller_node,
                controller,
                display_size,
            } => Self {
                role: ConfigRole::Agent,
                listen_address: String::from("0.0.0.0:24800"),
                controller_node: controller_node.to_string(),
                controller_address: controller.to_string(),
                agent_width: display_size.width.to_string(),
                agent_height: display_size.height.to_string(),
                agent_monitor_count: 1,
                screens: vec![ScreenEditor::from_local_node(&config.node)],
                selected_screen: 0,
                dragged_screen: None,
            },
        }
    }

    fn build(&self, node: NodeId) -> Result<Config, String> {
        let role = match self.role {
            ConfigRole::Controller => {
                let listen = parse_socket_address("Listen address", &self.listen_address)?;
                let screens = self
                    .screens
                    .iter()
                    .enumerate()
                    .map(|(index, screen)| screen.build(index))
                    .collect::<Result<Vec<_>, _>>()?;
                let topology = Topology::new(screens).map_err(|error| error.to_string())?;
                Role::Controller { listen, topology }
            }
            ConfigRole::Agent => {
                let controller_node = NodeId::new(self.controller_node.trim())
                    .map_err(|error| format!("Controller node: {error}"))?;
                Role::Agent {
                    controller_node,
                    controller: parse_socket_address(
                        "Controller address",
                        &self.controller_address,
                    )?,
                    display_size: Size::new(
                        parse_nonzero("Agent screen width", &self.agent_width)?,
                        parse_nonzero("Agent screen height", &self.agent_height)?,
                    ),
                }
            }
        };
        Config::new(node, role).map_err(|error| error.to_string())
    }

    fn contains_node(&self, node: &NodeId) -> bool {
        self.screens
            .iter()
            .any(|screen| screen.node.trim() == node.as_str())
    }

    fn discovery_port(&self) -> u16 {
        match self.role {
            ConfigRole::Controller => self
                .listen_address
                .trim()
                .parse::<SocketAddr>()
                .map_or(0, |address| address.port()),
            ConfigRole::Agent => 0,
        }
    }

    fn add_screen(&mut self, node: String) {
        let slot = self
            .screens
            .iter()
            .skip(self.selected_screen)
            .chain(self.screens.iter().take(self.selected_screen))
            .flat_map(|screen| {
                [Edge::Right, Edge::Bottom, Edge::Left, Edge::Top]
                    .into_iter()
                    .filter_map(move |edge| screen.slot.neighbor(edge))
            })
            .find(|slot| self.screen_index_at(*slot).is_none());
        let Some(slot) = slot else {
            return;
        };
        self.screens.push(ScreenEditor::from_node(node, slot));
        self.selected_screen = self.screens.len() - 1;
    }

    fn remove_selected(&mut self) {
        if !self.can_remove_screen(self.selected_screen) {
            return;
        }
        self.screens.remove(self.selected_screen);
        self.selected_screen = self
            .selected_screen
            .min(self.screens.len().saturating_sub(1));
        self.dragged_screen = None;
    }

    fn can_remove_screen(&self, index: usize) -> bool {
        if self.screens.len() <= 1 || index >= self.screens.len() {
            return false;
        }
        slots_are_connected(
            self.screens
                .iter()
                .enumerate()
                .filter_map(|(candidate, screen)| (candidate != index).then_some(screen.slot)),
        )
    }

    fn set_local_desktop(
        &mut self,
        local_node: &NodeId,
        geometry: platform::DesktopGeometry,
    ) -> Result<(), String> {
        let Some((index, screen)) = self
            .screens
            .iter_mut()
            .enumerate()
            .find(|(_, screen)| screen.node.trim() == local_node.as_str())
        else {
            return Err(format!(
                "Add the local node `{local_node}` to the topology first"
            ));
        };
        screen.width = geometry.size.width.to_string();
        screen.height = geometry.size.height.to_string();
        screen.monitor_count = geometry.monitor_count;
        self.selected_screen = index;
        Ok(())
    }

    fn set_agent_desktop(&mut self, geometry: platform::DesktopGeometry) {
        self.agent_width = geometry.size.width.to_string();
        self.agent_height = geometry.size.height.to_string();
        self.agent_monitor_count = geometry.monitor_count;
    }

    fn update_screen(&mut self, placement: &ScreenPlacement, monitor_count: u32) {
        let Some((index, screen)) = self
            .screens
            .iter_mut()
            .enumerate()
            .find(|(_, screen)| screen.node.trim() == placement.node.as_str())
        else {
            return;
        };
        screen.width = placement.size.width.to_string();
        screen.height = placement.size.height.to_string();
        screen.monitor_count = monitor_count;
        self.selected_screen = index;
    }

    fn place_screen(&mut self, index: usize, target: GridSlot) {
        if index >= self.screens.len()
            || target.column >= TOPOLOGY_COLUMNS
            || target.row >= TOPOLOGY_ROWS
        {
            return;
        }
        let source = self.screens[index].slot;
        if let Some(other) = self.screen_index_at(target) {
            self.screens[other].slot = source;
        } else if !slots_are_connected(self.screens.iter().enumerate().map(
            |(candidate, screen)| {
                if candidate == index {
                    target
                } else {
                    screen.slot
                }
            },
        )) {
            return;
        }
        self.screens[index].slot = target;
        self.selected_screen = index;
    }

    fn screen_index_at(&self, slot: GridSlot) -> Option<usize> {
        self.screens.iter().position(|screen| screen.slot == slot)
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ScreenGeometry {
    width: u32,
    height: u32,
}

struct ScreenEditor {
    node: String,
    slot: GridSlot,
    width: String,
    height: String,
    monitor_count: u32,
}

impl ScreenEditor {
    fn from_local_node(node: &NodeId) -> Self {
        Self::from_node(
            node.to_string(),
            GridSlot::new(TOPOLOGY_COLUMNS / 2, TOPOLOGY_ROWS / 2),
        )
    }

    fn from_node(node: String, slot: GridSlot) -> Self {
        Self {
            node,
            slot,
            width: String::from("1920"),
            height: String::from("1080"),
            monitor_count: 1,
        }
    }

    fn from_placement(placement: &ScreenPlacement) -> Self {
        Self {
            node: placement.node.to_string(),
            slot: placement.slot,
            width: placement.size.width.to_string(),
            height: placement.size.height.to_string(),
            monitor_count: 1,
        }
    }

    fn build(&self, index: usize) -> Result<ScreenPlacement, String> {
        let number = index + 1;
        let node = NodeId::new(self.node.trim())
            .map_err(|error| format!("Machine {number} node: {error}"))?;
        let width = parse_nonzero(&format!("Machine {number} width"), &self.width)?;
        let height = parse_nonzero(&format!("Machine {number} height"), &self.height)?;
        Ok(ScreenPlacement {
            node,
            slot: self.slot,
            size: Size::new(width, height),
        })
    }

    #[cfg(test)]
    fn geometry(&self) -> Option<ScreenGeometry> {
        let width = self.width.trim().parse().ok()?;
        let height = self.height.trim().parse().ok()?;
        if width == 0 || height == 0 {
            return None;
        }
        Some(ScreenGeometry { width, height })
    }
}

fn topology_grid(ui: &mut Ui, editor: &mut ConfigEditor, local_node: Option<&NodeId>) {
    const GAP: f32 = 6.0;
    let width = ui.available_width();
    let cell_width =
        ((width - GAP * f32::from(TOPOLOGY_COLUMNS - 1)) / f32::from(TOPOLOGY_COLUMNS)).max(24.0);
    let cell_height = (cell_width * 0.42).clamp(36.0, 50.0);
    let height =
        cell_height * f32::from(TOPOLOGY_ROWS) + GAP * f32::from(TOPOLOGY_ROWS.saturating_sub(1));
    let (canvas, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    let painter = ui.painter_at(canvas);
    let released = ui.input(|input| input.pointer.any_released());
    let mut placement = None;
    for row in 0..TOPOLOGY_ROWS {
        for column in 0..TOPOLOGY_COLUMNS {
            let slot = GridSlot::new(column, row);
            let minimum = egui::pos2(
                canvas.left() + f32::from(column) * (cell_width + GAP),
                canvas.top() + f32::from(row) * (cell_height + GAP),
            );
            let screen_rect =
                egui::Rect::from_min_size(minimum, Vec2::new(cell_width, cell_height));
            let screen_index = editor.screen_index_at(slot);
            let response = ui
                .interact(
                    screen_rect,
                    ui.id().with(("topology-slot", column, row)),
                    Sense::click_and_drag(),
                )
                .on_hover_cursor(if screen_index.is_some() {
                    egui::CursorIcon::Grab
                } else {
                    egui::CursorIcon::PointingHand
                });

            if response.clicked() {
                if let Some(index) = screen_index {
                    editor.selected_screen = index;
                } else {
                    placement = Some((editor.selected_screen, slot));
                }
            }
            if response.drag_started()
                && let Some(index) = screen_index
            {
                editor.selected_screen = index;
                editor.dragged_screen = Some(index);
            }
            if released
                && response.hovered()
                && let Some(index) = editor.dragged_screen
            {
                placement = Some((index, slot));
            }

            if let Some(index) = screen_index {
                let screen = &editor.screens[index];
                let local = local_node.is_some_and(|node| screen.node.trim() == node.as_str());
                paint_screen(
                    &painter,
                    screen_rect,
                    screen,
                    editor.selected_screen == index,
                    local,
                );
            } else {
                painter.rect_filled(screen_rect, CornerRadius::same(4), PANEL);
                painter.rect_stroke(
                    screen_rect,
                    CornerRadius::same(4),
                    Stroke::new(1.0, BORDER),
                    egui::StrokeKind::Inside,
                );
            }
        }
    }
    if released {
        editor.dragged_screen = None;
    }
    if let Some((index, slot)) = placement {
        editor.place_screen(index, slot);
    }
}

fn paint_screen(
    painter: &egui::Painter,
    screen_rect: egui::Rect,
    screen: &ScreenEditor,
    selected: bool,
    local: bool,
) {
    let stroke_color = if selected {
        ACCENT
    } else if local {
        SUCCESS
    } else {
        BORDER
    };
    painter.rect_filled(screen_rect, CornerRadius::same(4), ELEVATED);
    painter.rect_stroke(
        screen_rect,
        CornerRadius::same(4),
        Stroke::new(if selected { 2.0 } else { 1.0 }, stroke_color),
        egui::StrokeKind::Inside,
    );

    let node_size = (screen_rect.width() / 16.0).clamp(8.0, 14.0);
    let detail_size = node_size.min(11.0);
    let node_label = canvas_label(screen.node.trim(), screen_rect.width(), node_size);
    let center = screen_rect.center();
    painter.text(
        egui::pos2(center.x, center.y - detail_size * 0.65),
        egui::Align2::CENTER_CENTER,
        node_label,
        FontId::proportional(node_size),
        TEXT,
    );
    painter.text(
        egui::pos2(center.x, center.y + detail_size * 0.85),
        egui::Align2::CENTER_CENTER,
        format!(
            "{}x{}  |  {} display{}",
            screen.width,
            screen.height,
            screen.monitor_count,
            if screen.monitor_count == 1 { "" } else { "s" }
        ),
        FontId::monospace(detail_size),
        MUTED,
    );
}

fn canvas_label(value: &str, available_width: f32, font_size: f32) -> String {
    let estimated_character_width = font_size * 0.62;
    let maximum = (available_width / estimated_character_width).floor() as usize;
    if value.chars().count() <= maximum {
        return value.to_owned();
    }
    if maximum <= 3 {
        return String::from("...");
    }
    format!("{}...", value.chars().take(maximum - 3).collect::<String>())
}

fn slots_are_connected(slots: impl IntoIterator<Item = GridSlot>) -> bool {
    let slots = slots.into_iter().collect::<Vec<_>>();
    let Some(first) = slots.first().copied() else {
        return false;
    };
    let mut visited = vec![first];
    let mut index = 0;
    while let Some(slot) = visited.get(index).copied() {
        index += 1;
        for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
            if let Some(neighbor) = slot.neighbor(edge)
                && slots.contains(&neighbor)
                && !visited.contains(&neighbor)
            {
                visited.push(neighbor);
            }
        }
    }
    visited.len() == slots.len()
}

fn desktop_geometry(frame: &eframe::Frame) -> Option<platform::DesktopGeometry> {
    let window = frame.winit_window()?;
    let monitors = window.available_monitors().collect::<Vec<_>>();
    let first = monitors.first()?;
    let first_position = first.position();
    let first_size = first.size();
    let mut left = i64::from(first_position.x);
    let mut top = i64::from(first_position.y);
    let mut right = left + i64::from(first_size.width);
    let mut bottom = top + i64::from(first_size.height);
    for monitor in &monitors[1..] {
        let position = monitor.position();
        let size = monitor.size();
        left = left.min(i64::from(position.x));
        top = top.min(i64::from(position.y));
        right = right.max(i64::from(position.x) + i64::from(size.width));
        bottom = bottom.max(i64::from(position.y) + i64::from(size.height));
    }
    let width = u32::try_from(right.checked_sub(left)?).ok()?;
    let height = u32::try_from(bottom.checked_sub(top)?).ok()?;
    Some(platform::DesktopGeometry {
        origin: Point {
            x: i32::try_from(left).ok()?,
            y: i32::try_from(top).ok()?,
        },
        size: Size::new(NonZeroU32::new(width)?, NonZeroU32::new(height)?),
        monitor_count: u32::try_from(monitors.len()).ok()?,
    })
}

fn current_monitor_geometry(context: &egui::Context) -> Option<platform::DesktopGeometry> {
    context.input(|input| {
        let viewport = input.viewport();
        let size = viewport.monitor_size?;
        let scale = viewport.native_pixels_per_point.unwrap_or(1.0);
        Some(platform::DesktopGeometry {
            origin: Point { x: 0, y: 0 },
            size: Size::new(
                NonZeroU32::new(positive_pixel_dimension(size.x * scale)?)?,
                NonZeroU32::new(positive_pixel_dimension(size.y * scale)?)?,
            ),
            monitor_count: 1,
        })
    })
}

fn positive_pixel_dimension(value: f32) -> Option<u32> {
    (value.is_finite() && value >= 1.0 && value <= u32::MAX as f32).then(|| value.round() as u32)
}

fn parse_socket_address(label: &str, value: &str) -> Result<SocketAddr, String> {
    value
        .trim()
        .parse()
        .map_err(|error| format!("{label}: {error}"))
}

fn parse_nonzero(label: &str, value: &str) -> Result<NonZeroU32, String> {
    let value = value
        .trim()
        .parse::<u32>()
        .map_err(|error| format!("{label}: {error}"))?;
    NonZeroU32::new(value).ok_or_else(|| format!("{label} must be greater than zero"))
}

fn config_summary(config: &Config) -> String {
    match &config.role {
        Role::Controller { listen, topology } => format!(
            "Controller {} | {listen} | {} screens",
            config.node,
            topology.screens().len()
        ),
        Role::Agent {
            controller_node,
            controller,
            ..
        } => format!(
            "Agent {} | controller {controller_node} at {controller}",
            config.node
        ),
    }
}

struct Notice {
    message: String,
    color: Color32,
}

impl Notice {
    fn success(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            color: SUCCESS,
        }
    }

    fn error(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            color: DANGER,
        }
    }

    fn info(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            color: ACCENT,
        }
    }

    fn show(&self, ui: &mut Ui) {
        Frame::new()
            .fill(ELEVATED)
            .stroke(Stroke::new(1.0, self.color))
            .corner_radius(CornerRadius::same(5))
            .inner_margin(Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                status_label(ui, &self.message, self.color);
            });
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Settings(#[from] SettingsError),
    #[error("desktop UI failed: {0}")]
    Desktop(String),
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use domain::{GridSlot, NodeId, Point, ScreenPlacement, Size};
    use identity::IdentityStore;
    use tempfile::TempDir;

    use super::{
        ConfigEditor, ConfigRole, DesktopApp, RemoteDisplay, RuntimeEvent, RuntimeRole,
        ScreenEditor, ScreenGeometry, SessionState,
    };

    #[test]
    fn node_override_initializes_the_desktop_identity() {
        let directory =
            TempDir::new().unwrap_or_else(|error| panic!("temp directory failed: {error}"));
        let node = NodeId::new("studio-left")
            .unwrap_or_else(|error| panic!("test node should be valid: {error}"));
        let app = DesktopApp::load(
            directory.path().to_path_buf(),
            Some(node.clone()),
            telemetry::LogBuffer::default(),
        )
        .unwrap_or_else(|error| panic!("desktop initialization failed: {error}"));

        assert_eq!(app.settings.node.as_ref(), Some(&node));
        assert_eq!(
            app.identity.as_ref().map(|identity| identity.node()),
            Some(&node)
        );
        assert!(app.trust.is_some());
    }

    #[test]
    fn trusted_nodes_survive_a_desktop_restart() {
        let directory =
            TempDir::new().unwrap_or_else(|error| panic!("temp directory failed: {error}"));
        let remote_directory =
            TempDir::new().unwrap_or_else(|error| panic!("temp directory failed: {error}"));
        let local = NodeId::new("studio-left")
            .unwrap_or_else(|error| panic!("test node should be valid: {error}"));
        let remote_node = NodeId::new("studio-right")
            .unwrap_or_else(|error| panic!("test node should be valid: {error}"));
        let remote = IdentityStore::new(remote_directory.path())
            .load_or_create(&remote_node)
            .unwrap_or_else(|error| panic!("remote identity should be created: {error}"));
        let bundle = remote.pairing_bundle();
        let code = bundle.code().to_string();

        let mut app = DesktopApp::load(
            directory.path().to_path_buf(),
            Some(local.clone()),
            telemetry::LogBuffer::default(),
        )
        .unwrap_or_else(|error| panic!("desktop initialization failed: {error}"));
        app.trust
            .as_mut()
            .unwrap_or_else(|| panic!("trust store should be available"))
            .trust(bundle, &code)
            .unwrap_or_else(|error| panic!("peer should be trusted: {error}"));
        drop(app);

        let reloaded = DesktopApp::load(
            directory.path().to_path_buf(),
            Some(local),
            telemetry::LogBuffer::default(),
        )
        .unwrap_or_else(|error| panic!("desktop restart failed: {error}"));

        assert!(
            reloaded
                .trust
                .as_ref()
                .is_some_and(|trust| trust.get(&remote_node).is_some())
        );
    }

    #[test]
    fn configuration_editor_builds_a_valid_controller_topology() {
        let node = NodeId::new("studio-left")
            .unwrap_or_else(|error| panic!("test node should be valid: {error}"));
        let mut editor = ConfigEditor::for_node(Some(&node));
        editor.add_screen(String::from("studio-right"));

        let config = editor
            .build(node.clone())
            .unwrap_or_else(|error| panic!("editor should build a valid configuration: {error}"));

        assert_eq!(config.node, node);
        assert!(matches!(
            config.role,
            crate::config::Role::Controller { topology, .. }
                if topology.screens().len() == 2
        ));
    }

    #[test]
    fn configuration_editor_validates_agent_addresses() {
        let node = NodeId::new("studio-right")
            .unwrap_or_else(|error| panic!("test node should be valid: {error}"));
        let mut editor = ConfigEditor::for_node(Some(&node));
        editor.role = ConfigRole::Agent;
        editor.controller_address = String::from("not-an-address");

        assert!(editor.build(node).is_err());
    }

    #[test]
    fn configuration_editor_uses_the_local_desktop_geometry() {
        let local = NodeId::new("studio-left")
            .unwrap_or_else(|error| panic!("test node should be valid: {error}"));
        let mut editor = ConfigEditor::for_node(Some(&local));
        editor.add_screen(String::from("studio-right"));

        editor
            .set_local_desktop(
                &local,
                platform::DesktopGeometry {
                    origin: Point { x: 0, y: 0 },
                    size: Size::new(
                        NonZeroU32::new(5_760).unwrap_or(NonZeroU32::MIN),
                        NonZeroU32::new(1_080).unwrap_or(NonZeroU32::MIN),
                    ),
                    monitor_count: 3,
                },
            )
            .unwrap_or_else(|error| panic!("monitor dimensions should apply: {error}"));

        assert_eq!(editor.selected_screen, 0);
        assert_eq!(
            editor.screens[0].geometry(),
            Some(ScreenGeometry {
                width: 5_760,
                height: 1_080,
            })
        );
        assert_eq!(editor.screens[0].monitor_count, 3);
    }

    #[test]
    fn cached_desktop_geometry_applies_after_a_role_change() {
        let directory =
            TempDir::new().unwrap_or_else(|error| panic!("temp directory failed: {error}"));
        let node = NodeId::new("studio-left")
            .unwrap_or_else(|error| panic!("test node should be valid: {error}"));
        let mut app = DesktopApp::load(
            directory.path().to_path_buf(),
            Some(node),
            telemetry::LogBuffer::default(),
        )
        .unwrap_or_else(|error| panic!("desktop initialization failed: {error}"));
        let geometry = platform::DesktopGeometry {
            origin: Point { x: 0, y: 0 },
            size: Size::new(
                NonZeroU32::new(5_760).unwrap_or(NonZeroU32::MIN),
                NonZeroU32::new(1_080).unwrap_or(NonZeroU32::MIN),
            ),
            monitor_count: 3,
        };

        app.config_editor.role = ConfigRole::Agent;
        app.apply_local_desktop_to_editor(geometry)
            .unwrap_or_else(|error| panic!("desktop geometry should apply: {error}"));

        assert_eq!(app.config_editor.agent_width, "5760");
        assert_eq!(app.config_editor.agent_height, "1080");
        assert_eq!(app.config_editor.agent_monitor_count, 3);
    }

    #[test]
    fn reported_display_updates_live_state_and_the_visual_editor() {
        let local = NodeId::new("studio-left")
            .unwrap_or_else(|error| panic!("test node should be valid: {error}"));
        let remote = NodeId::new("studio-right")
            .unwrap_or_else(|error| panic!("test node should be valid: {error}"));
        let mut editor = ConfigEditor::for_node(Some(&local));
        editor.add_screen(remote.to_string());
        let screen = ScreenPlacement {
            node: remote.clone(),
            slot: GridSlot::new(3, 2),
            size: Size::new(
                NonZeroU32::new(2560).unwrap_or(NonZeroU32::MIN),
                NonZeroU32::new(1440).unwrap_or(NonZeroU32::MIN),
            ),
        };
        let mut state = SessionState::default();

        editor.update_screen(&screen, 2);
        state.apply(RuntimeEvent::DisplayChanged {
            screen: screen.clone(),
            monitor_count: 2,
        });

        assert_eq!(
            state.displays.get(&remote),
            Some(&RemoteDisplay {
                size: screen.size,
                monitor_count: 2,
            })
        );
        assert_eq!(editor.selected_screen, 1);
        assert_eq!(
            editor.screens[1].geometry(),
            Some(ScreenGeometry {
                width: 2560,
                height: 1440,
            })
        );
        assert_eq!(editor.screens[1].monitor_count, 2);
    }

    #[test]
    fn configuration_editor_places_machines_on_neighboring_slots() {
        let local = NodeId::new("studio-left")
            .unwrap_or_else(|error| panic!("test node should be valid: {error}"));
        let mut editor = ConfigEditor::for_node(Some(&local));
        editor.add_screen(String::from("studio-right"));

        editor.place_screen(1, GridSlot::new(2, 3));

        assert_eq!(editor.screens[1].slot, GridSlot::new(2, 3));
        editor
            .build(local)
            .unwrap_or_else(|error| panic!("grid topology should be valid: {error}"));
    }

    #[test]
    fn screen_geometry_rejects_zero_dimensions() {
        let screen = ScreenEditor {
            node: String::from("studio-left"),
            slot: GridSlot::new(2, 2),
            width: String::from("0"),
            height: String::from("1080"),
            monitor_count: 1,
        };

        assert_eq!(screen.geometry(), None);
    }

    #[test]
    fn live_status_distinguishes_connection_and_remote_focus() {
        let remote = NodeId::new("studio-right")
            .unwrap_or_else(|error| panic!("test node should be valid: {error}"));
        let mut state = SessionState::default();
        state.apply(RuntimeEvent::Starting {
            role: RuntimeRole::Controller,
        });
        state.apply(RuntimeEvent::NativeReady {
            backend: platform::BackendKind::WindowsHooks,
        });
        state.apply(RuntimeEvent::Connected {
            peer: remote.clone(),
            session_id: 42,
        });
        state.apply(RuntimeEvent::FocusChanged { node: remote });

        assert!(state.is_ready());
        assert!(state.is_controlling_remote());
        assert_eq!(state.status().0, "Connected");
    }
}
