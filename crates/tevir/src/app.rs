use std::{
    collections::BTreeMap,
    net::SocketAddr,
    num::NonZeroU32,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use discovery::{DiscoveredNode, DiscoveryService, NearbyNodes};
use domain::{
    DesktopLayout, DisplayRotation, Monitor, NodeId, Point, Rect, ScreenPlacement, Size, Topology,
};
use eframe::egui::{
    self, Align, Button, Color32, ComboBox, CornerRadius, FontFamily, FontId, Frame, Layout,
    Margin, RichText, ScrollArea, Sense, Stroke, TextEdit, TextStyle, Ui, Vec2, ViewportBuilder,
    ViewportCommand,
};
use identity::{IdentityStore, LocalIdentity, PairingBundle, TrustStore};
use platform::{EnvironmentStatus, PlatformReport};
use protocol::Capabilities;
use telemetry::{LogBuffer, LogEntry, LogLevel};

use crate::{
    config::{Config, EdgeBehavior, Role},
    desktop::{DesktopIntegration, TrayAction, set_autostart},
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
    desktop_integration: Option<DesktopIntegration>,
    allow_window_close: bool,
    show_window_on_first_frame: bool,
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
            desktop_integration: None,
            allow_window_close: false,
            show_window_on_first_frame: false,
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
                if let Some(geometry) = self.local_desktop.clone() {
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
            .filter(|geometry| self.local_desktop.as_ref() != Some(geometry));
        let Some(geometry) = geometry else {
            return;
        };
        self.local_desktop = Some(geometry.clone());
        let result = self.apply_local_desktop_to_editor(geometry.clone());
        match result {
            Ok(()) => tracing::info!(
                width = geometry.size().width.get(),
                height = geometry.size().height.get(),
                monitors = geometry.monitor_count(),
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
        let can_apply_live = self.session_runtime.is_some()
            && self.session_state.role == Some(RuntimeRole::Controller)
            && self.saved_config.as_ref().is_some_and(|previous| {
                previous.node == config.node
                    && matches!(
                        (&previous.role, &config.role),
                        (
                            Role::Controller {
                                listen: previous_listen,
                                ..
                            },
                            Role::Controller { listen, .. },
                        ) if previous_listen == listen
                    )
            });
        match config.save(&path) {
            Ok(()) => {
                self.config_summary = Some(config_summary(&config));
                self.saved_config = Some(config.clone());
                self.remember_config_path(&path);
                self.start_discovery();
                tracing::info!(path = %path.display(), "configuration saved");
                if can_apply_live
                    && let (
                        Some(runtime),
                        Role::Controller {
                            topology,
                            edge_behavior,
                            ..
                        },
                    ) = (self.session_runtime.as_ref(), &config.role)
                    && runtime
                        .reconfigure_controller(topology.clone(), *edge_behavior)
                        .is_ok()
                {
                    self.notice = Some(Notice::success("Configuration saved and applying"));
                } else {
                    self.notice = Some(Notice::success("Configuration saved"));
                    self.start_session(config);
                }
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

    fn return_control(&mut self) {
        let Some(runtime) = self.session_runtime.as_ref() else {
            return;
        };
        match runtime.return_control() {
            Ok(()) => {
                self.notice = Some(Notice::info("Returning control"));
            }
            Err(error) => {
                self.notice = Some(Notice::error(error.to_string()));
            }
        }
    }

    fn initialize_desktop_integration(&mut self) {
        match DesktopIntegration::start() {
            Ok(integration) => {
                self.desktop_integration = Some(integration);
                tracing::info!("system tray integration ready");
            }
            Err(error) => {
                tracing::warn!(%error, "system tray integration unavailable");
                if self.settings.start_minimized {
                    self.show_window_on_first_frame = true;
                }
                self.notice = Some(Notice::error(error.to_string()));
            }
        }
    }

    fn poll_desktop_integration(&mut self, context: &egui::Context) {
        if self.show_window_on_first_frame {
            context.send_viewport_cmd(ViewportCommand::Visible(true));
            self.show_window_on_first_frame = false;
        }

        let actions = self
            .desktop_integration
            .as_ref()
            .map_or_else(Vec::new, |integration| {
                integration.set_return_control_enabled(self.session_state.is_controlling_remote());
                integration.poll()
            });
        for action in actions {
            match action {
                TrayAction::Show => {
                    context.send_viewport_cmd(ViewportCommand::Visible(true));
                    context.send_viewport_cmd(ViewportCommand::Minimized(false));
                    context.send_viewport_cmd(ViewportCommand::Focus);
                }
                TrayAction::ReturnControl => self.return_control(),
                TrayAction::Quit => {
                    self.allow_window_close = true;
                    context.send_viewport_cmd(ViewportCommand::Close);
                }
            }
        }

        if context.input(|input| input.viewport().close_requested())
            && !self.allow_window_close
            && self.settings.keep_running_in_tray
            && self.desktop_integration.is_some()
        {
            context.send_viewport_cmd(ViewportCommand::CancelClose);
            context.send_viewport_cmd(ViewportCommand::Visible(false));
            tracing::info!("application window hidden to the system tray");
        }
    }

    fn update_autostart(&mut self, enabled: bool) {
        match set_autostart(enabled) {
            Ok(()) => {
                self.settings.autostart = enabled;
                self.save_desktop_settings();
                tracing::info!(enabled, "desktop autostart updated");
            }
            Err(error) => {
                tracing::error!(enabled, %error, "desktop autostart update failed");
                self.notice = Some(Notice::error(error.to_string()));
            }
        }
    }

    fn save_desktop_settings(&mut self) {
        if let Err(error) = self.settings.save(&self.data_directory) {
            tracing::error!(%error, "desktop settings save failed");
            self.notice = Some(Notice::error(error.to_string()));
        }
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
                    self.local_desktop = Some(geometry.clone());
                    let _ = self.apply_local_desktop_to_editor(geometry.clone());
                }
                RuntimeEvent::DisplayChanged { screen } => {
                    self.config_editor.update_screen(screen);
                    let monitor_count = screen.layout.monitor_count();
                    self.notice = Some(Notice::info(format!(
                        "{} desktop updated to {} x {} across {} display{}",
                        screen.node,
                        screen.bounds.size.width,
                        screen.bounds.size.height,
                        monitor_count,
                        if monitor_count == 1 { "" } else { "s" }
                    )));
                }
                RuntimeEvent::ConfigurationApplied => {
                    self.notice = Some(Notice::success("Configuration applied"));
                }
                RuntimeEvent::ClipboardReady { .. } => {
                    self.notice = Some(Notice::success("Clipboard synchronization ready"));
                }
                RuntimeEvent::ClipboardSynchronized { peer, received } => {
                    self.notice = Some(Notice::info(if *received {
                        format!("Clipboard received from {peer}")
                    } else {
                        format!("Clipboard sent to {peer}")
                    }));
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
                    if ui
                        .add_enabled(
                            self.session_state.is_controlling_remote(),
                            Button::new("Return control").min_size(Vec2::new(112.0, 34.0)),
                        )
                        .clicked()
                    {
                        self.return_control();
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
        let (native_label, native_color) = self.native_input_status();
        metric_row(ui, "Native permission", native_label, native_color);
        metric_row(
            ui,
            "Text clipboard",
            if self.session_state.clipboard_ready {
                "Ready"
            } else {
                "Waiting"
            },
            if self.session_state.clipboard_ready {
                SUCCESS
            } else {
                MUTED
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
            "Desktop environment",
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
            if let Some(geometry) = self.local_desktop.clone() {
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

        ui.add_space(30.0);
        self.application_configuration(ui);
    }

    fn application_configuration(&mut self, ui: &mut Ui) {
        section_heading(ui, "Application", "Startup and background behavior");
        ui.add_space(10.0);

        let mut autostart = self.settings.autostart;
        if ui
            .checkbox(&mut autostart, "Start automatically after sign-in")
            .changed()
        {
            self.update_autostart(autostart);
        }

        let tray_available = self.desktop_integration.is_some();
        ui.add_enabled_ui(tray_available, |ui| {
            let mut start_minimized = self.settings.start_minimized;
            if ui
                .checkbox(&mut start_minimized, "Start with the window hidden")
                .changed()
            {
                self.settings.start_minimized = start_minimized;
                self.save_desktop_settings();
            }

            let mut keep_running = self.settings.keep_running_in_tray;
            if ui
                .checkbox(&mut keep_running, "Keep running when the window closes")
                .changed()
            {
                self.settings.keep_running_in_tray = keep_running;
                self.save_desktop_settings();
            }
        });
        if !tray_available {
            status_label(ui, "System tray unavailable", DANGER);
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
                self.config_editor.agent_layout.monitor_count(),
                if self.config_editor.agent_layout.monitor_count() == 1 {
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
                if ui.add(Button::new("Add machine")).clicked() {
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
        topology_canvas(ui, &mut self.config_editor, local_node.as_ref());
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
        let mut alignment = None;
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
                {
                    let screen = &mut self.config_editor.screens[selected];
                    labeled_text_field(ui, "Node", &mut screen.node, "node-id");
                    ui.add_space(8.0);
                    ui.columns(2, |columns| {
                        compact_text_field(&mut columns[0], "Width", &mut screen.width, "1920");
                        compact_text_field(&mut columns[1], "Height", &mut screen.height, "1080");
                    });
                    ui.add_space(8.0);
                    ui.columns(2, |columns| {
                        compact_text_field(&mut columns[0], "X", &mut screen.x, "0");
                        compact_text_field(&mut columns[1], "Y", &mut screen.y, "0");
                    });
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(format!(
                            "{} display{}",
                            screen.layout.monitor_count(),
                            if screen.layout.monitor_count() == 1 {
                                ""
                            } else {
                                "s"
                            }
                        ))
                        .small()
                        .color(MUTED),
                    );
                    for (index, monitor) in screen.layout.monitors().iter().enumerate() {
                        let name = monitor
                            .name
                            .as_deref()
                            .map_or_else(|| format!("Display {}", index + 1), str::to_owned);
                        ui.label(
                            RichText::new(format!(
                                "{name}: {} x {} at {}%, {}",
                                monitor.bounds.size.width,
                                monitor.bounds.size.height,
                                monitor.scale_milli.get() / 10,
                                rotation_label(monitor.rotation),
                            ))
                            .small()
                            .color(MUTED),
                        );
                    }
                }
                ui.add_space(6.0);
                if let Some(axis) = self.config_editor.alignment_axis(selected) {
                    ui.label(RichText::new("Align edge").small().color(MUTED));
                    ui.horizontal(|ui| {
                        let labels = match axis {
                            AlignmentAxis::Vertical(_) => ["Top", "Center", "Bottom"],
                            AlignmentAxis::Horizontal(_) => ["Left", "Center", "Right"],
                        };
                        for (label, value) in labels.into_iter().zip([
                            ScreenAlignment::Start,
                            ScreenAlignment::Center,
                            ScreenAlignment::End,
                        ]) {
                            if ui.selectable_label(false, label).clicked() {
                                alignment = Some(value);
                            }
                        }
                    });
                }
            });
        if let Some(alignment) = alignment {
            self.config_editor.align_screen(selected, alignment);
        }
        if remove_selected {
            self.config_editor.remove_selected();
        }

        ui.add_space(26.0);
        section_heading(ui, "Edge switching", "Controller capture behavior");
        ui.add_space(10.0);
        ui.columns(2, |columns| {
            columns[0].label(RichText::new("Switch delay").color(MUTED));
            columns[0].add(
                egui::Slider::new(
                    &mut self.config_editor.edge_behavior.switch_delay_ms,
                    0..=2_000,
                )
                .suffix(" ms"),
            );
            columns[1].label(RichText::new("Corner dead zone").color(MUTED));
            columns[1].add(
                egui::Slider::new(
                    &mut self.config_editor.edge_behavior.corner_dead_zone_percent,
                    0..=25,
                )
                .suffix("%"),
            );
        });
        ui.add_space(8.0);
        egui::Grid::new("edge-behavior-grid")
            .num_columns(4)
            .spacing([14.0, 8.0])
            .show(ui, |ui| {
                ui.label(RichText::new("Edge").color(MUTED));
                ui.label(RichText::new("Enabled").color(MUTED));
                ui.label(RichText::new("Active start").color(MUTED));
                ui.label(RichText::new("Active end").color(MUTED));
                ui.end_row();
                for (edge, label) in [
                    (domain::Edge::Left, "Left"),
                    (domain::Edge::Right, "Right"),
                    (domain::Edge::Top, "Top"),
                    (domain::Edge::Bottom, "Bottom"),
                ] {
                    let rule = self.config_editor.edge_behavior.rule_mut(edge);
                    ui.label(label);
                    ui.checkbox(&mut rule.enabled, "");
                    let maximum_start = rule.active_end_percent.saturating_sub(1);
                    ui.add_enabled(
                        rule.enabled,
                        egui::Slider::new(&mut rule.active_start_percent, 0..=maximum_start)
                            .suffix("%"),
                    );
                    let minimum_end = rule.active_start_percent.saturating_add(1);
                    ui.add_enabled(
                        rule.enabled,
                        egui::Slider::new(&mut rule.active_end_percent, minimum_end..=100)
                            .suffix("%"),
                    );
                    ui.end_row();
                }
            });
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
        let (native_label, native_color) = self.native_input_status();
        metric_row(ui, "Native permission", native_label, native_color);
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

    fn native_input_status(&self) -> (&'static str, Color32) {
        if !self.report.is_available() {
            return ("Unavailable", DANGER);
        }
        match self.native_input.as_ref() {
            Some(native_input) if native_input.is_ready() => ("Granted", SUCCESS),
            Some(native_input) if native_input.is_finished() => ("Closed", DANGER),
            Some(_) => ("Requesting", WARNING),
            None => ("Not started", WARNING),
        }
    }
}

impl eframe::App for DesktopApp {
    fn ui(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        self.sync_desktop_geometry(frame, ui.ctx());
        self.start_deferred_session();
        self.poll_discovery();
        self.poll_session();
        self.poll_native_input();
        self.poll_desktop_integration(ui.ctx());
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
    let start_minimized = app.settings.start_minimized;
    let options = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_title("Tevir")
            .with_app_id("tevir")
            .with_inner_size([1080.0, 760.0])
            .with_min_inner_size([760.0, 520.0])
            .with_visible(!start_minimized),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "Tevir",
        options,
        Box::new(move |creation| {
            configure_style(&creation.egui_ctx);
            app.initialize_desktop_integration();
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
    clipboard_ready: bool,
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
                self.clipboard_ready = false;
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
            RuntimeEvent::ClipboardReady { backend } => {
                tracing::info!(?backend, "native clipboard backend ready");
                self.clipboard_ready = true;
            }
            RuntimeEvent::ClipboardSynchronized { peer, received } => {
                tracing::info!(%peer, received, "clipboard synchronized");
            }
            RuntimeEvent::FocusChanged { node } => {
                self.focus = Some(node);
            }
            RuntimeEvent::AgentControl { controller, active } => {
                tracing::info!(%controller, active, "agent control state changed");
                self.agent_controlled = active;
            }
            RuntimeEvent::LocalDesktopChanged { .. } => {}
            RuntimeEvent::DisplayChanged { screen } => {
                let monitor_count =
                    u32::try_from(screen.layout.monitor_count()).unwrap_or(u32::MAX);
                self.displays.insert(
                    screen.node,
                    RemoteDisplay {
                        size: screen.bounds.size,
                        monitor_count,
                    },
                );
            }
            RuntimeEvent::ConfigurationApplied => {}
            RuntimeEvent::Error { message } => self.record_error(message),
            RuntimeEvent::Stopped => {
                self.connected.clear();
                self.displays.clear();
                self.agent_controlled = false;
                self.native_ready = false;
                self.clipboard_ready = false;
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
    agent_layout: DesktopLayout,
    edge_behavior: EdgeBehavior,
    screens: Vec<ScreenEditor>,
    selected_screen: usize,
    canvas_view: Option<CanvasView>,
    drag_origin: Option<DragOrigin>,
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
            agent_layout: DesktopLayout::single(Size::new(
                NonZeroU32::new(1920).unwrap_or(NonZeroU32::MIN),
                NonZeroU32::new(1080).unwrap_or(NonZeroU32::MIN),
            )),
            edge_behavior: EdgeBehavior::default(),
            screens: vec![ScreenEditor::from_node(
                node.map_or_else(|| String::from("local-node"), ToString::to_string),
                0,
                0,
            )],
            selected_screen: 0,
            canvas_view: None,
            drag_origin: None,
        }
    }

    fn from_config(config: &Config) -> Self {
        match &config.role {
            Role::Controller {
                listen,
                topology,
                edge_behavior,
            } => Self {
                role: ConfigRole::Controller,
                listen_address: listen.to_string(),
                controller_node: String::from("peer-node"),
                controller_address: String::from("127.0.0.1:24800"),
                agent_width: String::from("1920"),
                agent_height: String::from("1080"),
                agent_layout: DesktopLayout::single(Size::new(
                    NonZeroU32::new(1920).unwrap_or(NonZeroU32::MIN),
                    NonZeroU32::new(1080).unwrap_or(NonZeroU32::MIN),
                )),
                edge_behavior: *edge_behavior,
                screens: topology
                    .screens()
                    .iter()
                    .map(ScreenEditor::from_placement)
                    .collect(),
                selected_screen: 0,
                canvas_view: None,
                drag_origin: None,
            },
            Role::Agent {
                controller_node,
                controller,
                display_layout,
            } => Self {
                role: ConfigRole::Agent,
                listen_address: String::from("0.0.0.0:24800"),
                controller_node: controller_node.to_string(),
                controller_address: controller.to_string(),
                agent_width: display_layout.size().width.to_string(),
                agent_height: display_layout.size().height.to_string(),
                agent_layout: display_layout.clone(),
                edge_behavior: EdgeBehavior::default(),
                screens: vec![ScreenEditor::from_local_node(&config.node)],
                selected_screen: 0,
                canvas_view: None,
                drag_origin: None,
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
                Role::Controller {
                    listen,
                    topology,
                    edge_behavior: self.edge_behavior,
                }
            }
            ConfigRole::Agent => {
                let controller_node = NodeId::new(self.controller_node.trim())
                    .map_err(|error| format!("Controller node: {error}"))?;
                let display_size = Size::new(
                    parse_nonzero("Agent screen width", &self.agent_width)?,
                    parse_nonzero("Agent screen height", &self.agent_height)?,
                );
                let display_layout = if self.agent_layout.size() == display_size {
                    self.agent_layout.clone()
                } else {
                    DesktopLayout::single(display_size)
                };
                Role::Agent {
                    controller_node,
                    controller: parse_socket_address(
                        "Controller address",
                        &self.controller_address,
                    )?,
                    display_layout,
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
        let selected = self
            .screens
            .get(self.selected_screen)
            .and_then(ScreenEditor::geometry)
            .unwrap_or(ScreenGeometry {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            });
        let x = saturating_i64_to_i32(i64::from(selected.x) + i64::from(selected.width));
        let y =
            saturating_i64_to_i32(i64::from(selected.y) + (i64::from(selected.height) - 1080) / 2);
        self.screens.push(ScreenEditor::from_node(node, x, y));
        self.selected_screen = self.screens.len() - 1;
        self.canvas_view = None;
    }

    fn remove_selected(&mut self) {
        if !self.can_remove_screen(self.selected_screen) {
            return;
        }
        self.screens.remove(self.selected_screen);
        self.selected_screen = self
            .selected_screen
            .min(self.screens.len().saturating_sub(1));
        self.canvas_view = None;
        self.drag_origin = None;
    }

    fn can_remove_screen(&self, index: usize) -> bool {
        if self.screens.len() <= 1 || index >= self.screens.len() {
            return false;
        }
        let geometries = self
            .screens
            .iter()
            .enumerate()
            .filter(|(candidate, _)| *candidate != index)
            .map(|(_, screen)| screen.geometry())
            .collect::<Option<Vec<_>>>();
        geometries.is_some_and(|geometries| geometries_are_connected(&geometries))
    }

    fn set_local_desktop(
        &mut self,
        local_node: &NodeId,
        geometry: platform::DesktopGeometry,
    ) -> Result<(), String> {
        let Some(index) = self
            .screens
            .iter()
            .position(|screen| screen.node.trim() == local_node.as_str())
        else {
            return Err(format!(
                "Add the local node `{local_node}` to the topology first"
            ));
        };
        let previous_origin = self.screens[index]
            .geometry()
            .map(|screen| (screen.x, screen.y));
        self.resize_screen(
            index,
            geometry.size().width.get(),
            geometry.size().height.get(),
        );
        if let (Some((previous_x, previous_y)), Some(updated)) =
            (previous_origin, self.screens[index].geometry())
        {
            let dx = i64::from(previous_x) - i64::from(updated.x);
            let dy = i64::from(previous_y) - i64::from(updated.y);
            for screen in &mut self.screens {
                let Some(screen_geometry) = screen.geometry() else {
                    continue;
                };
                screen.x = saturating_i64_to_i32(i64::from(screen_geometry.x) + dx).to_string();
                screen.y = saturating_i64_to_i32(i64::from(screen_geometry.y) + dy).to_string();
            }
        }
        self.screens[index].layout = geometry.layout;
        self.selected_screen = index;
        self.canvas_view = None;
        Ok(())
    }

    fn set_agent_desktop(&mut self, geometry: platform::DesktopGeometry) {
        self.agent_width = geometry.size().width.to_string();
        self.agent_height = geometry.size().height.to_string();
        self.agent_layout = geometry.layout;
    }

    fn update_screen(&mut self, placement: &ScreenPlacement) {
        let Some((index, screen)) = self
            .screens
            .iter_mut()
            .enumerate()
            .find(|(_, screen)| screen.node.trim() == placement.node.as_str())
        else {
            return;
        };
        screen.x = placement.bounds.origin.x.to_string();
        screen.y = placement.bounds.origin.y.to_string();
        screen.width = placement.bounds.size.width.to_string();
        screen.height = placement.bounds.size.height.to_string();
        screen.layout = placement.layout.clone();
        self.selected_screen = index;
        self.canvas_view = None;
    }

    fn resize_screen(&mut self, index: usize, width: u32, height: u32) {
        let Some(current) = self.screens.get(index).and_then(ScreenEditor::geometry) else {
            return;
        };
        let mut x =
            i64::from(current.x) + (i64::from(current.width).saturating_sub(i64::from(width))) / 2;
        let mut y = i64::from(current.y)
            + (i64::from(current.height).saturating_sub(i64::from(height))) / 2;
        for (other_index, screen) in self.screens.iter().enumerate() {
            if other_index == index {
                continue;
            }
            let Some(other) = screen.geometry() else {
                continue;
            };
            if current.left() == other.right() {
                x = current.left();
            } else if current.right() == other.left() {
                x = other.left() - i64::from(width);
            }
            if current.top() == other.bottom() {
                y = current.top();
            } else if current.bottom() == other.top() {
                y = other.top() - i64::from(height);
            }
        }
        let Some(screen) = self.screens.get_mut(index) else {
            return;
        };
        screen.x = saturating_i64_to_i32(x).to_string();
        screen.y = saturating_i64_to_i32(y).to_string();
        screen.width = width.to_string();
        screen.height = height.to_string();
    }

    fn move_screen(&mut self, index: usize, x: i32, y: i32) {
        if let Some(screen) = self.screens.get_mut(index) {
            screen.x = x.to_string();
            screen.y = y.to_string();
        }
    }

    fn snap_screen(&mut self, index: usize) {
        let Some(moved) = self.screens.get(index).and_then(ScreenEditor::geometry) else {
            return;
        };
        let mut closest = None;
        for (other_index, screen) in self.screens.iter().enumerate() {
            if other_index == index {
                continue;
            }
            let Some(other) = screen.geometry() else {
                continue;
            };
            for (x, y) in snap_candidates(moved, other) {
                let candidate = ScreenGeometry { x, y, ..moved };
                let geometries = self
                    .screens
                    .iter()
                    .enumerate()
                    .map(|(candidate_index, screen)| {
                        (candidate_index == index)
                            .then_some(candidate)
                            .or_else(|| screen.geometry())
                    })
                    .collect::<Option<Vec<_>>>();
                let Some(geometries) = geometries else {
                    continue;
                };
                if geometries
                    .iter()
                    .enumerate()
                    .any(|(candidate_index, geometry)| {
                        candidate_index != index && candidate.overlaps(*geometry)
                    })
                    || !geometries_are_connected(&geometries)
                {
                    continue;
                }
                let dx = i64::from(x) - i64::from(moved.x);
                let dy = i64::from(y) - i64::from(moved.y);
                let distance = dx.saturating_mul(dx).saturating_add(dy.saturating_mul(dy));
                if closest.is_none_or(|(_, closest_distance)| distance < closest_distance) {
                    closest = Some(((x, y), distance));
                }
            }
        }
        if let Some(((x, y), _)) = closest {
            self.move_screen(index, x, y);
        }
        self.canvas_view = None;
    }

    fn alignment_axis(&self, index: usize) -> Option<AlignmentAxis> {
        let selected = self.screens.get(index)?.geometry()?;
        self.screens
            .iter()
            .enumerate()
            .filter(|(other_index, _)| *other_index != index)
            .find_map(|(other_index, screen)| {
                alignment_axis(selected, screen.geometry()?).map(|axis| match axis {
                    AlignmentAxis::Vertical(_) => AlignmentAxis::Vertical(other_index),
                    AlignmentAxis::Horizontal(_) => AlignmentAxis::Horizontal(other_index),
                })
            })
    }

    fn align_screen(&mut self, index: usize, alignment: ScreenAlignment) {
        let Some(selected) = self.screens.get(index).and_then(ScreenEditor::geometry) else {
            return;
        };
        let Some(axis) = self.alignment_axis(index) else {
            return;
        };
        let neighbor_index = match axis {
            AlignmentAxis::Vertical(index) | AlignmentAxis::Horizontal(index) => index,
        };
        let Some(other) = self
            .screens
            .get(neighbor_index)
            .and_then(ScreenEditor::geometry)
        else {
            return;
        };
        let (x, y) = match axis {
            AlignmentAxis::Vertical(_) => (
                selected.x,
                align_coordinate(other.top(), other.height, selected.height, alignment),
            ),
            AlignmentAxis::Horizontal(_) => (
                align_coordinate(other.left(), other.width, selected.width, alignment),
                selected.y,
            ),
        };
        let candidate = ScreenGeometry { x, y, ..selected };
        if self
            .screens
            .iter()
            .enumerate()
            .any(|(other_index, screen)| {
                other_index != index
                    && other_index != neighbor_index
                    && screen
                        .geometry()
                        .is_some_and(|geometry| candidate.overlaps(geometry))
            })
        {
            return;
        }
        self.move_screen(index, x, y);
        self.canvas_view = None;
    }
}

#[derive(Clone, Copy, Debug)]
struct CanvasView {
    center_x: f32,
    center_y: f32,
    scale: f32,
}

impl CanvasView {
    fn fit(canvas: egui::Rect, screens: &[(usize, ScreenGeometry)]) -> Option<Self> {
        let mut screens = screens.iter().map(|(_, screen)| *screen);
        let first = screens.next()?;
        let mut left = first.left();
        let mut top = first.top();
        let mut right = first.right();
        let mut bottom = first.bottom();
        for screen in screens {
            left = left.min(screen.left());
            top = top.min(screen.top());
            right = right.max(screen.right());
            bottom = bottom.max(screen.bottom());
        }

        let available = canvas.shrink(24.0).size();
        let world_width = (right - left).max(1) as f32;
        let world_height = (bottom - top).max(1) as f32;
        Some(Self {
            center_x: (left as f32 + right as f32) / 2.0,
            center_y: (top as f32 + bottom as f32) / 2.0,
            scale: (available.x / world_width)
                .min(available.y / world_height)
                .max(0.000_001),
        })
    }

    fn screen_rect(self, canvas: egui::Rect, screen: ScreenGeometry) -> egui::Rect {
        let left = canvas.center().x + (screen.x as f32 - self.center_x) * self.scale;
        let top = canvas.center().y + (screen.y as f32 - self.center_y) * self.scale;
        egui::Rect::from_min_size(
            egui::pos2(left, top),
            Vec2::new(
                screen.width as f32 * self.scale,
                screen.height as f32 * self.scale,
            ),
        )
    }
}

#[derive(Clone, Copy, Debug)]
struct DragOrigin {
    index: usize,
    x: i32,
    y: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ScreenGeometry {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

impl ScreenGeometry {
    const fn left(self) -> i64 {
        self.x as i64
    }

    const fn top(self) -> i64 {
        self.y as i64
    }

    const fn right(self) -> i64 {
        self.left() + self.width as i64
    }

    const fn bottom(self) -> i64 {
        self.top() + self.height as i64
    }

    const fn overlaps(self, other: Self) -> bool {
        self.left() < other.right()
            && self.right() > other.left()
            && self.top() < other.bottom()
            && self.bottom() > other.top()
    }

    const fn shares_edge(self, other: Self) -> bool {
        let vertical_overlap = self.top() < other.bottom() && self.bottom() > other.top();
        let horizontal_overlap = self.left() < other.right() && self.right() > other.left();
        ((self.right() == other.left() || other.right() == self.left()) && vertical_overlap)
            || ((self.bottom() == other.top() || other.bottom() == self.top())
                && horizontal_overlap)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AlignmentAxis {
    Vertical(usize),
    Horizontal(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScreenAlignment {
    Start,
    Center,
    End,
}

struct ScreenEditor {
    node: String,
    x: String,
    y: String,
    width: String,
    height: String,
    layout: DesktopLayout,
}

impl ScreenEditor {
    fn from_local_node(node: &NodeId) -> Self {
        Self::from_node(node.to_string(), 0, 0)
    }

    fn from_node(node: String, x: i32, y: i32) -> Self {
        Self {
            node,
            x: x.to_string(),
            y: y.to_string(),
            width: String::from("1920"),
            height: String::from("1080"),
            layout: DesktopLayout::single(Size::new(
                NonZeroU32::new(1920).unwrap_or(NonZeroU32::MIN),
                NonZeroU32::new(1080).unwrap_or(NonZeroU32::MIN),
            )),
        }
    }

    fn from_placement(placement: &ScreenPlacement) -> Self {
        Self {
            node: placement.node.to_string(),
            x: placement.bounds.origin.x.to_string(),
            y: placement.bounds.origin.y.to_string(),
            width: placement.bounds.size.width.to_string(),
            height: placement.bounds.size.height.to_string(),
            layout: placement.layout.clone(),
        }
    }

    fn build(&self, index: usize) -> Result<ScreenPlacement, String> {
        let number = index + 1;
        let node = NodeId::new(self.node.trim())
            .map_err(|error| format!("Machine {number} node: {error}"))?;
        let x = self
            .x
            .trim()
            .parse()
            .map_err(|error| format!("Machine {number} X: {error}"))?;
        let y = self
            .y
            .trim()
            .parse()
            .map_err(|error| format!("Machine {number} Y: {error}"))?;
        let width = parse_nonzero(&format!("Machine {number} width"), &self.width)?;
        let height = parse_nonzero(&format!("Machine {number} height"), &self.height)?;
        let size = Size::new(width, height);
        let layout = if self.layout.size() == size {
            self.layout.clone()
        } else {
            DesktopLayout::single(size)
        };
        ScreenPlacement::with_layout(node, Point { x, y }, layout)
            .map_err(|error| format!("Machine {number}: {error}"))
    }

    fn geometry(&self) -> Option<ScreenGeometry> {
        let width = self.width.trim().parse().ok()?;
        let height = self.height.trim().parse().ok()?;
        if width == 0 || height == 0 {
            return None;
        }
        Some(ScreenGeometry {
            x: self.x.trim().parse().ok()?,
            y: self.y.trim().parse().ok()?,
            width,
            height,
        })
    }
}

fn topology_canvas(ui: &mut Ui, editor: &mut ConfigEditor, local_node: Option<&NodeId>) {
    let (canvas, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 280.0), Sense::hover());
    let painter = ui.painter_at(canvas);
    painter.rect_filled(canvas, CornerRadius::same(5), CANVAS);
    painter.rect_stroke(
        canvas,
        CornerRadius::same(5),
        Stroke::new(1.0, BORDER),
        egui::StrokeKind::Inside,
    );
    paint_topology_grid(&painter, canvas);

    let screens = editor
        .screens
        .iter()
        .enumerate()
        .filter_map(|(index, screen)| Some((index, screen.geometry()?)))
        .collect::<Vec<_>>();
    if screens.is_empty() {
        return;
    }

    if editor.drag_origin.is_none() {
        editor.canvas_view = CanvasView::fit(canvas, &screens);
    }
    let Some(view) = editor.canvas_view else {
        return;
    };

    for (index, geometry) in screens {
        let screen_rect = view.screen_rect(canvas, geometry);
        let response = ui
            .interact(
                screen_rect,
                ui.id().with(("topology-screen", index)),
                Sense::click_and_drag(),
            )
            .on_hover_cursor(egui::CursorIcon::Grab)
            .on_hover_text(editor.screens[index].node.trim());

        if response.clicked() {
            editor.selected_screen = index;
        }
        if response.drag_started() {
            editor.selected_screen = index;
            editor.drag_origin = Some(DragOrigin {
                index,
                x: geometry.x,
                y: geometry.y,
            });
        }
        if let (Some(origin), Some(delta)) = (editor.drag_origin, response.total_drag_delta())
            && origin.index == index
        {
            let x = f64::from(origin.x) + f64::from(delta.x / view.scale);
            let y = f64::from(origin.y) + f64::from(delta.y / view.scale);
            editor.move_screen(
                index,
                saturating_f64_to_i32(x.round()),
                saturating_f64_to_i32(y.round()),
            );
        }
        if response.drag_stopped()
            && editor
                .drag_origin
                .is_some_and(|origin| origin.index == index)
        {
            editor.snap_screen(index);
            editor.drag_origin = None;
        }

        let local =
            local_node.is_some_and(|node| editor.screens[index].node.trim() == node.as_str());
        paint_screen(
            &painter,
            screen_rect,
            &editor.screens[index],
            geometry,
            editor.selected_screen == index,
            local,
            editor.edge_behavior,
        );
    }
}

fn paint_topology_grid(painter: &egui::Painter, canvas: egui::Rect) {
    let color = Color32::from_rgb(31, 34, 35);
    let spacing = 24.0;
    let mut x = canvas.left() + spacing;
    while x < canvas.right() {
        painter.vline(x, canvas.y_range(), Stroke::new(1.0, color));
        x += spacing;
    }
    let mut y = canvas.top() + spacing;
    while y < canvas.bottom() {
        painter.hline(canvas.x_range(), y, Stroke::new(1.0, color));
        y += spacing;
    }
}

fn paint_screen(
    painter: &egui::Painter,
    screen_rect: egui::Rect,
    screen: &ScreenEditor,
    geometry: ScreenGeometry,
    selected: bool,
    local: bool,
    edge_behavior: EdgeBehavior,
) {
    let painter = painter.with_clip_rect(screen_rect);
    let stroke_color = if selected {
        ACCENT
    } else if local {
        SUCCESS
    } else {
        BORDER
    };
    painter.rect_filled(screen_rect, CornerRadius::same(4), ELEVATED);
    let layout_width = screen.layout.size().width.get() as f32;
    let layout_height = screen.layout.size().height.get() as f32;
    for (index, monitor) in screen.layout.monitors().iter().enumerate() {
        let bounds = monitor.bounds;
        let monitor_rect = egui::Rect::from_min_max(
            egui::pos2(
                screen_rect.left() + screen_rect.width() * bounds.origin.x as f32 / layout_width,
                screen_rect.top() + screen_rect.height() * bounds.origin.y as f32 / layout_height,
            ),
            egui::pos2(
                screen_rect.left() + screen_rect.width() * bounds.right() as f32 / layout_width,
                screen_rect.top() + screen_rect.height() * bounds.bottom() as f32 / layout_height,
            ),
        );
        painter.rect_filled(
            monitor_rect.shrink(1.0),
            CornerRadius::same(2),
            if index % 2 == 0 { PANEL } else { CANVAS },
        );
        painter.rect_stroke(
            monitor_rect,
            CornerRadius::same(2),
            Stroke::new(1.0, BORDER),
            egui::StrokeKind::Inside,
        );
    }
    painter.rect_stroke(
        screen_rect,
        CornerRadius::same(4),
        Stroke::new(if selected { 2.0 } else { 1.0 }, stroke_color),
        egui::StrokeKind::Inside,
    );
    if local {
        paint_active_edges(&painter, screen_rect, edge_behavior);
    }

    let node_size = (screen_rect.width() / 16.0).clamp(8.0, 14.0);
    let detail_size = node_size.min(11.0);
    let node_label = canvas_label(screen.node.trim(), screen_rect.width(), node_size);
    let detail_label = canvas_label(
        &format!(
            "{}x{} | {} display{}",
            geometry.width,
            geometry.height,
            screen.layout.monitor_count(),
            if screen.layout.monitor_count() == 1 {
                ""
            } else {
                "s"
            }
        ),
        screen_rect.width(),
        detail_size,
    );
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
        detail_label,
        FontId::monospace(detail_size),
        MUTED,
    );
}

fn paint_active_edges(
    painter: &egui::Painter,
    screen_rect: egui::Rect,
    edge_behavior: EdgeBehavior,
) {
    for edge in [
        domain::Edge::Left,
        domain::Edge::Right,
        domain::Edge::Top,
        domain::Edge::Bottom,
    ] {
        let Some((start, end)) = edge_behavior.active_interval(edge) else {
            continue;
        };
        let start = start as f32;
        let end = end as f32;
        let (first, second) = match edge {
            domain::Edge::Left => (
                egui::pos2(screen_rect.left(), egui::lerp(screen_rect.y_range(), start)),
                egui::pos2(screen_rect.left(), egui::lerp(screen_rect.y_range(), end)),
            ),
            domain::Edge::Right => (
                egui::pos2(
                    screen_rect.right(),
                    egui::lerp(screen_rect.y_range(), start),
                ),
                egui::pos2(screen_rect.right(), egui::lerp(screen_rect.y_range(), end)),
            ),
            domain::Edge::Top => (
                egui::pos2(egui::lerp(screen_rect.x_range(), start), screen_rect.top()),
                egui::pos2(egui::lerp(screen_rect.x_range(), end), screen_rect.top()),
            ),
            domain::Edge::Bottom => (
                egui::pos2(
                    egui::lerp(screen_rect.x_range(), start),
                    screen_rect.bottom(),
                ),
                egui::pos2(egui::lerp(screen_rect.x_range(), end), screen_rect.bottom()),
            ),
        };
        painter.line_segment([first, second], Stroke::new(3.0, ACCENT));
    }
}

const fn rotation_label(rotation: DisplayRotation) -> &'static str {
    match rotation {
        DisplayRotation::Normal => "landscape",
        DisplayRotation::Clockwise90 => "90 degrees",
        DisplayRotation::Clockwise180 => "180 degrees",
        DisplayRotation::Clockwise270 => "270 degrees",
    }
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

fn snap_candidates(moved: ScreenGeometry, other: ScreenGeometry) -> [(i32, i32); 4] {
    let moved_width = i64::from(moved.width);
    let moved_height = i64::from(moved.height);
    let side_y = i64::from(moved.y).clamp(other.top() - moved_height + 1, other.bottom() - 1);
    let vertical_x = i64::from(moved.x).clamp(other.left() - moved_width + 1, other.right() - 1);
    [
        (
            saturating_i64_to_i32(other.left() - moved_width),
            saturating_i64_to_i32(side_y),
        ),
        (
            saturating_i64_to_i32(other.right()),
            saturating_i64_to_i32(side_y),
        ),
        (
            saturating_i64_to_i32(vertical_x),
            saturating_i64_to_i32(other.top() - moved_height),
        ),
        (
            saturating_i64_to_i32(vertical_x),
            saturating_i64_to_i32(other.bottom()),
        ),
    ]
}

fn geometries_are_connected(screens: &[ScreenGeometry]) -> bool {
    if screens.is_empty() {
        return false;
    }
    let mut visited = vec![0];
    let mut index = 0;
    while let Some(screen_index) = visited.get(index).copied() {
        index += 1;
        for (candidate_index, candidate) in screens.iter().enumerate() {
            if !visited.contains(&candidate_index) && screens[screen_index].shares_edge(*candidate)
            {
                visited.push(candidate_index);
            }
        }
    }
    visited.len() == screens.len()
}

fn alignment_axis(selected: ScreenGeometry, other: ScreenGeometry) -> Option<AlignmentAxis> {
    let vertical_overlap = selected.top() < other.bottom() && selected.bottom() > other.top();
    if (selected.left() == other.right() || selected.right() == other.left()) && vertical_overlap {
        return Some(AlignmentAxis::Vertical(0));
    }
    let horizontal_overlap = selected.left() < other.right() && selected.right() > other.left();
    if (selected.top() == other.bottom() || selected.bottom() == other.top()) && horizontal_overlap
    {
        return Some(AlignmentAxis::Horizontal(0));
    }
    None
}

fn align_coordinate(
    other_start: i64,
    other_length: u32,
    selected_length: u32,
    alignment: ScreenAlignment,
) -> i32 {
    let offset = match alignment {
        ScreenAlignment::Start => 0,
        ScreenAlignment::Center => (i64::from(other_length) - i64::from(selected_length)) / 2,
        ScreenAlignment::End => i64::from(other_length) - i64::from(selected_length),
    };
    saturating_i64_to_i32(other_start + offset)
}

fn saturating_i64_to_i32(value: i64) -> i32 {
    i32::try_from(value).unwrap_or_else(|_| {
        if value.is_negative() {
            i32::MIN
        } else {
            i32::MAX
        }
    })
}

fn saturating_f64_to_i32(value: f64) -> i32 {
    if value.is_nan() {
        0
    } else if value <= f64::from(i32::MIN) {
        i32::MIN
    } else if value >= f64::from(i32::MAX) {
        i32::MAX
    } else {
        value as i32
    }
}

fn desktop_geometry(frame: &eframe::Frame) -> Option<platform::DesktopGeometry> {
    let window = frame.winit_window()?;
    aggregate_monitor_geometry(window.available_monitors().map(|monitor| {
        let position = monitor.position();
        let size = monitor.size();
        MonitorGeometry {
            name: monitor.name(),
            x: position.x,
            y: position.y,
            width: size.width,
            height: size.height,
            scale_milli: positive_scale_milli(monitor.scale_factor()),
        }
    }))
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct MonitorGeometry {
    name: Option<String>,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    scale_milli: u32,
}

fn aggregate_monitor_geometry(
    monitors: impl IntoIterator<Item = MonitorGeometry>,
) -> Option<platform::DesktopGeometry> {
    let mut monitors = monitors.into_iter().collect::<Vec<_>>();
    monitors.sort_unstable();
    monitors.dedup();
    let first = monitors.first()?;
    let mut left = i64::from(first.x);
    let mut top = i64::from(first.y);
    let mut right = left + i64::from(first.width);
    let mut bottom = top + i64::from(first.height);
    for monitor in &monitors[1..] {
        left = left.min(i64::from(monitor.x));
        top = top.min(i64::from(monitor.y));
        right = right.max(i64::from(monitor.x) + i64::from(monitor.width));
        bottom = bottom.max(i64::from(monitor.y) + i64::from(monitor.height));
    }
    let width = u32::try_from(right.checked_sub(left)?).ok()?;
    let height = u32::try_from(bottom.checked_sub(top)?).ok()?;
    let size = Size::new(NonZeroU32::new(width)?, NonZeroU32::new(height)?);
    let layout = DesktopLayout::new(
        size,
        monitors
            .into_iter()
            .map(|monitor| {
                let mut display = Monitor::new(
                    monitor.name,
                    Rect::new(
                        Point {
                            x: i32::try_from(i64::from(monitor.x) - left).ok()?,
                            y: i32::try_from(i64::from(monitor.y) - top).ok()?,
                        },
                        Size::new(
                            NonZeroU32::new(monitor.width)?,
                            NonZeroU32::new(monitor.height)?,
                        ),
                    ),
                );
                display.scale_milli = NonZeroU32::new(monitor.scale_milli)?;
                display.rotation = DisplayRotation::Normal;
                Some(display)
            })
            .collect::<Option<Vec<_>>>()?,
    )
    .ok()?;
    Some(platform::DesktopGeometry {
        origin: Point {
            x: i32::try_from(left).ok()?,
            y: i32::try_from(top).ok()?,
        },
        layout,
    })
}

fn current_monitor_geometry(context: &egui::Context) -> Option<platform::DesktopGeometry> {
    context.input(|input| {
        let viewport = input.viewport();
        let size = viewport.monitor_size?;
        let scale = viewport.native_pixels_per_point.unwrap_or(1.0);
        let size = Size::new(
            NonZeroU32::new(positive_pixel_dimension(size.x * scale)?)?,
            NonZeroU32::new(positive_pixel_dimension(size.y * scale)?)?,
        );
        let mut monitor = Monitor::new(None, Rect::new(Point { x: 0, y: 0 }, size));
        monitor.scale_milli = NonZeroU32::new(positive_scale_milli(f64::from(scale)))?;
        Some(platform::DesktopGeometry {
            origin: Point { x: 0, y: 0 },
            layout: DesktopLayout::new(size, vec![monitor]).ok()?,
        })
    })
}

fn positive_pixel_dimension(value: f32) -> Option<u32> {
    (value.is_finite() && value >= 1.0 && value <= u32::MAX as f32).then(|| value.round() as u32)
}

fn positive_scale_milli(value: f64) -> u32 {
    if value.is_finite() {
        (value * 1000.0).round().clamp(1.0, f64::from(u32::MAX)) as u32
    } else {
        1000
    }
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
        Role::Controller {
            listen, topology, ..
        } => format!(
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

    use domain::{DesktopLayout, Monitor, NodeId, Point, Rect, ScreenPlacement, Size};
    use identity::IdentityStore;
    use tempfile::TempDir;

    use super::{
        ConfigEditor, ConfigRole, DesktopApp, MonitorGeometry, RemoteDisplay, RuntimeEvent,
        RuntimeRole, ScreenEditor, ScreenGeometry, SessionState, aggregate_monitor_geometry,
    };

    fn horizontal_layout(width: u32, height: u32, monitor_count: u32) -> DesktopLayout {
        let desktop_size = Size::new(
            NonZeroU32::new(width).unwrap_or(NonZeroU32::MIN),
            NonZeroU32::new(height).unwrap_or(NonZeroU32::MIN),
        );
        let monitor_width = width / monitor_count;
        let monitors = (0..monitor_count)
            .map(|index| {
                let left = monitor_width * index;
                let width = if index + 1 == monitor_count {
                    width - left
                } else {
                    monitor_width
                };
                Monitor::new(
                    Some(format!("Display {}", index + 1)),
                    Rect::new(
                        Point {
                            x: i32::try_from(left).unwrap_or(i32::MAX),
                            y: 0,
                        },
                        Size::new(
                            NonZeroU32::new(width).unwrap_or(NonZeroU32::MIN),
                            NonZeroU32::new(height).unwrap_or(NonZeroU32::MIN),
                        ),
                    ),
                )
            })
            .collect();
        DesktopLayout::new(desktop_size, monitors)
            .unwrap_or_else(|error| panic!("test desktop layout should be valid: {error}"))
    }

    #[test]
    fn desktop_geometry_deduplicates_wayland_output_handles() {
        let monitor = |name: &str, x| MonitorGeometry {
            name: Some(name.to_owned()),
            x,
            y: 0,
            width: 1920,
            height: 1080,
            scale_milli: 1000,
        };
        let geometry = aggregate_monitor_geometry([
            monitor("HDMI-A-1", 0),
            monitor("DP-1", 1920),
            monitor("DP-2", 3840),
            monitor("HDMI-A-1", 0),
            monitor("DP-1", 1920),
            monitor("DP-2", 3840),
        ])
        .unwrap_or_else(|| panic!("monitor geometry should aggregate"));

        assert_eq!(geometry.origin, Point { x: 0, y: 0 });
        assert_eq!(geometry.size().width.get(), 5760);
        assert_eq!(geometry.size().height.get(), 1080);
        assert_eq!(geometry.monitor_count(), 3);
    }

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
                    layout: horizontal_layout(5_760, 1_080, 3),
                },
            )
            .unwrap_or_else(|error| panic!("monitor dimensions should apply: {error}"));

        assert_eq!(editor.selected_screen, 0);
        assert_eq!(
            editor.screens[0].geometry(),
            Some(ScreenGeometry {
                x: 0,
                y: 0,
                width: 5_760,
                height: 1_080,
            })
        );
        assert_eq!(editor.screens[0].layout.monitor_count(), 3);
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
            layout: horizontal_layout(5_760, 1_080, 3),
        };

        app.config_editor.role = ConfigRole::Agent;
        app.apply_local_desktop_to_editor(geometry)
            .unwrap_or_else(|error| panic!("desktop geometry should apply: {error}"));

        assert_eq!(app.config_editor.agent_width, "5760");
        assert_eq!(app.config_editor.agent_height, "1080");
        assert_eq!(app.config_editor.agent_layout.monitor_count(), 3);
    }

    #[test]
    fn reported_display_updates_live_state_and_the_visual_editor() {
        let local = NodeId::new("studio-left")
            .unwrap_or_else(|error| panic!("test node should be valid: {error}"));
        let remote = NodeId::new("studio-right")
            .unwrap_or_else(|error| panic!("test node should be valid: {error}"));
        let mut editor = ConfigEditor::for_node(Some(&local));
        editor.add_screen(remote.to_string());
        let screen = ScreenPlacement::with_layout(
            remote.clone(),
            Point { x: 1920, y: -180 },
            horizontal_layout(2560, 1440, 2),
        )
        .unwrap_or_else(|error| panic!("screen layout should be valid: {error}"));
        let mut state = SessionState::default();

        editor.update_screen(&screen);
        state.apply(RuntimeEvent::DisplayChanged {
            screen: screen.clone(),
        });

        assert_eq!(
            state.displays.get(&remote),
            Some(&RemoteDisplay {
                size: screen.bounds.size,
                monitor_count: 2,
            })
        );
        assert_eq!(editor.selected_screen, 1);
        assert_eq!(
            editor.screens[1].geometry(),
            Some(ScreenGeometry {
                x: 1920,
                y: -180,
                width: 2560,
                height: 1440,
            })
        );
        assert_eq!(editor.screens[1].layout.monitor_count(), 2);
    }

    #[test]
    fn configuration_editor_snaps_machines_to_neighboring_edges() {
        let local = NodeId::new("studio-left")
            .unwrap_or_else(|error| panic!("test node should be valid: {error}"));
        let mut editor = ConfigEditor::for_node(Some(&local));
        editor.add_screen(String::from("studio-right"));

        editor.move_screen(1, 1500, 120);
        editor.snap_screen(1);

        assert_eq!(
            editor.screens[1].geometry(),
            Some(ScreenGeometry {
                x: 1920,
                y: 120,
                width: 1920,
                height: 1080,
            })
        );
        editor
            .build(local)
            .unwrap_or_else(|error| panic!("topology should be valid: {error}"));
    }

    #[test]
    fn configuration_editor_centers_a_taller_neighbor() {
        let local = NodeId::new("studio-left")
            .unwrap_or_else(|error| panic!("test node should be valid: {error}"));
        let mut editor = ConfigEditor::for_node(Some(&local));
        editor.add_screen(String::from("studio-right"));
        editor.resize_screen(1, 2560, 1440);

        assert_eq!(
            editor.screens[1].geometry(),
            Some(ScreenGeometry {
                x: 1920,
                y: -180,
                width: 2560,
                height: 1440,
            })
        );
        editor.align_screen(1, super::ScreenAlignment::Start);
        assert_eq!(editor.screens[1].y, "0");
    }

    #[test]
    fn screen_geometry_rejects_zero_dimensions() {
        let screen = ScreenEditor {
            node: String::from("studio-left"),
            x: String::from("0"),
            y: String::from("0"),
            width: String::from("0"),
            height: String::from("1080"),
            layout: horizontal_layout(1920, 1080, 1),
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
