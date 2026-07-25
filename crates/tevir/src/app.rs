use std::path::{Path, PathBuf};

use domain::NodeId;
use eframe::egui::{
    self, Align, Button, Color32, CornerRadius, FontFamily, FontId, Frame, Layout, Margin,
    RichText, ScrollArea, Sense, Stroke, TextEdit, TextStyle, Ui, Vec2, ViewportBuilder,
};
use identity::{IdentityStore, LocalIdentity, PairingBundle, TrustStore};
use platform::{EnvironmentStatus, PlatformReport};

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
    Pairing,
    Diagnostics,
}

impl Page {
    const ALL: [(Self, &'static str); 3] = [
        (Self::Status, "Status"),
        (Self::Pairing, "Pairing"),
        (Self::Diagnostics, "Diagnostics"),
    ];
}

pub struct DesktopApp {
    data_directory: PathBuf,
    settings: DesktopSettings,
    identity: Option<LocalIdentity>,
    trust: Option<TrustStore>,
    page: Page,
    node_input: String,
    pairing_bundle_input: String,
    pairing_code_input: String,
    config_path_input: String,
    report: PlatformReport,
    notice: Option<Notice>,
    config_summary: Option<String>,
    confirm_remove: Option<NodeId>,
}

impl DesktopApp {
    pub fn load(data_directory: PathBuf, node_override: Option<NodeId>) -> Result<Self, AppError> {
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
                    Some(Notice::error(format!("Identity unavailable: {error}"))),
                ),
            }
        } else {
            (None, None, None)
        };

        Ok(Self {
            data_directory,
            node_input: settings
                .node
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            settings,
            identity,
            trust,
            page: initial_page(),
            pairing_bundle_input: String::new(),
            pairing_code_input: String::new(),
            config_path_input: String::new(),
            report: platform::probe_host(),
            notice,
            config_summary: None,
            confirm_remove: None,
        })
    }

    fn create_identity(&mut self) {
        let node = match NodeId::new(self.node_input.trim()) {
            Ok(node) => node,
            Err(error) => {
                self.notice = Some(Notice::error(error.to_string()));
                return;
            }
        };
        match load_identity(&self.data_directory, &node) {
            Ok((identity, trust)) => {
                self.settings.node = Some(node);
                if let Err(error) = self.settings.save(&self.data_directory) {
                    self.notice = Some(Notice::error(error.to_string()));
                    return;
                }
                self.identity = Some(identity);
                self.trust = Some(trust);
                self.notice = Some(Notice::success("Local identity ready"));
            }
            Err(error) => self.notice = Some(Notice::error(error)),
        }
    }

    fn import_pairing(&mut self) {
        let bundle = match PairingBundle::decode(&self.pairing_bundle_input) {
            Ok(bundle) => bundle,
            Err(error) => {
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
                self.pairing_bundle_input.clear();
                self.pairing_code_input.clear();
                self.notice = Some(Notice::success(format!("Paired with {node}")));
            }
            Err(error) => self.notice = Some(Notice::error(error.to_string())),
        }
    }

    fn remove_peer(&mut self, node: &NodeId) {
        let Some(trust) = self.trust.as_mut() else {
            return;
        };
        match trust.remove(node) {
            Ok(true) => self.notice = Some(Notice::success(format!("Removed {node}"))),
            Ok(false) => self.notice = Some(Notice::error(format!("{node} is not paired"))),
            Err(error) => self.notice = Some(Notice::error(error.to_string())),
        }
        self.confirm_remove = None;
    }

    fn validate_config(&mut self) {
        let path = Path::new(self.config_path_input.trim());
        match Config::load(path) {
            Ok(config) => {
                let summary = match config.role {
                    Role::Controller { listen, topology } => format!(
                        "Controller {} | {listen} | {} screens",
                        config.node,
                        topology.screens().len()
                    ),
                    Role::Agent { controller } => {
                        format!("Agent {} | controller {controller}", config.node)
                    }
                };
                self.config_summary = Some(summary);
                self.notice = Some(Notice::success("Configuration valid"));
            }
            Err(error) => {
                self.config_summary = None;
                self.notice = Some(Notice::error(error.to_string()));
            }
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
                        TextEdit::singleline(&mut self.node_input)
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
                        Page::Pairing => "Pairing",
                        Page::Diagnostics => "Diagnostics",
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
                    Page::Pairing => self.pairing_view(ui),
                    Page::Diagnostics => self.diagnostics_view(ui),
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
        metric_row(ui, "Transport", "TLS 1.3 / QUIC", ACCENT);

        ui.add_space(30.0);
        section_heading(ui, "Configuration", "Validate a controller or agent file");
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            let available = (ui.available_width() - 110.0).max(160.0);
            ui.add_sized(
                [available, 34.0],
                TextEdit::singleline(&mut self.config_path_input).hint_text("Configuration path"),
            );
            if ui
                .add_sized([96.0, 34.0], Button::new("Validate"))
                .clicked()
            {
                self.validate_config();
            }
        });
        if let Some(summary) = self.config_summary.as_ref() {
            ui.add_space(10.0);
            ui.label(RichText::new(summary).color(SUCCESS));
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
                TextEdit::singleline(&mut self.pairing_code_input)
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

    fn peer_count(&self) -> usize {
        self.trust.as_ref().map_or(0, |trust| trust.peers().len())
    }
}

impl eframe::App for DesktopApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        if self.identity.is_none() {
            self.setup_view(ui);
            return;
        }
        self.navigation(ui);
        self.top_bar(ui);
        self.content(ui);
    }
}

pub fn run(data_directory: PathBuf, node: Option<NodeId>) -> Result<(), AppError> {
    let app = DesktopApp::load(data_directory, node)?;
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

fn format_fingerprint(fingerprint: [u8; 32]) -> String {
    fingerprint[..12]
        .chunks_exact(2)
        .map(|chunk| format!("{:02X}{:02X}", chunk[0], chunk[1]))
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(feature = "screenshot-tests")]
fn initial_page() -> Page {
    match std::env::var("TEVIR_SCREENSHOT_PAGE").as_deref() {
        Ok("pairing") => Page::Pairing,
        Ok("diagnostics") => Page::Diagnostics,
        _ => Page::Status,
    }
}

#[cfg(not(feature = "screenshot-tests"))]
const fn initial_page() -> Page {
    Page::Status
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

    use super::DesktopApp;

    #[test]
    fn node_override_initializes_the_desktop_identity() {
        let directory =
            TempDir::new().unwrap_or_else(|error| panic!("temp directory failed: {error}"));
        let node = NodeId::new("studio-left")
            .unwrap_or_else(|error| panic!("test node should be valid: {error}"));
        let app = DesktopApp::load(directory.path().to_path_buf(), Some(node.clone()))
            .unwrap_or_else(|error| panic!("desktop initialization failed: {error}"));

        assert_eq!(app.settings.node.as_ref(), Some(&node));
        assert_eq!(
            app.identity.as_ref().map(|identity| identity.node()),
            Some(&node)
        );
        assert!(app.trust.is_some());
    }
}
