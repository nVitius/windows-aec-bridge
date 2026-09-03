use std::collections::VecDeque;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::bridge::{self, BridgeEvent, BridgeHandle};
use crate::devices::{self, EndpointCatalog, EndpointDescriptor};
use crate::startup::{self, LoginStartupStatus};
#[cfg(target_os = "windows")]
use crate::tray::{TrayAction, TrayController};
use crate::{EndpointArgs, ReferenceMode, RunArgs};

const STORAGE_KEY: &str = "aec_bridge_settings";
const DEBUG_LOG_LIMIT: usize = 500;
const LOGIN_AUTO_START_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Clone, Copy, Debug, Default)]
pub struct GuiLaunchOptions {
    pub login_startup: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
struct SavedEndpoint {
    id: String,
    last_name: String,
}

impl SavedEndpoint {
    fn from_descriptor(endpoint: &EndpointDescriptor) -> Self {
        Self {
            id: endpoint.id.clone(),
            last_name: endpoint.name.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default)]
struct PersistedSettings {
    microphone: Option<SavedEndpoint>,
    reference: Option<SavedEndpoint>,
    reference_mode: ReferenceMode,
    handoff_render: Option<SavedEndpoint>,
    downstream_capture: Option<SavedEndpoint>,
    capture_delay_ms: u32,
    stream_delay_ms: i32,
    dark_mode: bool,
    approved_handoff_id: Option<String>,
    auto_start_bridge: bool,
    minimize_to_tray: bool,
}

impl Default for PersistedSettings {
    fn default() -> Self {
        Self {
            microphone: None,
            reference: None,
            reference_mode: ReferenceMode::Loopback,
            handoff_render: None,
            downstream_capture: None,
            capture_delay_ms: 20,
            stream_delay_ms: 0,
            dark_mode: true,
            approved_handoff_id: None,
            auto_start_bridge: false,
            minimize_to_tray: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StartOrigin {
    Manual,
    Login,
}

#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TrayEnableOrigin {
    SavedPreference,
    User,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WindowVisibilityAction {
    None,
    HideToTray,
    Restore,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LoginStartupWindowAction {
    HideToTray,
    MinimizeToTaskbar,
}

fn login_startup_window_action(tray_available: bool) -> LoginStartupWindowAction {
    if tray_available {
        LoginStartupWindowAction::HideToTray
    } else {
        LoginStartupWindowAction::MinimizeToTaskbar
    }
}

fn restore_after_fatal_bridge_error(completed_automatic_start: bool, hidden_to_tray: bool) -> bool {
    completed_automatic_start || hidden_to_tray
}

fn window_visibility_action(
    quitting: bool,
    restore_requested: bool,
    minimize_to_tray: bool,
    tray_available: bool,
    hidden_to_tray: bool,
    minimized: bool,
    previously_minimized: bool,
) -> WindowVisibilityAction {
    if quitting {
        WindowVisibilityAction::None
    } else if restore_requested {
        WindowVisibilityAction::Restore
    } else if minimize_to_tray
        && tray_available
        && !hidden_to_tray
        && minimized
        && !previously_minimized
    {
        WindowVisibilityAction::HideToTray
    } else {
        WindowVisibilityAction::None
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum StartReadiness {
    Ready,
    Waiting(String),
    Blocked(String),
}

#[derive(Debug)]
enum LoginAutoStartState {
    Inactive,
    Waiting {
        deadline: Instant,
        next_retry: Instant,
        attempt: u8,
        last_issue: Option<String>,
    },
    Starting {
        deadline: Instant,
        attempt: u8,
    },
    Cancelling,
    Complete,
    Cancelled,
    Failed(String),
}

impl LoginAutoStartState {
    fn waiting() -> Self {
        let now = Instant::now();
        Self::Waiting {
            deadline: now + LOGIN_AUTO_START_TIMEOUT,
            next_retry: now + Duration::from_secs(1),
            attempt: 0,
            last_issue: None,
        }
    }

    fn is_waiting(&self) -> bool {
        matches!(self, Self::Waiting { .. })
    }
}

#[derive(Clone, Copy)]
struct Palette {
    background: egui::Color32,
    surface: egui::Color32,
    surface_raised: egui::Color32,
    field: egui::Color32,
    border: egui::Color32,
    text: egui::Color32,
    muted: egui::Color32,
    accent: egui::Color32,
    accent_hover: egui::Color32,
    accent_soft: egui::Color32,
    success: egui::Color32,
    success_soft: egui::Color32,
    warning: egui::Color32,
    warning_soft: egui::Color32,
    danger: egui::Color32,
    danger_soft: egui::Color32,
}

fn palette(dark_mode: bool) -> Palette {
    if dark_mode {
        Palette {
            background: egui::Color32::from_rgb(12, 17, 23),
            surface: egui::Color32::from_rgb(20, 27, 36),
            surface_raised: egui::Color32::from_rgb(26, 35, 46),
            field: egui::Color32::from_rgb(15, 21, 29),
            border: egui::Color32::from_rgb(43, 55, 70),
            text: egui::Color32::from_rgb(237, 243, 249),
            muted: egui::Color32::from_rgb(145, 160, 179),
            accent: egui::Color32::from_rgb(86, 180, 255),
            accent_hover: egui::Color32::from_rgb(123, 197, 255),
            accent_soft: egui::Color32::from_rgb(24, 49, 72),
            success: egui::Color32::from_rgb(82, 209, 144),
            success_soft: egui::Color32::from_rgb(23, 57, 45),
            warning: egui::Color32::from_rgb(243, 183, 91),
            warning_soft: egui::Color32::from_rgb(61, 45, 24),
            danger: egui::Color32::from_rgb(255, 107, 120),
            danger_soft: egui::Color32::from_rgb(67, 30, 37),
        }
    } else {
        Palette {
            background: egui::Color32::from_rgb(244, 247, 250),
            surface: egui::Color32::WHITE,
            surface_raised: egui::Color32::from_rgb(248, 250, 252),
            field: egui::Color32::from_rgb(241, 245, 249),
            border: egui::Color32::from_rgb(216, 224, 233),
            text: egui::Color32::from_rgb(28, 37, 49),
            muted: egui::Color32::from_rgb(101, 115, 133),
            accent: egui::Color32::from_rgb(32, 132, 230),
            accent_hover: egui::Color32::from_rgb(25, 112, 204),
            accent_soft: egui::Color32::from_rgb(224, 240, 255),
            success: egui::Color32::from_rgb(38, 158, 98),
            success_soft: egui::Color32::from_rgb(228, 247, 237),
            warning: egui::Color32::from_rgb(201, 131, 31),
            warning_soft: egui::Color32::from_rgb(255, 246, 226),
            danger: egui::Color32::from_rgb(214, 67, 78),
            danger_soft: egui::Color32::from_rgb(255, 235, 237),
        }
    }
}

fn apply_theme(context: &egui::Context, dark_mode: bool) {
    let colors = palette(dark_mode);
    let theme = egui::Theme::from_dark_mode(dark_mode);
    context.set_theme(theme);
    let mut style = (*context.style_of(theme)).clone();
    let mut visuals = if dark_mode {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    visuals.panel_fill = colors.background;
    visuals.window_fill = colors.surface;
    visuals.window_stroke = egui::Stroke::new(1.0, colors.border);
    visuals.window_corner_radius = 12.into();
    visuals.menu_corner_radius = 10.into();
    visuals.faint_bg_color = colors.surface_raised;
    visuals.extreme_bg_color = colors.field;
    visuals.text_edit_bg_color = Some(colors.field);
    visuals.code_bg_color = colors.field;
    visuals.weak_text_color = Some(colors.muted);
    visuals.hyperlink_color = colors.accent;
    visuals.warn_fg_color = colors.warning;
    visuals.error_fg_color = colors.danger;
    visuals.selection.bg_fill = colors.accent_soft;
    visuals.selection.stroke = egui::Stroke::new(1.0, colors.accent);
    visuals.interact_cursor = Some(egui::CursorIcon::PointingHand);
    visuals.collapsing_header_frame = false;
    visuals.indent_has_left_vline = false;

    visuals.widgets.noninteractive.bg_fill = colors.surface;
    visuals.widgets.noninteractive.weak_bg_fill = colors.surface;
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, colors.border);
    visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, colors.text);
    visuals.widgets.noninteractive.corner_radius = 8.into();

    visuals.widgets.inactive.bg_fill = colors.field;
    visuals.widgets.inactive.weak_bg_fill = colors.field;
    visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, colors.border);
    visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, colors.text);
    visuals.widgets.inactive.corner_radius = 8.into();

    visuals.widgets.hovered.bg_fill = colors.surface_raised;
    visuals.widgets.hovered.weak_bg_fill = colors.surface_raised;
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, colors.accent);
    visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, colors.accent_hover);
    visuals.widgets.hovered.corner_radius = 8.into();

    visuals.widgets.active.bg_fill = colors.accent_soft;
    visuals.widgets.active.weak_bg_fill = colors.accent_soft;
    visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, colors.accent);
    visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0, colors.accent);
    visuals.widgets.active.corner_radius = 8.into();
    visuals.widgets.open = visuals.widgets.hovered;

    style.visuals = visuals;
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(14.0, 8.0);
    style.spacing.interact_size = egui::vec2(40.0, 36.0);
    style.spacing.combo_width = 420.0;
    style.spacing.tooltip_width = 360.0;
    style.text_styles.insert(
        egui::TextStyle::Heading,
        egui::FontId::new(26.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Body,
        egui::FontId::new(14.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Button,
        egui::FontId::new(14.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Small,
        egui::FontId::new(12.0, egui::FontFamily::Proportional),
    );
    context.set_style_of(theme, style);
}

pub fn run(launch_options: GuiLaunchOptions) -> eframe::Result {
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([840.0, 780.0])
            .with_min_inner_size([680.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        "AEC Bridge",
        options,
        Box::new(move |context| Ok(Box::new(AecBridgeApp::new(context, launch_options)))),
    )
}

struct AecBridgeApp {
    settings: PersistedSettings,
    catalog: EndpointCatalog,
    refresh_rx: Option<mpsc::Receiver<Result<EndpointCatalog, String>>>,
    bridge: Option<BridgeHandle>,
    status: String,
    error: Option<String>,
    metrics: Option<String>,
    started_streams: Vec<String>,
    bypass: bool,
    mute_output: bool,
    launch_options: GuiLaunchOptions,
    login_auto_start: LoginAutoStartState,
    restore_window: bool,
    window_hidden_to_tray: bool,
    last_window_minimized: bool,
    quit_requested: bool,
    #[cfg(target_os = "windows")]
    tray: Option<TrayController>,
    tray_error: Option<String>,
    login_startup_status: Option<LoginStartupStatus>,
    startup_error: Option<String>,
    startup_notice: Option<String>,
    session_started: Instant,
    diagnostic_command: String,
    debug_log: VecDeque<String>,
}

impl AecBridgeApp {
    fn new(context: &eframe::CreationContext<'_>, launch_options: GuiLaunchOptions) -> Self {
        let settings: PersistedSettings = context
            .storage
            .and_then(|storage| eframe::get_value(storage, STORAGE_KEY))
            .unwrap_or_default();
        let (login_startup_status, startup_error) = match startup::query_login_startup() {
            Ok(status) => (Some(status), None),
            Err(error) => (
                None,
                Some(format!(
                    "Could not read Windows startup registration: {error:#}"
                )),
            ),
        };
        apply_theme(&context.egui_ctx, settings.dark_mode);
        let should_auto_start = launch_options.login_startup && settings.auto_start_bridge;
        let mut app = Self {
            settings,
            catalog: EndpointCatalog::default(),
            refresh_rx: None,
            bridge: None,
            status: "Idle".to_owned(),
            error: None,
            metrics: None,
            started_streams: Vec::new(),
            bypass: false,
            mute_output: false,
            launch_options,
            login_auto_start: if should_auto_start {
                LoginAutoStartState::waiting()
            } else {
                LoginAutoStartState::Inactive
            },
            restore_window: false,
            window_hidden_to_tray: false,
            last_window_minimized: false,
            quit_requested: false,
            #[cfg(target_os = "windows")]
            tray: None,
            tray_error: None,
            login_startup_status,
            startup_error,
            startup_notice: None,
            session_started: Instant::now(),
            diagnostic_command: String::new(),
            debug_log: VecDeque::new(),
        };
        if launch_options.login_startup {
            app.push_log("AEC Bridge opened by Windows sign-in startup.");
        } else {
            app.push_log("AEC Bridge opened.");
        }
        if let Some(error) = app.startup_error.clone() {
            app.push_log(format!("ERROR: {error}"));
        }
        #[cfg(target_os = "windows")]
        if app.settings.minimize_to_tray {
            app.set_tray_enabled(true, TrayEnableOrigin::SavedPreference);
        }
        app.request_refresh();
        if should_auto_start {
            if let Some(problem) = app.automatic_start_configuration_problem() {
                let message = format!("Automatic start is unavailable: {problem}");
                app.push_log(format!("ERROR: {message}"));
                app.status = "Automatic start unavailable".to_owned();
                app.error = Some(message.clone());
                app.login_auto_start = LoginAutoStartState::Failed(message);
            } else {
                app.status = "Waiting for saved audio devices".to_owned();
                app.push_log("Automatic start armed; waiting for the exact saved audio endpoints.");
                match login_startup_window_action(app.tray_available()) {
                    LoginStartupWindowAction::HideToTray => app.hide_window_to_tray(
                        &context.egui_ctx,
                        "Windows sign-in startup is continuing in the notification area.",
                    ),
                    LoginStartupWindowAction::MinimizeToTaskbar => context
                        .egui_ctx
                        .send_viewport_cmd(egui::ViewportCommand::Minimized(true)),
                }
            }
        }
        app
    }

    fn push_log(&mut self, message: impl Into<String>) {
        let elapsed = self.session_started.elapsed().as_secs_f64();
        self.debug_log
            .push_back(format!("[+{elapsed:>8.3}s] {}", message.into()));
        while self.debug_log.len() > DEBUG_LOG_LIMIT {
            self.debug_log.pop_front();
        }
    }

    fn tray_available(&self) -> bool {
        #[cfg(target_os = "windows")]
        {
            self.tray.is_some()
        }
        #[cfg(not(target_os = "windows"))]
        {
            false
        }
    }

    #[cfg(target_os = "windows")]
    fn set_tray_enabled(&mut self, enabled: bool, origin: TrayEnableOrigin) {
        self.tray_error = None;
        if !enabled {
            if self.window_hidden_to_tray {
                self.restore_window = true;
            }
            self.settings.minimize_to_tray = false;
            if self.tray.take().is_some() {
                self.push_log("Notification-area mode disabled.");
            }
            return;
        }

        if self.tray.is_some() {
            self.settings.minimize_to_tray = true;
            return;
        }

        match TrayController::create(&self.status) {
            Ok(tray) => {
                self.tray = Some(tray);
                self.settings.minimize_to_tray = true;
                self.push_log("Notification-area mode enabled.");
            }
            Err(error) => {
                let preserve_preference = origin == TrayEnableOrigin::SavedPreference;
                let fallback = if preserve_preference {
                    "This session will minimize to the taskbar; the saved preference was kept so the app can try again next launch."
                } else {
                    "The window will continue to minimize to the taskbar."
                };
                let message =
                    format!("Could not enable notification-area mode: {error:#}. {fallback}");
                self.settings.minimize_to_tray = preserve_preference;
                self.tray_error = Some(message.clone());
                self.push_log(format!("ERROR: {message}"));
            }
        }
    }

    fn hide_window_to_tray(&mut self, context: &egui::Context, log_message: &str) {
        if !self.tray_available() {
            return;
        }
        if self.window_hidden_to_tray {
            return;
        }

        context.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        self.window_hidden_to_tray = true;
        self.push_log(log_message);
    }

    fn restore_main_window(&mut self, context: &egui::Context) {
        context.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        context.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        context.send_viewport_cmd(egui::ViewportCommand::Focus);
        self.window_hidden_to_tray = false;
        self.restore_window = false;
    }

    #[cfg(target_os = "windows")]
    fn poll_tray_actions(&mut self, context: &egui::Context) -> bool {
        if self.quit_requested {
            return true;
        }
        let actions = self
            .tray
            .as_ref()
            .map(TrayController::poll_actions)
            .unwrap_or_default();
        let mut quit = false;
        for action in actions {
            match action {
                TrayAction::Open => {
                    self.restore_window = true;
                }
                TrayAction::Quit => quit = true,
            }
        }

        if quit {
            self.quit_requested = true;
            self.restore_window = false;
            if let Some(bridge) = &self.bridge {
                bridge.stop();
            }
            self.push_log("Quit requested from the notification-area menu.");
            context.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        quit
    }

    fn request_refresh(&mut self) {
        if self.refresh_rx.is_some() {
            return;
        }
        self.push_log("Scanning active Windows audio endpoints.");
        let (sender, receiver) = mpsc::channel();
        match thread::Builder::new()
            .name("endpoint-discovery".to_owned())
            .spawn(move || {
                let result = devices::endpoint_catalog().map_err(|error| format!("{error:#}"));
                let _ = sender.send(result);
            }) {
            Ok(_) => {
                self.refresh_rx = Some(receiver);
                self.error = None;
            }
            Err(error) => {
                let message = format!("Could not start endpoint discovery: {error}");
                self.push_log(format!("ERROR: {message}"));
                self.error = Some(message);
                self.status = "Device refresh failed".to_owned();
                if self.window_hidden_to_tray {
                    self.restore_window = true;
                }
            }
        }
    }

    fn poll_background_work(&mut self) {
        let refresh_result =
            self.refresh_rx
                .as_ref()
                .and_then(|receiver| match receiver.try_recv() {
                    Ok(result) => Some(result),
                    Err(mpsc::TryRecvError::Empty) => None,
                    Err(mpsc::TryRecvError::Disconnected) => Some(Err(
                        "endpoint discovery stopped before returning a result".to_owned(),
                    )),
                });
        if let Some(result) = refresh_result {
            self.refresh_rx = None;
            match result {
                Ok(catalog) => {
                    self.push_log(format!(
                        "Endpoint scan complete: {} capture, {} playback.",
                        catalog.capture.len(),
                        catalog.render.len()
                    ));
                    self.catalog = catalog;
                    if !matches!(self.login_auto_start, LoginAutoStartState::Failed(_)) {
                        self.error = None;
                    }
                }
                Err(error) => {
                    let message = format!("Device refresh failed: {error}");
                    self.push_log(format!("ERROR: {message}"));
                    self.catalog = EndpointCatalog::default();
                    if self.login_auto_start.is_waiting() {
                        if let LoginAutoStartState::Waiting { last_issue, .. } =
                            &mut self.login_auto_start
                        {
                            *last_issue = Some(message);
                        }
                        self.status = "Waiting for saved audio devices".to_owned();
                    } else {
                        self.error = Some(message);
                        self.status = "Device refresh failed".to_owned();
                        if self.window_hidden_to_tray {
                            self.restore_window = true;
                        }
                    }
                }
            }
        }

        let events = self
            .bridge
            .as_ref()
            .map(BridgeHandle::drain_events)
            .unwrap_or_default();
        for event in events {
            match event {
                BridgeEvent::Starting => {
                    self.push_log("Starting bridge and resolving selected endpoints.");
                    self.status = "Starting: resolving endpoints".to_owned();
                    self.error = None;
                    self.metrics = None;
                    self.started_streams.clear();
                }
                BridgeEvent::StreamStarted {
                    label,
                    buffer_frames,
                } => {
                    let message = format!("{label}: {buffer_frames}-frame shared buffer");
                    self.push_log(format!("WASAPI stream ready: {message}."));
                    self.started_streams.push(message);
                    self.status = format!(
                        "Starting: {}/3 audio streams ready",
                        self.started_streams.len()
                    );
                }
                BridgeEvent::Running => {
                    let automatic_start_timed_out = matches!(
                        self.login_auto_start,
                        LoginAutoStartState::Starting { deadline, .. }
                            if Instant::now() >= deadline
                    );
                    if matches!(self.login_auto_start, LoginAutoStartState::Cancelling) {
                        self.push_log(
                            "Cancelled automatic start reported ready late; stopping it again.",
                        );
                        if let Some(bridge) = &self.bridge {
                            bridge.stop();
                        }
                        self.status = "Cancelling automatic start".to_owned();
                    } else if automatic_start_timed_out {
                        if let Some(bridge) = &self.bridge {
                            bridge.stop();
                        }
                        self.fail_login_auto_start(
                            "Automatic start reported ready after its 90-second deadline; a stop was requested."
                                .to_owned(),
                        );
                    } else if matches!(self.login_auto_start, LoginAutoStartState::Failed(_))
                        && self.launch_options.login_startup
                    {
                        self.push_log(
                            "A timed-out automatic start reported ready late; stopping it.",
                        );
                        if let Some(bridge) = &self.bridge {
                            bridge.stop();
                        }
                        self.status = "Automatic start paused".to_owned();
                        self.restore_window = true;
                    } else {
                        self.push_log("Bridge is running.");
                        self.status = "Running".to_owned();
                        if matches!(self.login_auto_start, LoginAutoStartState::Starting { .. }) {
                            self.login_auto_start = LoginAutoStartState::Complete;
                        }
                    }
                }
                BridgeEvent::Metrics(summary) => {
                    self.push_log(format!("AEC metrics: {summary}"));
                    self.metrics = Some(summary);
                }
                BridgeEvent::Stopped(summary) => {
                    self.push_log(format!("Bridge stopped: {summary}"));
                    self.status = "Stopped cleanly".to_owned();
                    self.metrics = Some(summary);
                    if matches!(
                        self.login_auto_start,
                        LoginAutoStartState::Complete | LoginAutoStartState::Cancelling
                    ) {
                        self.login_auto_start = LoginAutoStartState::Cancelled;
                    }
                }
                BridgeEvent::Failed(message) => {
                    self.push_log(format!("ERROR: Bridge failed: {message}"));
                    if !self.retry_login_after_start_failure(&message) {
                        let completed_automatic_start = matches!(
                            self.login_auto_start,
                            LoginAutoStartState::Complete | LoginAutoStartState::Cancelling
                        );
                        self.status = "Failed".to_owned();
                        self.error = Some(message.clone());
                        if completed_automatic_start {
                            self.login_auto_start = LoginAutoStartState::Failed(message);
                        }
                        self.restore_window = restore_after_fatal_bridge_error(
                            completed_automatic_start,
                            self.window_hidden_to_tray,
                        );
                    }
                }
            }
        }

        let reap_result = self.bridge.as_mut().map(BridgeHandle::reap_if_finished);
        match reap_result {
            Some(Ok(true)) => self.bridge = None,
            Some(Err(error)) => {
                self.bridge = None;
                self.push_log(format!("ERROR: Bridge worker failed: {error}"));
                if !self.retry_login_after_start_failure(&error) {
                    let completed_automatic_start = matches!(
                        self.login_auto_start,
                        LoginAutoStartState::Complete | LoginAutoStartState::Cancelling
                    );
                    self.status = "Failed".to_owned();
                    self.error = Some(error.clone());
                    if completed_automatic_start {
                        self.login_auto_start = LoginAutoStartState::Failed(error);
                    }
                    self.restore_window = restore_after_fatal_bridge_error(
                        completed_automatic_start,
                        self.window_hidden_to_tray,
                    );
                }
            }
            _ => {}
        }

        self.advance_login_auto_start();
    }

    fn handoff_is_approved(&self) -> bool {
        self.settings
            .handoff_render
            .as_ref()
            .zip(self.settings.approved_handoff_id.as_ref())
            .is_some_and(|(handoff, approved_id)| handoff.id == *approved_id)
    }

    fn automatic_start_configuration_problem(&self) -> Option<String> {
        let Some(microphone) = self.settings.microphone.as_ref() else {
            return Some("Select a raw microphone first.".to_owned());
        };
        let Some(reference) = self.settings.reference.as_ref() else {
            return Some("Select an echo reference first.".to_owned());
        };
        let Some(handoff) = self.settings.handoff_render.as_ref() else {
            return Some("Select a virtual cable output first.".to_owned());
        };
        if microphone.id == reference.id {
            return Some("The microphone and echo reference must be different devices.".to_owned());
        }
        if self.settings.reference_mode == ReferenceMode::Loopback && reference.id == handoff.id {
            return Some(
                "The playback reference and virtual cable output must be different devices."
                    .to_owned(),
            );
        }
        if !self.handoff_is_approved() {
            return Some(
                "Confirm that the selected handoff is a virtual cable before enabling automatic start."
                    .to_owned(),
            );
        }
        None
    }

    fn start_readiness(&self) -> StartReadiness {
        if self.bridge.is_some() {
            return StartReadiness::Blocked("The bridge is already active.".to_owned());
        }
        if self.refresh_rx.is_some() {
            return StartReadiness::Waiting("Waiting for device discovery to finish.".to_owned());
        }
        if self.settings.microphone.is_none() {
            return StartReadiness::Blocked("Raw microphone is not selected.".to_owned());
        }
        if self.settings.reference.is_none() {
            return StartReadiness::Blocked("Echo reference is not selected.".to_owned());
        }
        if self.settings.handoff_render.is_none() {
            return StartReadiness::Blocked("Virtual cable output is not selected.".to_owned());
        }
        if let Some(problem) = self.automatic_start_configuration_problem() {
            return StartReadiness::Blocked(problem);
        }
        match endpoint_readiness(
            "Raw microphone",
            self.settings.microphone.as_ref(),
            &self.catalog.capture,
            true,
        ) {
            StartReadiness::Ready => {}
            issue => return issue,
        }
        let reference_endpoints = match self.settings.reference_mode {
            ReferenceMode::Loopback => &self.catalog.render,
            ReferenceMode::Capture => &self.catalog.capture,
        };
        match endpoint_readiness(
            "AEC reference",
            self.settings.reference.as_ref(),
            reference_endpoints,
            true,
        ) {
            StartReadiness::Ready => {}
            issue => return issue,
        }
        match endpoint_readiness(
            "Virtual cable output",
            self.settings.handoff_render.as_ref(),
            &self.catalog.render,
            true,
        ) {
            StartReadiness::Ready => {}
            issue => return issue,
        }
        StartReadiness::Ready
    }

    fn start_problem(&self) -> Option<String> {
        match self.start_readiness() {
            StartReadiness::Ready => None,
            StartReadiness::Waiting(problem) | StartReadiness::Blocked(problem) => Some(problem),
        }
    }

    fn fail_login_auto_start(&mut self, message: String) {
        // Ignore any discovery result that arrives after this attempt. The
        // detached worker cannot mutate application state without its sender.
        self.refresh_rx = None;
        self.push_log(format!("ERROR: {message}"));
        self.status = "Automatic start paused".to_owned();
        self.error = Some(message.clone());
        self.login_auto_start = LoginAutoStartState::Failed(message);
        self.restore_window = true;
    }

    fn retry_login_after_start_failure(&mut self, message: &str) -> bool {
        let (deadline, attempt) = match self.login_auto_start {
            LoginAutoStartState::Starting { deadline, attempt } => (deadline, attempt),
            _ => return false,
        };
        let now = Instant::now();
        if now >= deadline {
            self.fail_login_auto_start(format!(
                "Automatic start timed out after an audio stream failed to open: {message}"
            ));
            return true;
        }

        let next_attempt = attempt.saturating_add(1);
        let retry_delay = login_retry_delay(next_attempt);
        let issue = format!("audio streams were not ready: {message}");
        self.catalog = EndpointCatalog::default();
        self.started_streams.clear();
        self.metrics = None;
        self.error = None;
        self.status = "Waiting for saved audio devices".to_owned();
        self.login_auto_start = LoginAutoStartState::Waiting {
            deadline,
            next_retry: now + retry_delay,
            attempt: next_attempt,
            last_issue: Some(issue),
        };
        self.push_log(format!(
            "Automatic stream startup failed; retrying in {} second(s).",
            retry_delay.as_secs()
        ));
        true
    }

    fn cancel_login_auto_start(&mut self, reason: &str) {
        let was_starting = matches!(self.login_auto_start, LoginAutoStartState::Starting { .. });
        if self.login_auto_start.is_waiting()
            || matches!(
                self.login_auto_start,
                LoginAutoStartState::Starting { .. } | LoginAutoStartState::Failed(_)
            )
        {
            if was_starting && let Some(bridge) = &self.bridge {
                bridge.stop();
            }
            self.push_log(format!("Automatic start cancelled: {reason}."));
            self.login_auto_start = if was_starting {
                LoginAutoStartState::Cancelling
            } else {
                LoginAutoStartState::Cancelled
            };
            if was_starting {
                self.status = "Cancelling automatic start".to_owned();
            } else if self.bridge.is_none() {
                self.status = "Idle".to_owned();
            }
        }
    }

    fn advance_login_auto_start(&mut self) {
        let now = Instant::now();
        if let LoginAutoStartState::Starting { deadline, .. } = self.login_auto_start
            && now >= deadline
        {
            if let Some(bridge) = &self.bridge {
                bridge.stop();
            }
            self.fail_login_auto_start(
                "Automatic start exceeded 90 seconds while opening audio streams; a stop was requested."
                    .to_owned(),
            );
            return;
        }
        let (deadline, next_retry, attempt, previous_issue) = match &self.login_auto_start {
            LoginAutoStartState::Waiting {
                deadline,
                next_retry,
                attempt,
                last_issue,
            } => (*deadline, *next_retry, *attempt, last_issue.clone()),
            _ => return,
        };
        if now >= deadline {
            let detail = previous_issue
                .unwrap_or_else(|| "the saved devices did not become ready".to_owned());
            self.fail_login_auto_start(format!(
                "Automatic start paused because {detail} Refresh the devices and retry when the audio stack is ready."
            ));
            return;
        }
        if self.refresh_rx.is_some() {
            return;
        }
        if self.bridge.is_some() {
            return;
        }

        match self.start_readiness() {
            StartReadiness::Ready => {
                self.push_log("All exact saved endpoints are ready; starting automatically.");
                self.login_auto_start = LoginAutoStartState::Starting { deadline, attempt };
                self.start_bridge(StartOrigin::Login);
            }
            StartReadiness::Blocked(problem) => {
                self.fail_login_auto_start(format!("Automatic start is blocked: {problem}"));
            }
            StartReadiness::Waiting(problem) => {
                if previous_issue.as_deref() != Some(problem.as_str()) {
                    self.push_log(format!("Automatic start is waiting: {problem}"));
                }
                let retry_now = now >= next_retry;
                if let LoginAutoStartState::Waiting {
                    next_retry,
                    attempt,
                    last_issue,
                    ..
                } = &mut self.login_auto_start
                {
                    *last_issue = Some(problem);
                    if retry_now {
                        *attempt = attempt.saturating_add(1);
                        *next_retry = now + login_retry_delay(*attempt);
                    }
                }
                self.status = "Waiting for saved audio devices".to_owned();
                if retry_now {
                    self.push_log(format!(
                        "Retrying endpoint discovery for automatic start (attempt {}).",
                        attempt.saturating_add(1)
                    ));
                    self.request_refresh();
                }
            }
        }
    }

    fn start_bridge(&mut self, origin: StartOrigin) {
        if origin == StartOrigin::Manual {
            self.cancel_login_auto_start("a manual start was requested");
        }
        let readiness = self.start_readiness();
        if let StartReadiness::Waiting(problem) | StartReadiness::Blocked(problem) = readiness {
            self.push_log(format!("Start blocked: {problem}"));
            self.error = Some(problem);
            return;
        }
        let reference_mode = match self.settings.reference_mode {
            ReferenceMode::Loopback => "playback loopback",
            ReferenceMode::Capture => "recording device/virtual mix",
        };
        self.push_log(format!(
            "Start requested: mic='{}', reference='{}' ({reference_mode}), handoff='{}'.",
            saved_endpoint_name(&self.settings.microphone, "<missing>"),
            saved_endpoint_name(&self.settings.reference, "<missing>"),
            saved_endpoint_name(&self.settings.handoff_render, "<missing>"),
        ));
        let args = RunArgs {
            endpoints: EndpointArgs {
                mic: self
                    .settings
                    .microphone
                    .as_ref()
                    .expect("selection was validated")
                    .id
                    .clone(),
                reference: self
                    .settings
                    .reference
                    .as_ref()
                    .expect("selection was validated")
                    .id
                    .clone(),
                reference_mode: self.settings.reference_mode,
                output: self
                    .settings
                    .handoff_render
                    .as_ref()
                    .expect("selection was validated")
                    .id
                    .clone(),
            },
            capture_delay_ms: self.settings.capture_delay_ms,
            stream_delay_ms: self.settings.stream_delay_ms,
            duration_seconds: 0,
            bypass: self.bypass,
            mute_output: self.mute_output,
        };
        match bridge::spawn(args) {
            Ok(handle) => {
                self.bridge = Some(handle);
                self.status = "Starting".to_owned();
                self.error = None;
                self.metrics = None;
            }
            Err(error) => {
                let message = format!("Could not start AEC Bridge: {error:#}");
                self.push_log(format!("ERROR: {message}"));
                if origin != StartOrigin::Login || !self.retry_login_after_start_failure(&message) {
                    self.status = "Failed".to_owned();
                    self.error = Some(message);
                }
            }
        }
    }

    fn execute_diagnostic_command(&mut self, command: &str) {
        let command = command.trim();
        if command.is_empty() {
            return;
        }
        if command.eq_ignore_ascii_case("clear") {
            self.debug_log.clear();
            return;
        }

        self.push_log(format!("> {command}"));
        match command.to_ascii_lowercase().as_str() {
            "help" => {
                self.push_log("Commands: help, list, check, status, refresh, start, stop, clear.");
                self.push_log(
                    "This console runs built-in AEC Bridge actions only; it is not a system shell.",
                );
            }
            "list" => {
                let capture = self
                    .catalog
                    .capture
                    .iter()
                    .map(|endpoint| {
                        format!(
                            "  [capture] {} | {} | {}",
                            endpoint.name, endpoint.format, endpoint.id
                        )
                    })
                    .collect::<Vec<_>>();
                let render = self
                    .catalog
                    .render
                    .iter()
                    .map(|endpoint| {
                        format!(
                            "  [playback] {} | {} | {}",
                            endpoint.name, endpoint.format, endpoint.id
                        )
                    })
                    .collect::<Vec<_>>();
                self.push_log(format!("Capture endpoints ({}):", capture.len()));
                for line in capture {
                    self.push_log(line);
                }
                self.push_log(format!("Playback endpoints ({}):", render.len()));
                for line in render {
                    self.push_log(line);
                }
            }
            "check" => match self.start_problem() {
                Some(problem) => self.push_log(format!("Configuration check failed: {problem}")),
                None => self.push_log("Configuration check passed; the bridge is ready to start."),
            },
            "status" => {
                let mode = match self.settings.reference_mode {
                    ReferenceMode::Loopback => "playback loopback",
                    ReferenceMode::Capture => "recording device/virtual mix",
                };
                self.push_log(format!("Status: {}.", self.status));
                let login_status = match &self.login_startup_status {
                    Some(LoginStartupStatus::Disabled) => "not registered",
                    Some(LoginStartupStatus::Current) => "registered",
                    Some(LoginStartupStatus::Stale { .. }) => "needs repair",
                    None => "unknown",
                };
                self.push_log(format!(
                    "Windows sign-in startup: {login_status}; automatic bridge start: {}.",
                    if self.settings.auto_start_bridge {
                        "enabled"
                    } else {
                        "disabled"
                    }
                ));
                self.push_log(format!(
                    "Route: '{}' + '{}' ({mode}) -> '{}' -> '{}'.",
                    saved_endpoint_name(&self.settings.microphone, "<not selected>"),
                    saved_endpoint_name(&self.settings.reference, "<not selected>"),
                    saved_endpoint_name(&self.settings.handoff_render, "<not selected>"),
                    saved_endpoint_name(&self.settings.downstream_capture, "<not selected>"),
                ));
            }
            "refresh" => {
                if self.bridge.is_some() {
                    self.push_log("Refresh blocked while the bridge is active; stop it first.");
                } else {
                    self.request_refresh();
                }
            }
            "start" => {
                if self.bridge.is_some() {
                    self.push_log("Start ignored: the bridge is already active.");
                } else {
                    self.start_bridge(StartOrigin::Manual);
                }
            }
            "stop" => {
                if let Some(bridge) = &self.bridge {
                    bridge.stop();
                    self.cancel_login_auto_start("the bridge was stopped manually");
                    self.status = "Stopping".to_owned();
                    self.push_log("Stop requested from the diagnostics console.");
                } else {
                    self.push_log("Stop ignored: the bridge is not running.");
                }
            }
            _ => self.push_log(format!(
                "Unknown command '{command}'. Type 'help' for the built-in command list."
            )),
        }
    }

    fn login_startup_registered(&self) -> bool {
        matches!(
            self.login_startup_status,
            Some(LoginStartupStatus::Current | LoginStartupStatus::Stale { .. })
        )
    }

    fn login_startup_is_current(&self) -> bool {
        matches!(self.login_startup_status, Some(LoginStartupStatus::Current))
    }

    fn set_login_startup_enabled(&mut self, enabled: bool) {
        self.startup_error = None;
        self.startup_notice = None;
        let result = if enabled {
            startup::enable_login_startup().map(|()| true)
        } else {
            startup::disable_login_startup().map(|_| false)
        };
        match result {
            Ok(true) => {
                self.login_startup_status = Some(LoginStartupStatus::Current);
                self.startup_notice = Some(
                    "Startup registration added. Windows Startup Apps can still disable it."
                        .to_owned(),
                );
                self.push_log("Windows sign-in startup enabled for the current executable.");
            }
            Ok(false) => {
                self.login_startup_status = Some(LoginStartupStatus::Disabled);
                self.settings.auto_start_bridge = false;
                self.cancel_login_auto_start("Windows sign-in startup was disabled");
                self.startup_notice = Some("Windows sign-in startup is disabled.".to_owned());
                self.push_log("Windows sign-in startup disabled.");
            }
            Err(error) => {
                let action = if enabled { "enable" } else { "disable" };
                let message = format!("Could not {action} Windows sign-in startup: {error:#}");
                self.push_log(format!("ERROR: {message}"));
                self.startup_error = Some(message);
            }
        }
    }

    fn arm_login_auto_start(&mut self) {
        if let Some(problem) = self.automatic_start_configuration_problem() {
            self.fail_login_auto_start(format!("Automatic start is blocked: {problem}"));
            return;
        }
        self.login_auto_start = LoginAutoStartState::waiting();
        self.status = "Waiting for saved audio devices".to_owned();
        self.error = None;
        self.push_log("Automatic start retry requested for this sign-in.");
        if self.refresh_rx.is_none() {
            self.request_refresh();
        }
    }

    fn render_header(&mut self, ui: &mut egui::Ui, colors: Palette) {
        ui.horizontal(|ui| {
            brand_mark(ui, colors);
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new("AEC BRIDGE")
                        .size(11.0)
                        .strong()
                        .color(colors.accent),
                );
                ui.label(
                    egui::RichText::new("Clean microphone routing")
                        .size(24.0)
                        .strong(),
                );
                ui.label(
                    egui::RichText::new(
                        "Remove playback echo before sending your microphone downstream.",
                    )
                    .color(colors.muted),
                );
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let theme_label = if self.settings.dark_mode {
                    "Light theme"
                } else {
                    "Dark theme"
                };
                if ui
                    .add(
                        egui::Button::new(egui::RichText::new(theme_label).color(colors.muted))
                            .fill(colors.field)
                            .stroke(egui::Stroke::new(1.0, colors.border))
                            .corner_radius(8),
                    )
                    .on_hover_text("Switch the application color theme")
                    .clicked()
                {
                    self.settings.dark_mode = !self.settings.dark_mode;
                    apply_theme(ui.ctx(), self.settings.dark_mode);
                    ui.ctx().request_repaint();
                }
                ui.add_space(4.0);

                let (label, foreground, background) =
                    if self.error.is_some() && self.status != "Running" {
                        ("Needs attention", colors.danger, colors.danger_soft)
                    } else if self.status == "Running" {
                        ("Running", colors.success, colors.success_soft)
                    } else if self.status.starts_with("Starting")
                        || self.status.starts_with("Waiting")
                        || self.status == "Stopping"
                    {
                        (self.status.as_str(), colors.warning, colors.warning_soft)
                    } else {
                        (self.status.as_str(), colors.muted, colors.field)
                    };
                status_badge(ui, label, foreground, background, colors.border);
            });
        });
    }

    fn render_session_details(&self, ui: &mut egui::Ui, colors: Palette) {
        card(ui, colors, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(if self.status == "Running" {
                        "Live session"
                    } else {
                        "Session activity"
                    })
                    .strong(),
                );
                if self.status.starts_with("Starting") {
                    ui.spinner();
                }
            });
            if !self.started_streams.is_empty() {
                ui.add_space(4.0);
                for stream in &self.started_streams {
                    ui.horizontal(|ui| {
                        status_dot(ui, colors.success);
                        ui.label(egui::RichText::new(stream).small().color(colors.muted));
                    });
                }
            }
            if let Some(metrics) = &self.metrics {
                ui.add_space(4.0);
                ui.add(
                    egui::Label::new(egui::RichText::new(metrics).small().color(colors.muted))
                        .wrap(),
                );
            }
        });
    }

    fn render_input_card(&mut self, ui: &mut egui::Ui, colors: Palette) {
        let active = self.bridge.is_some();
        card(ui, colors, |ui| {
            section_header(
                ui,
                colors,
                "01",
                "Microphone and echo reference",
                "Choose the raw microphone to clean and the audio being played into your headphones.",
            );
            ui.add_space(18.0);

            ui.add_enabled_ui(!active, |ui| {
                field_label(
                    ui,
                    colors,
                    "Raw microphone",
                    "The unprocessed physical microphone signal.",
                );
                if endpoint_combo(
                    ui,
                    "microphone-combo",
                    &mut self.settings.microphone,
                    &self.catalog.capture,
                    false,
                ) {
                    self.error = None;
                    self.cancel_login_auto_start("the microphone selection changed");
                }

                ui.add_space(18.0);
                field_label(
                    ui,
                    colors,
                    "Reference method",
                    "How the bridge reads the audio that may leak back into the microphone.",
                );

                let previous = self.settings.reference_mode;
                ui.horizontal(|ui| {
                    let button_width = ((ui.available_width() - 8.0) / 2.0).max(180.0);
                    if segmented_button(
                        ui,
                        colors,
                        button_width,
                        "Playback device",
                        self.settings.reference_mode == ReferenceMode::Loopback,
                    )
                    .on_hover_text(
                        "Capture an ordinary Windows headphones or speaker output using loopback.",
                    )
                    .clicked()
                    {
                        self.settings.reference_mode = ReferenceMode::Loopback;
                    }
                    if segmented_button(
                        ui,
                        colors,
                        button_width,
                        "Recording device or virtual mix",
                        self.settings.reference_mode == ReferenceMode::Capture,
                    )
                    .on_hover_text(
                        "Use an existing recording endpoint that carries the audio heard through your headphones.",
                    )
                    .clicked()
                    {
                        self.settings.reference_mode = ReferenceMode::Capture;
                    }
                });
                if self.settings.reference_mode != previous {
                    self.settings.reference = None;
                    self.error = None;
                    self.cancel_login_auto_start("the reference method changed");
                }

                let method_help = match self.settings.reference_mode {
                    ReferenceMode::Loopback => {
                        "Best when your headphones appear as a normal Windows playback device."
                    }
                    ReferenceMode::Capture => {
                        "Choose a recording source containing headphone playback, such as Stereo Mix or a virtual listening mix. Do not select a microphone by itself."
                    }
                };
                ui.label(
                    egui::RichText::new(method_help)
                        .small()
                        .color(colors.muted),
                );
                ui.add_space(10.0);

                let (reference_label, reference_help, reference_endpoints) =
                    match self.settings.reference_mode {
                        ReferenceMode::Loopback => (
                            "Headphones or speaker output",
                            "The Windows playback endpoint whose audio can leak into the microphone.",
                            &self.catalog.render,
                        ),
                        ReferenceMode::Capture => (
                            "Recording device or virtual mix",
                            "A recording source containing playback audio, but no raw or processed microphone return.",
                            &self.catalog.capture,
                        ),
                    };
                field_label(ui, colors, reference_label, reference_help);
                if endpoint_combo(
                    ui,
                    "reference-combo",
                    &mut self.settings.reference,
                    reference_endpoints,
                    false,
                ) {
                    self.error = None;
                    self.cancel_login_auto_start("the reference selection changed");
                }

            });
        });
    }

    fn render_output_card(&mut self, ui: &mut egui::Ui, colors: Palette) {
        let active = self.bridge.is_some();
        card(ui, colors, |ui| {
            section_header(
                ui,
                colors,
                "02",
                "Clean output",
                "Hand the echo-cancelled microphone to any downstream app through a virtual cable.",
            );
            ui.add_space(18.0);

            ui.add_enabled_ui(!active, |ui| {
                field_label(
                    ui,
                    colors,
                    "Virtual cable output",
                    "The bridge writes cleaned audio to the playback side of the cable.",
                );
                if endpoint_combo(
                    ui,
                    "handoff-combo",
                    &mut self.settings.handoff_render,
                    &self.catalog.render,
                    false,
                ) {
                    self.settings.approved_handoff_id = None;
                    self.settings.auto_start_bridge = false;
                    self.error = None;
                    self.cancel_login_auto_start("the virtual cable output changed");
                }

                ui.add_space(16.0);
                field_label(
                    ui,
                    colors,
                    "Downstream microphone input  (optional)",
                    "Select this recording side of the cable in your downstream app.",
                );
                if endpoint_combo(
                    ui,
                    "downstream-combo",
                    &mut self.settings.downstream_capture,
                    &self.catalog.capture,
                    true,
                ) {
                    self.error = None;
                }

                if let (Some(output), Some(input)) = (
                    &self.settings.handoff_render,
                    &self.settings.downstream_capture,
                ) {
                    ui.add_space(10.0);
                    let message = format!(
                        "The bridge writes to '{}'. Select '{}' as the microphone in your downstream app.",
                        output.last_name, input.last_name
                    );
                    callout(
                        ui,
                        colors.accent_soft,
                        colors.accent,
                        "Cable pairing",
                        &message,
                    );
                }

                ui.add_space(12.0);
                callout(
                    ui,
                    colors.warning_soft,
                    colors.warning,
                    "Feedback protection",
                    "This output must be a virtual cable. Sending the bridge to headphones or speakers can create loud feedback.",
                );
                ui.add_space(6.0);
                ui.add_enabled_ui(self.settings.handoff_render.is_some(), |ui| {
                    let mut handoff_confirmed = self.handoff_is_approved();
                    if ui
                        .checkbox(
                            &mut handoff_confirmed,
                            "This is a virtual cable, not headphones or speakers",
                        )
                        .changed()
                    {
                        self.settings.approved_handoff_id = handoff_confirmed.then(|| {
                            self.settings
                                .handoff_render
                                .as_ref()
                                .expect("confirmation is disabled without a handoff")
                                .id
                                .clone()
                        });
                        if !handoff_confirmed {
                            self.settings.auto_start_bridge = false;
                            self.cancel_login_auto_start("the virtual output approval was removed");
                        }
                        self.error = None;
                    }
                });
            });
        });
    }

    fn render_startup_card(&mut self, ui: &mut egui::Ui, colors: Palette) {
        let registered = self.login_startup_registered();
        let registration_current = self.login_startup_is_current();
        let route_problem = self.automatic_start_configuration_problem();
        let stale_command = match &self.login_startup_status {
            Some(LoginStartupStatus::Stale { registered_command }) => {
                Some(registered_command.clone())
            }
            _ => None,
        };
        let runtime_status = match &self.login_auto_start {
            LoginAutoStartState::Waiting {
                deadline,
                last_issue,
                ..
            } => {
                let issue = last_issue.as_deref().unwrap_or(
                    "Checking the exact saved microphone, reference, and virtual output.",
                );
                let seconds = deadline.saturating_duration_since(Instant::now()).as_secs();
                Some((
                    "Waiting for saved audio devices",
                    format!("{issue} Retrying for up to {seconds} more seconds."),
                    colors.accent_soft,
                    colors.accent,
                    true,
                ))
            }
            LoginAutoStartState::Starting { .. } => Some((
                "Starting automatically",
                "The saved devices are ready and the audio streams are opening.".to_owned(),
                colors.warning_soft,
                colors.warning,
                false,
            )),
            LoginAutoStartState::Cancelling => Some((
                "Cancelling automatic start",
                "A stop was requested while the audio streams were opening.".to_owned(),
                colors.warning_soft,
                colors.warning,
                false,
            )),
            LoginAutoStartState::Complete => Some((
                "Started automatically",
                "The bridge started from the saved route for this Windows sign-in.".to_owned(),
                colors.success_soft,
                colors.success,
                false,
            )),
            LoginAutoStartState::Failed(message) => Some((
                "Automatic start paused",
                message.clone(),
                colors.danger_soft,
                colors.danger,
                false,
            )),
            LoginAutoStartState::Cancelled => Some((
                "Automatic start cancelled",
                "The bridge will remain stopped for this sign-in unless you start it manually."
                    .to_owned(),
                colors.field,
                colors.muted,
                false,
            )),
            LoginAutoStartState::Inactive => None,
        };

        card(ui, colors, |ui| {
            section_header(
                ui,
                colors,
                "03",
                "Windows startup",
                "Control sign-in startup, background behavior, and automatic route startup.",
            );
            ui.add_space(16.0);

            let mut launch_at_login = registered;
            if ui
                .checkbox(
                    &mut launch_at_login,
                    "Open AEC Bridge when I sign in to Windows",
                )
                .changed()
            {
                self.set_login_startup_enabled(launch_at_login);
            }
            ui.label(
                egui::RichText::new(
                    "Registers this copy for your Windows account. Windows Startup Apps can independently disable it.",
                )
                .small()
                .color(colors.muted),
            );

            if let Some(command) = stale_command {
                ui.add_space(10.0);
                callout(
                    ui,
                    colors.warning_soft,
                    colors.warning,
                    "Startup entry needs repair",
                    &format!(
                        "Windows is registered to open a different copy or command: {command}"
                    ),
                );
                ui.add_space(6.0);
                if ui.small_button("Repair startup entry").clicked() {
                    self.set_login_startup_enabled(true);
                }
            }
            if let Some(error) = self.startup_error.clone() {
                ui.add_space(10.0);
                callout(
                    ui,
                    colors.danger_soft,
                    colors.danger,
                    "Windows startup error",
                    &error,
                );
            } else if let Some(notice) = self.startup_notice.clone() {
                ui.add_space(8.0);
                ui.label(egui::RichText::new(notice).small().color(colors.success));
            }

            ui.add_space(14.0);
            let mut minimize_to_tray = self.settings.minimize_to_tray;
            if ui
                .checkbox(
                    &mut minimize_to_tray,
                    "Hide AEC Bridge in the notification area when minimized",
                )
                .changed()
            {
                #[cfg(target_os = "windows")]
                self.set_tray_enabled(minimize_to_tray, TrayEnableOrigin::User);
            }
            ui.label(
                egui::RichText::new(
                    "AEC Bridge stays open in the background; active audio processing continues. Click its icon near the clock to reopen it; Close still quits.",
                )
                .small()
                .color(colors.muted),
            );
            if let Some(error) = self.tray_error.clone() {
                ui.add_space(8.0);
                callout(
                    ui,
                    colors.danger_soft,
                    colors.danger,
                    "Notification-area error",
                    &error,
                );
                #[cfg(target_os = "windows")]
                if self.settings.minimize_to_tray && self.tray.is_none() {
                    ui.add_space(6.0);
                    if ui.small_button("Retry notification-area icon").clicked() {
                        self.set_tray_enabled(true, TrayEnableOrigin::SavedPreference);
                    }
                }
            }

            ui.add_space(14.0);
            let auto_start_available = self.settings.auto_start_bridge
                || (registration_current && route_problem.is_none());
            ui.add_enabled_ui(auto_start_available, |ui| {
                let mut auto_start_bridge = self.settings.auto_start_bridge;
                if ui
                    .checkbox(
                        &mut auto_start_bridge,
                        "Start the bridge when saved audio devices are ready",
                    )
                    .changed()
                {
                    self.settings.auto_start_bridge = auto_start_bridge;
                    if auto_start_bridge {
                        self.push_log(
                            "Automatic bridge start enabled for future Windows sign-ins.",
                        );
                    } else {
                        self.cancel_login_auto_start("automatic bridge start was disabled");
                        self.push_log("Automatic bridge start disabled.");
                    }
                }
            });
            ui.label(
                egui::RichText::new(
                    "The app waits up to 90 seconds and uses exact device IDs. It never substitutes a default device.",
                )
                .small()
                .color(colors.muted),
            );
            if !registration_current {
                ui.label(
                    egui::RichText::new(
                        "Enable or repair Windows sign-in startup before enabling automatic bridge start.",
                    )
                    .small()
                    .color(colors.muted),
                );
            } else if let Some(problem) = route_problem {
                ui.label(egui::RichText::new(problem).small().color(colors.warning));
            }
            if let Some(handoff) = self
                .settings
                .handoff_render
                .as_ref()
                .filter(|_| self.handoff_is_approved())
            {
                ui.label(
                    egui::RichText::new(format!(
                        "Approved automatic output: {}",
                        handoff.last_name
                    ))
                    .small()
                    .color(colors.success),
                );
            }

            if let Some((title, detail, background, foreground, waiting)) = runtime_status {
                ui.add_space(12.0);
                callout(ui, background, foreground, title, &detail);
                if self.launch_options.login_startup
                    && self.settings.auto_start_bridge
                    && self.bridge.is_none()
                    && matches!(
                        self.login_auto_start,
                        LoginAutoStartState::Failed(_) | LoginAutoStartState::Cancelled
                    )
                {
                    ui.add_space(6.0);
                    if ui.small_button("Retry automatic start now").clicked() {
                        self.arm_login_auto_start();
                    }
                } else if matches!(self.login_auto_start, LoginAutoStartState::Failed(_))
                    && self.bridge.is_some()
                {
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(
                            "A stop was requested. Wait for the previous audio attempt to finish before retrying.",
                        )
                        .small()
                        .color(colors.muted),
                    );
                } else if waiting {
                    ui.add_space(6.0);
                    if ui.small_button("Cancel for this sign-in").clicked() {
                        self.cancel_login_auto_start("cancelled from Windows startup settings");
                    }
                }
            }
        });
    }

    fn render_advanced_card(&mut self, ui: &mut egui::Ui, colors: Palette) {
        let active = self.bridge.is_some();
        card(ui, colors, |ui| {
            egui::CollapsingHeader::new(
                egui::RichText::new("Advanced processing").strong().size(15.0),
            )
            .default_open(false)
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(
                        "The defaults are a good starting point. Change these only while diagnosing alignment or routing.",
                    )
                    .small()
                    .color(colors.muted),
                );
                ui.add_space(10.0);
                ui.add_enabled_ui(!active, |ui| {
                    egui::Grid::new("advanced-settings")
                        .num_columns(2)
                        .spacing([24.0, 10.0])
                        .show(ui, |ui| {
                            ui.label("Microphone capture delay")
                                .on_hover_text("Briefly holds microphone audio so the reference reaches AEC first.");
                            ui.add(
                                egui::DragValue::new(&mut self.settings.capture_delay_ms)
                                    .range(0..=250)
                                    .suffix(" ms"),
                            );
                            ui.end_row();

                            ui.label("AEC stream delay")
                                .on_hover_text("The device/render delay estimate supplied to the echo canceller.");
                            ui.add(
                                egui::DragValue::new(&mut self.settings.stream_delay_ms)
                                    .range(0..=500)
                                    .suffix(" ms"),
                            );
                            ui.end_row();
                        });
                    ui.add_space(8.0);
                    ui.checkbox(&mut self.bypass, "Bypass echo cancellation");
                    ui.checkbox(&mut self.mute_output, "Mute virtual handoff (safety test)");
                });
            });
        });
    }

    fn render_diagnostics_card(&mut self, ui: &mut egui::Ui, colors: Palette) {
        card(ui, colors, |ui| {
            egui::CollapsingHeader::new(
                egui::RichText::new("Diagnostics console")
                    .strong()
                    .size(15.0),
            )
            .default_open(false)
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(
                        "Run built-in bridge commands and inspect endpoint, startup, runtime, and failure logs.",
                    )
                    .small()
                    .color(colors.muted),
                );
                ui.label(
                    egui::RichText::new(
                        "This accepts AEC Bridge commands only; it does not execute system shell commands.",
                    )
                    .small()
                    .color(colors.muted),
                );
                ui.add_space(10.0);

                let mut run_command = false;
                let mut command_response = None;
                ui.horizontal(|ui| {
                    let command_width = (ui.available_width() - 92.0).max(220.0);
                    let response = ui.add_sized(
                        [command_width, 36.0],
                        egui::TextEdit::singleline(&mut self.diagnostic_command)
                            .hint_text("Type help, list, check, status, refresh, start, stop, or clear"),
                    );
                    run_command |= response.has_focus()
                        && ui.input(|input| input.key_pressed(egui::Key::Enter));
                    command_response = Some(response);
                    run_command |= ui
                        .add_sized(
                            [84.0, 36.0],
                            egui::Button::new("Run")
                                .fill(colors.accent_soft)
                                .stroke(egui::Stroke::new(1.0, colors.accent))
                                .corner_radius(8),
                        )
                        .clicked();
                });
                if run_command {
                    let command = std::mem::take(&mut self.diagnostic_command);
                    self.execute_diagnostic_command(&command);
                    if let Some(response) = command_response {
                        response.request_focus();
                    }
                }

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "Rolling log  ·  {} / {DEBUG_LOG_LIMIT} lines",
                            self.debug_log.len()
                        ))
                        .small()
                        .color(colors.muted),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Clear log").clicked() {
                            self.debug_log.clear();
                        }
                        if ui.small_button("Copy log").clicked() {
                            let text = self
                                .debug_log
                                .iter()
                                .cloned()
                                .collect::<Vec<_>>()
                                .join("\n");
                            ui.ctx().copy_text(text);
                        }
                    });
                });

                egui::Frame::new()
                    .fill(colors.field)
                    .stroke(egui::Stroke::new(1.0, colors.border))
                    .corner_radius(8)
                    .inner_margin(egui::Margin::same(10))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        egui::ScrollArea::vertical()
                            .id_salt("diagnostic-log")
                            .max_height(200.0)
                            .min_scrolled_height(110.0)
                            .stick_to_bottom(true)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                if self.debug_log.is_empty() {
                                    ui.label(
                                        egui::RichText::new(
                                            "No log entries. Type 'help' to see the built-in commands.",
                                        )
                                        .monospace()
                                        .small()
                                        .color(colors.muted),
                                    );
                                } else {
                                    for line in &self.debug_log {
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(line)
                                                    .monospace()
                                                    .small()
                                                    .color(colors.text),
                                            )
                                            .wrap(),
                                        );
                                    }
                                }
                            });
                    });
            });
        });
    }

    fn render_signal_path_card(&self, ui: &mut egui::Ui, colors: Palette) {
        let microphone = saved_endpoint_name(&self.settings.microphone, "Raw microphone");
        let reference = saved_endpoint_name(&self.settings.reference, "Echo reference");
        let handoff = saved_endpoint_name(&self.settings.handoff_render, "Virtual cable output");
        let downstream =
            saved_endpoint_name(&self.settings.downstream_capture, "Downstream cable input");
        let reference_detail = match self.settings.reference_mode {
            ReferenceMode::Loopback => {
                format!(
                    "Listen to '{}' through Windows playback loopback.",
                    reference
                )
            }
            ReferenceMode::Capture => {
                format!(
                    "Read '{}' as the recording-device or virtual-mix reference.",
                    reference
                )
            }
        };

        card(ui, colors, |ui| {
            egui::CollapsingHeader::new(
                egui::RichText::new("Signal path and setup notes")
                    .strong()
                    .size(15.0),
            )
            .default_open(false)
            .show(ui, |ui| {
                ui.add_space(4.0);
                route_step(ui, colors, "1", "Echo reference", &reference_detail);
                route_step(
                    ui,
                    colors,
                    "2",
                    "Echo cancellation",
                    &format!("Clean '{}' using the reference audio.", microphone),
                );
                route_step(
                    ui,
                    colors,
                    "3",
                    "Virtual handoff",
                    &format!("Send the cleaned microphone to '{}', paired with '{}'.", handoff, downstream),
                );
                route_step(
                    ui,
                    colors,
                    "4",
                    "Downstream processing",
                    "Your processing, recording, streaming, or communication app reads the cable input.",
                );
                ui.add_space(6.0);
                callout(
                    ui,
                    colors.accent_soft,
                    colors.accent,
                    "Reference audio is analysis-only",
                    "The bridge uses the reference to recognize echo; it does not mix that playback audio into the microphone output.",
                );
                if self.settings.reference_mode == ReferenceMode::Capture {
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(
                            "Keep the raw mic, virtual-cable return, and processed microphone out of the reference mix.",
                        )
                        .small()
                        .color(colors.muted),
                    );
                }
            });
        });
    }

    fn render_action_bar(&mut self, ui: &mut egui::Ui, colors: Palette) {
        let active = self.bridge.is_some();
        let refreshing = self.refresh_rx.is_some();
        let stopping = self.status == "Stopping";
        let start_problem = self.start_problem();

        card(ui, colors, |ui| {
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        !active && !refreshing,
                        egui::Button::new("Refresh devices")
                            .fill(colors.field)
                            .stroke(egui::Stroke::new(1.0, colors.border))
                            .corner_radius(9),
                    )
                    .clicked()
                {
                    self.request_refresh();
                }
                if refreshing {
                    ui.spinner();
                    ui.label(
                        egui::RichText::new("Scanning Windows audio endpoints...")
                            .small()
                            .color(colors.muted),
                    );
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if active {
                        let label = if stopping {
                            "Stopping..."
                        } else {
                            "Stop bridge"
                        };
                        if ui
                            .add_enabled(
                                !stopping,
                                egui::Button::new(
                                    egui::RichText::new(label).strong().color(colors.danger),
                                )
                                .fill(colors.danger_soft)
                                .stroke(egui::Stroke::new(1.0, colors.danger))
                                .corner_radius(9)
                                .min_size(egui::vec2(144.0, 42.0)),
                            )
                            .clicked()
                            && let Some(bridge) = &self.bridge
                        {
                            bridge.stop();
                            self.cancel_login_auto_start("the bridge was stopped manually");
                            self.status = "Stopping".to_owned();
                            self.push_log("Stop requested from the main control.");
                        }
                    } else if ui
                        .add_enabled(
                            start_problem.is_none(),
                            egui::Button::new(
                                egui::RichText::new("Start bridge")
                                    .strong()
                                    .color(egui::Color32::WHITE),
                            )
                            .fill(colors.accent)
                            .stroke(egui::Stroke::NONE)
                            .corner_radius(9)
                            .min_size(egui::vec2(144.0, 42.0)),
                        )
                        .clicked()
                    {
                        self.start_bridge(StartOrigin::Manual);
                    }
                });
            });

            ui.add_space(6.0);
            if active {
                ui.label(egui::RichText::new(&self.status).small().color(
                    if self.status == "Running" {
                        colors.success
                    } else {
                        colors.muted
                    },
                ));
            } else if let Some(problem) = start_problem {
                ui.add(
                    egui::Label::new(egui::RichText::new(problem).small().color(colors.muted))
                        .wrap(),
                );
            } else {
                ui.label(
                    egui::RichText::new(
                        "Configuration ready. Start the bridge when you are ready.",
                    )
                    .small()
                    .color(colors.success),
                );
            }
        });
    }
}

impl eframe::App for AecBridgeApp {
    fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_background_work();

        #[cfg(target_os = "windows")]
        if let Some(tray) = &mut self.tray {
            tray.set_status(&self.status);
        }

        #[cfg(target_os = "windows")]
        let quitting = self.poll_tray_actions(context);
        #[cfg(not(target_os = "windows"))]
        let quitting = false;

        let minimized = context.input(|input| input.viewport().minimized == Some(true));
        match window_visibility_action(
            quitting,
            self.restore_window,
            self.settings.minimize_to_tray,
            self.tray_available(),
            self.window_hidden_to_tray,
            minimized,
            self.last_window_minimized,
        ) {
            WindowVisibilityAction::Restore => self.restore_main_window(context),
            WindowVisibilityAction::HideToTray => self.hide_window_to_tray(
                context,
                "Window minimized to the notification area; audio processing continues.",
            ),
            WindowVisibilityAction::None => {}
        }
        self.last_window_minimized = minimized;

        let repaint_ms = if self.bridge.is_some()
            || self.refresh_rx.is_some()
            || self.login_auto_start.is_waiting()
        {
            100
        } else {
            400
        };
        context.request_repaint_after(Duration::from_millis(repaint_ms));
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let previous_settings = self.settings.clone();
        let colors = palette(self.settings.dark_mode);
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(colors.background)
                    .inner_margin(egui::Margin::symmetric(24, 20)),
            )
            .show(ui, |ui| {
                self.render_header(ui, colors);
                ui.add_space(18.0);

                let action_reserve = 118.0;
                let body_height = (ui.available_height() - action_reserve).max(180.0);
                egui::ScrollArea::vertical()
                    .max_height(body_height)
                    .min_scrolled_height(body_height)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        if let Some(error) = &self.error {
                            callout(
                                ui,
                                colors.danger_soft,
                                colors.danger,
                                "Needs attention",
                                error,
                            );
                        }

                        if !self.started_streams.is_empty() || self.metrics.is_some() {
                            if self.error.is_some() {
                                ui.add_space(14.0);
                            }
                            self.render_session_details(ui, colors);
                        }

                        if self.error.is_some()
                            || !self.started_streams.is_empty()
                            || self.metrics.is_some()
                        {
                            ui.add_space(14.0);
                        }
                        self.render_input_card(ui, colors);
                        ui.add_space(14.0);
                        self.render_output_card(ui, colors);
                        ui.add_space(14.0);
                        self.render_startup_card(ui, colors);
                        ui.add_space(14.0);
                        self.render_advanced_card(ui, colors);
                        ui.add_space(14.0);
                        self.render_diagnostics_card(ui, colors);
                        ui.add_space(14.0);
                        self.render_signal_path_card(ui, colors);
                        ui.add_space(4.0);
                    });
                ui.add_space(14.0);
                self.render_action_bar(ui, colors);
            });

        if self.settings != previous_settings
            && let Some(storage) = frame.storage_mut()
        {
            eframe::set_value(storage, STORAGE_KEY, &self.settings);
            if settings_change_requires_flush(&previous_settings, &self.settings) {
                storage.flush();
            }
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, STORAGE_KEY, &self.settings);
    }
}

fn card<R>(
    ui: &mut egui::Ui,
    colors: Palette,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    egui::Frame::new()
        .fill(colors.surface)
        .stroke(egui::Stroke::new(1.0, colors.border))
        .corner_radius(12)
        .inner_margin(egui::Margin::same(18))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add_contents(ui)
        })
}

fn section_header(
    ui: &mut egui::Ui,
    colors: Palette,
    number: &str,
    title: &str,
    description: &str,
) {
    ui.horizontal(|ui| {
        egui::Frame::new()
            .fill(colors.accent_soft)
            .corner_radius(7)
            .inner_margin(egui::Margin::symmetric(8, 5))
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(number)
                        .size(11.0)
                        .strong()
                        .color(colors.accent),
                );
            });
        ui.add_space(4.0);
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(title).size(17.0).strong());
            ui.add(
                egui::Label::new(egui::RichText::new(description).small().color(colors.muted))
                    .wrap(),
            );
        });
    });
}

fn field_label(ui: &mut egui::Ui, colors: Palette, title: &str, description: &str) {
    ui.label(egui::RichText::new(title).strong());
    ui.label(egui::RichText::new(description).small().color(colors.muted));
    ui.add_space(4.0);
}

fn segmented_button(
    ui: &mut egui::Ui,
    colors: Palette,
    width: f32,
    label: &str,
    selected: bool,
) -> egui::Response {
    let (fill, stroke, text) = if selected {
        (colors.accent_soft, colors.accent, colors.accent)
    } else {
        (colors.field, colors.border, colors.text)
    };
    ui.add_sized(
        [width, 40.0],
        egui::Button::new(egui::RichText::new(label).strong().color(text))
            .fill(fill)
            .stroke(egui::Stroke::new(1.0, stroke))
            .corner_radius(8),
    )
}

fn callout(
    ui: &mut egui::Ui,
    background: egui::Color32,
    foreground: egui::Color32,
    title: &str,
    body: &str,
) {
    egui::Frame::new()
        .fill(background)
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(egui::RichText::new(title).strong().color(foreground));
            ui.add(egui::Label::new(egui::RichText::new(body).small().color(foreground)).wrap());
        });
}

fn brand_mark(ui: &mut egui::Ui, colors: Palette) {
    let (rect, _response) = ui.allocate_exact_size(egui::vec2(48.0, 48.0), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 12.0, colors.accent);

    let center = rect.center();
    let bars = [
        (-12.0, 9.0),
        (-6.0, 17.0),
        (0.0, 25.0),
        (6.0, 17.0),
        (12.0, 9.0),
    ];
    for (x, height) in bars {
        painter.line_segment(
            [
                egui::pos2(center.x + x, center.y - height / 2.0),
                egui::pos2(center.x + x, center.y + height / 2.0),
            ],
            egui::Stroke::new(2.5, egui::Color32::WHITE),
        );
    }
}

fn status_badge(
    ui: &mut egui::Ui,
    label: &str,
    foreground: egui::Color32,
    background: egui::Color32,
    border: egui::Color32,
) {
    egui::Frame::new()
        .fill(background)
        .stroke(egui::Stroke::new(1.0, border))
        .corner_radius(99)
        .inner_margin(egui::Margin::symmetric(11, 7))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                status_dot(ui, foreground);
                ui.label(egui::RichText::new(label).strong().color(foreground));
            });
        });
}

fn status_dot(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _response) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

fn route_step(ui: &mut egui::Ui, colors: Palette, number: &str, title: &str, detail: &str) {
    ui.horizontal(|ui| {
        let (rect, _response) =
            ui.allocate_exact_size(egui::vec2(24.0, 24.0), egui::Sense::hover());
        ui.painter()
            .circle_filled(rect.center(), 12.0, colors.accent_soft);
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            number,
            egui::FontId::proportional(11.0),
            colors.accent,
        );
        ui.add_space(4.0);
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(title).strong());
            ui.add(
                egui::Label::new(egui::RichText::new(detail).small().color(colors.muted)).wrap(),
            );
        });
    });
    ui.add_space(6.0);
}

fn saved_endpoint_name(selected: &Option<SavedEndpoint>, fallback: &str) -> String {
    selected
        .as_ref()
        .map(|endpoint| endpoint.last_name.clone())
        .unwrap_or_else(|| fallback.to_owned())
}

fn settings_change_requires_flush(
    previous: &PersistedSettings,
    current: &PersistedSettings,
) -> bool {
    previous.auto_start_bridge
        || current.auto_start_bridge
        || previous.minimize_to_tray != current.minimize_to_tray
        || previous.approved_handoff_id != current.approved_handoff_id
        || previous.handoff_render != current.handoff_render
}

fn endpoint_combo(
    ui: &mut egui::Ui,
    id_salt: &str,
    selected: &mut Option<SavedEndpoint>,
    endpoints: &[EndpointDescriptor],
    informational: bool,
) -> bool {
    let selected_text = match selected.as_ref() {
        None => "Not selected".to_owned(),
        Some(saved) => endpoints
            .iter()
            .find(|endpoint| endpoint.id == saved.id)
            .map(|endpoint| endpoint.name.clone())
            .unwrap_or_else(|| format!("Missing: {}", saved.last_name)),
    };
    let mut changed = false;
    egui::ComboBox::from_id_salt(id_salt)
        .width(ui.available_width())
        .selected_text(selected_text)
        .show_ui(ui, |ui| {
            if ui
                .selectable_label(selected.is_none(), "Not selected")
                .clicked()
            {
                changed |= selected.take().is_some();
            }
            for endpoint in endpoints {
                let compatible = informational || endpoint.compatible;
                let label = if compatible {
                    format!("{}  |  {}", endpoint.name, endpoint.format)
                } else {
                    format!("{}  |  {}  [unsupported]", endpoint.name, endpoint.format)
                };
                let is_selected = selected
                    .as_ref()
                    .is_some_and(|saved| saved.id == endpoint.id);
                if ui
                    .selectable_label(is_selected, label)
                    .on_hover_text(&endpoint.id)
                    .clicked()
                {
                    let next = SavedEndpoint::from_descriptor(endpoint);
                    if selected.as_ref() != Some(&next) {
                        *selected = Some(next);
                        changed = true;
                    }
                }
            }
        });
    changed
}

fn endpoint_readiness(
    label: &str,
    selected: Option<&SavedEndpoint>,
    endpoints: &[EndpointDescriptor],
    require_compatible: bool,
) -> StartReadiness {
    let Some(selected) = selected else {
        return StartReadiness::Blocked(format!("{label} is not selected."));
    };
    let Some(endpoint) = endpoints.iter().find(|endpoint| endpoint.id == selected.id) else {
        return StartReadiness::Waiting(format!(
            "{label} '{}' is not currently available.",
            selected.last_name
        ));
    };
    if require_compatible && !endpoint.compatible {
        if endpoint.format.starts_with("unavailable:") {
            return StartReadiness::Waiting(format!(
                "{label} '{}' is present, but its audio format is not ready yet.",
                endpoint.name
            ));
        }
        return StartReadiness::Blocked(format!(
            "{label} '{}' is not compatible: {}. This prototype requires 48 kHz f32; handoff render must be stereo.",
            endpoint.name, endpoint.format
        ));
    }
    StartReadiness::Ready
}

fn login_retry_delay(attempt: u8) -> Duration {
    Duration::from_secs(match attempt {
        0 | 1 => 1,
        2 => 2,
        3 => 4,
        _ => 5,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        EndpointDescriptor, LoginStartupWindowAction, PersistedSettings, SavedEndpoint,
        StartReadiness, WindowVisibilityAction, endpoint_readiness, login_retry_delay,
        login_startup_window_action, restore_after_fatal_bridge_error,
        settings_change_requires_flush, window_visibility_action,
    };
    use std::time::Duration;

    fn endpoint(id: &str, compatible: bool) -> EndpointDescriptor {
        EndpointDescriptor {
            id: id.to_owned(),
            name: "Test endpoint".to_owned(),
            format: "48 kHz f32 stereo".to_owned(),
            compatible,
        }
    }

    fn saved(id: &str) -> SavedEndpoint {
        SavedEndpoint {
            id: id.to_owned(),
            last_name: "Saved endpoint".to_owned(),
        }
    }

    #[test]
    fn exact_ready_endpoint_is_accepted() {
        assert_eq!(
            endpoint_readiness(
                "Reference",
                Some(&saved("exact")),
                &[endpoint("exact", true)],
                true
            ),
            StartReadiness::Ready
        );
    }

    #[test]
    fn missing_saved_endpoint_is_retryable_without_fallback() {
        let readiness = endpoint_readiness(
            "Reference",
            Some(&saved("missing")),
            &[endpoint("different", true)],
            true,
        );
        assert!(
            matches!(readiness, StartReadiness::Waiting(message) if message.contains("not currently available"))
        );
    }

    #[test]
    fn incompatible_exact_endpoint_is_terminal() {
        let readiness = endpoint_readiness(
            "Reference",
            Some(&saved("exact")),
            &[endpoint("exact", false)],
            true,
        );
        assert!(
            matches!(readiness, StartReadiness::Blocked(message) if message.contains("not compatible"))
        );
    }

    #[test]
    fn temporarily_unavailable_format_is_retryable() {
        let mut pending = endpoint("exact", false);
        pending.format = "unavailable: device is initializing".to_owned();
        let readiness = endpoint_readiness("Reference", Some(&saved("exact")), &[pending], true);
        assert!(
            matches!(readiness, StartReadiness::Waiting(message) if message.contains("not ready yet"))
        );
    }

    #[test]
    fn login_retry_backoff_caps_at_five_seconds() {
        assert_eq!(login_retry_delay(1), Duration::from_secs(1));
        assert_eq!(login_retry_delay(2), Duration::from_secs(2));
        assert_eq!(login_retry_delay(3), Duration::from_secs(4));
        assert_eq!(login_retry_delay(4), Duration::from_secs(5));
        assert_eq!(login_retry_delay(20), Duration::from_secs(5));
    }

    #[test]
    fn unattended_route_and_approval_changes_require_immediate_flush() {
        let mut previous = PersistedSettings::default();
        let mut current = previous.clone();
        current.approved_handoff_id = Some("cable".to_owned());
        assert!(settings_change_requires_flush(&previous, &current));

        previous = current.clone();
        current.auto_start_bridge = true;
        assert!(settings_change_requires_flush(&previous, &current));

        previous = current.clone();
        current.microphone = Some(saved("new-microphone"));
        assert!(settings_change_requires_flush(&previous, &current));

        let previous = PersistedSettings::default();
        let mut current = previous.clone();
        current.minimize_to_tray = true;
        assert!(settings_change_requires_flush(&previous, &current));
    }

    #[test]
    fn cosmetic_change_without_unattended_start_can_wait_for_normal_save() {
        let previous = PersistedSettings::default();
        let mut current = previous.clone();
        current.dark_mode = !current.dark_mode;
        assert!(!settings_change_requires_flush(&previous, &current));
    }

    #[test]
    fn notification_area_mode_is_opt_in() {
        let settings = PersistedSettings::default();
        assert!(!settings.minimize_to_tray);
        assert_eq!(
            window_visibility_action(
                false,
                false,
                settings.minimize_to_tray,
                true,
                false,
                true,
                false,
            ),
            WindowVisibilityAction::None
        );
    }

    #[test]
    fn newly_minimized_window_hides_only_with_a_working_tray() {
        assert_eq!(
            window_visibility_action(false, false, true, true, false, true, false),
            WindowVisibilityAction::HideToTray
        );
        assert_eq!(
            window_visibility_action(false, false, true, false, false, true, false),
            WindowVisibilityAction::None
        );
    }

    #[test]
    fn restore_wins_over_stale_minimized_state() {
        assert_eq!(
            window_visibility_action(false, true, true, true, true, true, true),
            WindowVisibilityAction::Restore
        );
        assert_eq!(
            window_visibility_action(true, true, true, true, true, true, true),
            WindowVisibilityAction::None
        );
    }

    #[test]
    fn login_start_uses_tray_only_when_it_is_available() {
        assert_eq!(
            login_startup_window_action(true),
            LoginStartupWindowAction::HideToTray
        );
        assert_eq!(
            login_startup_window_action(false),
            LoginStartupWindowAction::MinimizeToTaskbar
        );
    }

    #[test]
    fn fatal_bridge_error_restores_a_manually_hidden_window() {
        assert!(restore_after_fatal_bridge_error(false, true));
        assert!(restore_after_fatal_bridge_error(true, false));
        assert!(!restore_after_fatal_bridge_error(false, false));
    }
}
