use std::{
    net::SocketAddr,
    num::NonZeroU32,
    path::{Path, PathBuf},
    time::Duration,
};

use discovery::{DiscoveredNode, DiscoveryService, NearbyNodes};
use domain::{NodeId, Point, Rect, ScreenPlacement, Size, Topology};
use eframe::egui::{
    self, Align, Button, Color32, CornerRadius, FontFamily, FontId, Frame, Layout, Margin,
    RichText, ScrollArea, Sense, Stroke, TextEdit, TextStyle, Ui, Vec2, ViewportBuilder,
};
use identity::{IdentityStore, LocalIdentity, PairingBundle, TrustStore};
use platform::{EnvironmentStatus, PlatformReport};
use protocol::Capabilities;
use telemetry::{LogBuffer, LogEntry, LogLevel};

use crate::{
    config::{Config, Role},
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
    report: PlatformReport,
    notice: Option<Notice>,
    config_summary: Option<String>,
    confirm_remove: Option<NodeId>,
    logs: LogBuffer,
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
            report,
            notice,
            config_summary: None,
            confirm_remove: None,
            logs,
        };
        app.start_discovery();
        if load_saved_config && app.identity.is_some() {
            app.load_config();
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
                self.notice = Some(Notice::success(format!("Paired with {node}")));
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
        let path = PathBuf::from(self.config_path_input.trim());
        match Config::load(&path) {
            Ok(config) => {
                let Some(local_node) = self.identity.as_ref().map(LocalIdentity::node) else {
                    self.notice = Some(Notice::error("Local identity is not ready"));
                    return;
                };
                if &config.node != local_node {
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
                self.config_summary = Some(config_summary(&config));
                self.remember_config_path(&path);
                tracing::info!(path = %path.display(), "configuration loaded");
                self.notice = Some(Notice::success("Configuration loaded"));
            }
            Err(error) => {
                self.config_summary = None;
                tracing::warn!(path = %path.display(), error = %error, "configuration load failed");
                self.notice = Some(Notice::error(error.to_string()));
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
                self.remember_config_path(&path);
                tracing::info!(path = %path.display(), "configuration saved");
                self.notice = Some(Notice::success("Configuration saved"));
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
                        let ready = self.report.is_available() && self.peer_count() > 0;
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
        metric_row(ui, "Transport", "TLS 1.3 / QUIC", ACCENT);

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
        ui.horizontal(|ui| {
            ui.selectable_value(
                &mut self.config_editor.role,
                ConfigRole::Controller,
                "Controller",
            );
            ui.selectable_value(&mut self.config_editor.role, ConfigRole::Agent, "Agent");
        });

        ui.add_space(22.0);
        match self.config_editor.role {
            ConfigRole::Controller => self.controller_configuration(ui),
            ConfigRole::Agent => {
                section_heading(ui, "Controller endpoint", "IP address and port");
                ui.add_space(10.0);
                labeled_text_field(
                    ui,
                    "Controller address",
                    &mut self.config_editor.controller_address,
                    "192.0.2.10:24800",
                );
            }
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
        section_heading(
            ui,
            "Screen topology",
            &format!("{} screens", self.config_editor.screens.len()),
        );
        ui.add_space(10.0);

        let mut remove = None;
        let can_remove_screen = self.config_editor.screens.len() > 1;
        for (index, screen) in self.config_editor.screens.iter_mut().enumerate() {
            Frame::new()
                .fill(ELEVATED)
                .stroke(Stroke::new(1.0, BORDER))
                .corner_radius(CornerRadius::same(5))
                .inner_margin(Margin::symmetric(14, 12))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("Screen {}", index + 1))
                                .strong()
                                .color(TEXT),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui
                                .add_enabled(can_remove_screen, Button::new("Remove"))
                                .clicked()
                            {
                                remove = Some(index);
                            }
                        });
                    });
                    ui.add_space(8.0);
                    labeled_text_field(ui, "Node", &mut screen.node, "node-id");
                    ui.add_space(8.0);
                    ui.columns(4, |columns| {
                        compact_text_field(&mut columns[0], "X", &mut screen.x, "0");
                        compact_text_field(&mut columns[1], "Y", &mut screen.y, "0");
                        compact_text_field(&mut columns[2], "Width", &mut screen.width, "1920");
                        compact_text_field(&mut columns[3], "Height", &mut screen.height, "1080");
                    });
                });
            ui.add_space(8.0);
        }
        if let Some(index) = remove {
            self.config_editor.screens.remove(index);
        }

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
        if ui.button("Add screen").clicked() {
            self.config_editor.add_screen(suggested_node);
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
            &format!("{} paired", self.peer_count()),
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
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.poll_discovery();
        if self.identity.is_none() {
            self.setup_view(ui);
            return;
        }
        self.navigation(ui);
        self.top_bar(ui);
        self.content(ui);
        ui.ctx().request_repaint_after(if self.page == Page::Logs {
            Duration::from_millis(500)
        } else {
            Duration::from_secs(1)
        });
    }
}

pub fn run(data_directory: PathBuf, node: Option<NodeId>, logs: LogBuffer) -> Result<(), AppError> {
    let app = DesktopApp::load(data_directory, node, logs)?;
    let options = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_title("Tevir")
            .with_app_id("tevir")
            .with_inner_size([980.0, 680.0])
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
        .map(ToString::to_string)
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfigRole {
    Controller,
    Agent,
}

struct ConfigEditor {
    role: ConfigRole,
    listen_address: String,
    controller_address: String,
    screens: Vec<ScreenEditor>,
}

impl ConfigEditor {
    fn for_node(node: Option<&NodeId>) -> Self {
        Self {
            role: ConfigRole::Controller,
            listen_address: String::from("0.0.0.0:24800"),
            controller_address: String::from("127.0.0.1:24800"),
            screens: vec![ScreenEditor {
                node: node.map_or_else(|| String::from("local-node"), ToString::to_string),
                x: String::from("0"),
                y: String::from("0"),
                width: String::from("1920"),
                height: String::from("1080"),
            }],
        }
    }

    fn from_config(config: &Config) -> Self {
        match &config.role {
            Role::Controller { listen, topology } => Self {
                role: ConfigRole::Controller,
                listen_address: listen.to_string(),
                controller_address: String::from("127.0.0.1:24800"),
                screens: topology
                    .screens()
                    .iter()
                    .map(ScreenEditor::from_placement)
                    .collect(),
            },
            Role::Agent { controller } => Self {
                role: ConfigRole::Agent,
                listen_address: String::from("0.0.0.0:24800"),
                controller_address: controller.to_string(),
                screens: vec![ScreenEditor::from_local_node(&config.node)],
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
            ConfigRole::Agent => Role::Agent {
                controller: parse_socket_address("Controller address", &self.controller_address)?,
            },
        };
        Config::new(node, role).map_err(|error| error.to_string())
    }

    fn contains_node(&self, node: &NodeId) -> bool {
        self.screens
            .iter()
            .any(|screen| screen.node.trim() == node.as_str())
    }

    fn add_screen(&mut self, node: String) {
        let x = self
            .screens
            .iter()
            .filter_map(|screen| {
                let x = screen.x.parse::<i64>().ok()?;
                let width = screen.width.parse::<i64>().ok()?;
                Some(x.saturating_add(width))
            })
            .max()
            .and_then(|x| i32::try_from(x).ok())
            .unwrap_or(1920);
        self.screens.push(ScreenEditor {
            node,
            x: x.to_string(),
            y: String::from("0"),
            width: String::from("1920"),
            height: String::from("1080"),
        });
    }
}

struct ScreenEditor {
    node: String,
    x: String,
    y: String,
    width: String,
    height: String,
}

impl ScreenEditor {
    fn from_local_node(node: &NodeId) -> Self {
        Self {
            node: node.to_string(),
            x: String::from("0"),
            y: String::from("0"),
            width: String::from("1920"),
            height: String::from("1080"),
        }
    }

    fn from_placement(placement: &ScreenPlacement) -> Self {
        Self {
            node: placement.node.to_string(),
            x: placement.bounds.origin.x.to_string(),
            y: placement.bounds.origin.y.to_string(),
            width: placement.bounds.size.width.to_string(),
            height: placement.bounds.size.height.to_string(),
        }
    }

    fn build(&self, index: usize) -> Result<ScreenPlacement, String> {
        let number = index + 1;
        let node = NodeId::new(self.node.trim())
            .map_err(|error| format!("Screen {number} node: {error}"))?;
        let x = parse_i32(&format!("Screen {number} X"), &self.x)?;
        let y = parse_i32(&format!("Screen {number} Y"), &self.y)?;
        let width = parse_nonzero(&format!("Screen {number} width"), &self.width)?;
        let height = parse_nonzero(&format!("Screen {number} height"), &self.height)?;
        Ok(ScreenPlacement {
            node,
            bounds: Rect::new(Point { x, y }, Size::new(width, height)),
        })
    }
}

fn parse_socket_address(label: &str, value: &str) -> Result<SocketAddr, String> {
    value
        .trim()
        .parse()
        .map_err(|error| format!("{label}: {error}"))
}

fn parse_i32(label: &str, value: &str) -> Result<i32, String> {
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
        Role::Agent { controller } => {
            format!("Agent {} | controller {controller}", config.node)
        }
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
    use domain::NodeId;
    use tempfile::TempDir;

    use super::{ConfigEditor, ConfigRole, DesktopApp};

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
}
