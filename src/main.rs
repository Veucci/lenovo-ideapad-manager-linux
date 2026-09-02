use eframe::egui;
use rusb::{Context, UsbContext};
use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

const VENDOR_ID: u16 = 0x048d;
const PRODUCT_ID: u16 = 0xc963;
const CONFIG_DIR: &str = "lenovo-ideapad-manager";
const PROFILE_PATH: &str = "/sys/firmware/acpi/platform_profile";
const PROFILE_CHOICES_PATH: &str = "/sys/firmware/acpi/platform_profile_choices";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Effect {
    Static,
    Breath,
    Wave,
    Hue,
    Off,
}

impl Effect {
    fn label(self) -> &'static str {
        match self {
            Self::Static => "Static",
            Self::Breath => "Breath",
            Self::Wave => "Wave",
            Self::Hue => "Hue",
            Self::Off => "Off",
        }
    }

    fn code(self) -> u8 {
        match self {
            Self::Static => 1,
            Self::Breath => 3,
            Self::Wave => 4,
            Self::Hue => 6,
            Self::Off => 1,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    LeftToRight,
    RightToLeft,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Keyboard,
    FanControl,
}

impl Direction {
    fn label(self) -> &'static str {
        match self {
            Self::LeftToRight => "Left to right",
            Self::RightToLeft => "Right to left",
        }
    }
}

struct Settings {
    effect: Effect,
    colors: [String; 4],
    speed: u8,
    brightness: u8,
    direction: Direction,
    profile: String,
    autostart: bool,
}

struct ThermalStatus {
    profile: String,
    choices: Vec<String>,
    temperature: Option<f32>,
    fan_rpm: Option<u32>,
}

impl ThermalStatus {
    fn read() -> Self {
        Self {
            profile: read_trimmed(PROFILE_PATH).unwrap_or_else(|| "Unsupported".into()),
            choices: read_trimmed(PROFILE_CHOICES_PATH)
                .map(|value| value.split_whitespace().map(str::to_string).collect())
                .unwrap_or_default(),
            temperature: read_sensor("temp", "_input").map(|value| value / 1000.0),
            fan_rpm: read_sensor("fan", "_input").map(|value| value as u32),
        }
    }
}

fn read_trimmed(path: &str) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
}

fn read_sensor(prefix: &str, suffix: &str) -> Option<f32> {
    let entries = fs::read_dir("/sys/class/hwmon").ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let files = fs::read_dir(path).ok()?;
        for file in files.flatten() {
            let name = file.file_name().to_string_lossy().to_string();
            if name.starts_with(prefix)
                && name.ends_with(suffix)
                && let Ok(value) = fs::read_to_string(file.path()).ok()?.trim().parse::<f32>()
            {
                return Some(value);
            }
        }
    }
    None
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            effect: Effect::Static,
            colors: [
                "ffffff".into(),
                "ffffff".into(),
                "ffffff".into(),
                "ffffff".into(),
            ],
            speed: 1,
            brightness: 1,
            direction: Direction::LeftToRight,
            profile: "balanced".into(),
            autostart: false,
        }
    }
}

fn config_path() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|path| path.join(CONFIG_DIR).join("settings"))
}

fn parse_color(input: &str) -> Result<[u8; 3], String> {
    let value = input.trim().to_lowercase();
    if value.len() == 6 && value.chars().all(|character| character.is_ascii_hexdigit()) {
        return (0..3)
            .map(|index| {
                u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
                    .map_err(|_| "Invalid HEX color".to_string())
            })
            .collect::<Result<Vec<_>, _>>()
            .and_then(|values| {
                values
                    .try_into()
                    .map_err(|_| "Invalid HEX color".to_string())
            });
    }

    let parts: Vec<&str> = value.split(',').map(str::trim).collect();
    if parts.len() != 3 {
        return Err(format!("Invalid color: {input}"));
    }
    if parts.iter().all(|part| part.parse::<u8>().is_ok()) {
        return Ok([
            parts[0].parse().unwrap_or_default(),
            parts[1].parse().unwrap_or_default(),
            parts[2].parse().unwrap_or_default(),
        ]);
    }
    let hsv: Result<Vec<f32>, _> = parts.iter().map(|part| part.parse::<f32>()).collect();
    let values = hsv.map_err(|_| format!("Invalid color: {input}"))?;
    if values.iter().any(|value| !(0.0..=1.0).contains(value)) {
        return Err(format!("HSV values must be between 0 and 1: {input}"));
    }
    let (h, s, v) = (values[0], values[1], values[2]);
    let sector = h * 6.0;
    let index = sector.floor() as u8;
    let fraction = sector - f32::from(index);
    let p = v * (1.0 - s);
    let q = v * (1.0 - fraction * s);
    let t = v * (1.0 - (1.0 - fraction) * s);
    let rgb = match index % 6 {
        0 => [v, t, p],
        1 => [q, v, p],
        2 => [p, v, t],
        3 => [p, q, v],
        4 => [t, p, v],
        _ => [v, p, q],
    };
    Ok(rgb.map(|value| (value * 255.0) as u8))
}

fn build_payload(settings: &Settings) -> Result<Vec<u8>, String> {
    if settings.effect == Effect::Off {
        return Ok([vec![204, 22, 1], vec![0; 30]].concat());
    }
    let mut data = vec![
        204,
        22,
        settings.effect.code(),
        settings.speed,
        settings.brightness,
    ];
    if matches!(settings.effect, Effect::Static | Effect::Breath) {
        let mut parsed = Vec::with_capacity(12);
        for color in &settings.colors {
            parsed.extend(parse_color(color)?);
        }
        data.extend(parsed);
    } else {
        data.extend([0; 12]);
    }
    data.push(0);
    match (settings.effect, settings.direction) {
        (Effect::Wave, Direction::RightToLeft) => data.extend([1, 0]),
        (Effect::Wave, Direction::LeftToRight) => data.extend([0, 1]),
        _ => data.extend([0, 0]),
    }
    data.extend([0; 13]);
    Ok(data)
}

fn set_platform_profile(profile: &str) -> Result<(), String> {
    let choices = read_trimmed(PROFILE_CHOICES_PATH)
        .ok_or_else(|| "Thermal profiles are not available".to_string())?;
    if !choices.split_whitespace().any(|choice| choice == profile) {
        return Err("Selected thermal profile is not available".into());
    }
    fs::write(PROFILE_PATH, profile).map_err(|error| error.to_string())
}

fn send_payload(payload: &[u8]) -> Result<(), String> {
    let context = Context::new().map_err(|error| error.to_string())?;
    let device = context
        .devices()
        .map_err(|error| error.to_string())?
        .iter()
        .find(|device| {
            device
                .device_descriptor()
                .map(|descriptor| {
                    descriptor.vendor_id() == VENDOR_ID && descriptor.product_id() == PRODUCT_ID
                })
                .unwrap_or(false)
        })
        .ok_or_else(|| {
            format!("USB keyboard light device {VENDOR_ID:04x}:{PRODUCT_ID:04x} not found")
        })?;
    let handle = device.open().map_err(|error| error.to_string())?;
    if handle.kernel_driver_active(0).unwrap_or(false) {
        handle
            .detach_kernel_driver(0)
            .map_err(|error| error.to_string())?;
    }
    handle
        .claim_interface(0)
        .map_err(|error| error.to_string())?;
    handle
        .write_control(0x21, 0x09, 0x03cc, 0, payload, Duration::from_secs(2))
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn payload_hex(payload: &[u8]) -> String {
    payload.iter().map(|byte| format!("{byte:02x}")).collect()
}

struct PrivilegedSession {
    _process: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl PrivilegedSession {
    fn start() -> Result<Self, String> {
        let executable = env::current_exe().map_err(|error| error.to_string())?;
        let mut process = Command::new("pkexec")
            .arg(executable)
            .arg("--privileged-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| {
                format!("Permission denied and pkexec could not be started: {error}")
            })?;
        let input = process
            .stdin
            .take()
            .ok_or_else(|| "Unable to open the privileged helper input".to_string())?;
        let output = process
            .stdout
            .take()
            .ok_or_else(|| "Unable to open the privileged helper output".to_string())?;
        Ok(Self {
            _process: process,
            input,
            output: BufReader::new(output),
        })
    }

    fn send_command(&mut self, command: &str) -> Result<(), String> {
        writeln!(self.input, "{command}").map_err(|error| error.to_string())?;
        self.input.flush().map_err(|error| error.to_string())?;
        let mut response = String::new();
        self.output
            .read_line(&mut response)
            .map_err(|error| error.to_string())?;
        if response.is_empty() {
            return Err("The privileged helper stopped unexpectedly".into());
        }
        if response.trim() == "OK" {
            Ok(())
        } else {
            Err(response
                .strip_prefix("ERR ")
                .unwrap_or(response.trim())
                .to_string())
        }
    }

    fn send(&mut self, payload: &[u8]) -> Result<(), String> {
        self.send_command(&payload_hex(payload))
    }

    fn set_profile(&mut self, profile: &str) -> Result<(), String> {
        self.send_command(&format!("PROFILE {profile}"))
    }
}

fn set_profile_with_privilege(
    profile: &str,
    session: &mut Option<PrivilegedSession>,
) -> Result<(), String> {
    if let Some(active_session) = session {
        return active_session.set_profile(profile);
    }
    match set_platform_profile(profile) {
        Ok(()) => Ok(()),
        Err(error)
            if error.to_lowercase().contains("access denied")
                || error.to_lowercase().contains("permission denied")
                || error.to_lowercase().contains("operation not permitted") =>
        {
            let mut new_session = PrivilegedSession::start()?;
            let result = new_session.set_profile(profile);
            if result.is_ok() {
                *session = Some(new_session);
            }
            result
        }
        Err(error) => Err(error),
    }
}

fn send_payload_with_privilege(
    payload: &[u8],
    session: &mut Option<PrivilegedSession>,
) -> Result<(), String> {
    if let Some(active_session) = session {
        match active_session.send(payload) {
            Ok(()) => return Ok(()),
            Err(_) => *session = None,
        }
    }
    match send_payload(payload) {
        Ok(()) => Ok(()),
        Err(error)
            if error.to_lowercase().contains("access denied")
                || error.to_lowercase().contains("permission denied")
                || error.to_lowercase().contains("insufficient permissions") =>
        {
            let mut new_session = PrivilegedSession::start()?;
            let result = new_session.send(payload);
            if result.is_ok() {
                *session = Some(new_session);
            }
            result
        }
        Err(error) => Err(error),
    }
}

fn run_privileged_server() -> Result<(), String> {
    let input = io::stdin();
    let mut output = io::BufWriter::new(io::stdout());
    for line in input.lock().lines() {
        let response = match line {
            Ok(value) => {
                let command = value.trim();
                let result = if let Some(profile) = command.strip_prefix("PROFILE ") {
                    set_platform_profile(profile)
                } else {
                    decode_payload(command).and_then(|payload| send_payload(&payload))
                };
                match result {
                    Ok(()) => "OK".to_string(),
                    Err(error) => format!("ERR {error}"),
                }
            }
            Err(error) => format!("ERR {error}"),
        };
        writeln!(output, "{response}").map_err(|error| error.to_string())?;
        output.flush().map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn decode_payload(value: &str) -> Result<Vec<u8>, String> {
    if !value.len().is_multiple_of(2) {
        return Err("Invalid payload".into());
    }
    (0..value.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&value[index..index + 2], 16).map_err(|_| "Invalid payload".into())
        })
        .collect()
}

fn settings_to_text(settings: &Settings) -> String {
    format!(
        "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
        settings.effect.label(),
        settings.colors[0],
        settings.colors[1],
        settings.colors[2],
        settings.colors[3],
        settings.speed,
        settings.brightness,
        settings.direction.label(),
        settings.autostart
    )
}

fn save_settings(settings: &Settings) -> Result<(), String> {
    let path = config_path()
        .ok_or_else(|| "Unable to locate the user configuration directory".to_string())?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    fs::write(path, settings_to_text(settings)).map_err(|error| error.to_string())
}

fn install_autostart(enabled: bool) -> Result<(), String> {
    let config_home = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok_or_else(|| "Unable to locate the user configuration directory".to_string())?;
    let path = config_home
        .join("autostart")
        .join("lenovo-ideapad-manager.desktop");
    if !enabled {
        if path.exists() {
            fs::remove_file(path).map_err(|error| error.to_string())?;
        }
        return Ok(());
    }
    let executable = env::current_exe().map_err(|error| error.to_string())?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let content = format!(
        "[Desktop Entry]\nType=Application\nName=Lenovo Ideapad Manager\nExec=pkexec \"{}\" --apply-saved\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
        executable.display()
    );
    fs::write(path, content).map_err(|error| error.to_string())
}

fn load_settings() -> Settings {
    let Some(path) = config_path() else {
        return Settings::default();
    };
    let Ok(content) = fs::read_to_string(path) else {
        return Settings::default();
    };
    let values: Vec<&str> = content.lines().collect();
    let mut settings = Settings::default();
    if let Some(value) = values.first() {
        settings.effect = match *value {
            "Static" => Effect::Static,
            "Breath" => Effect::Breath,
            "Wave" => Effect::Wave,
            "Hue" => Effect::Hue,
            "Off" => Effect::Off,
            _ => settings.effect,
        };
    }
    for (index, color) in settings.colors.iter_mut().enumerate() {
        if let Some(value) = values.get(index + 1) {
            *color = (*value).to_string();
        }
    }
    if let Some(value) = values.get(5).and_then(|value| value.parse::<u8>().ok()) {
        settings.speed = value.clamp(1, 4);
    }
    if let Some(value) = values.get(6).and_then(|value| value.parse::<u8>().ok()) {
        settings.brightness = value.clamp(1, 2);
    }
    if let Some(value) = values.get(7) {
        settings.direction = if *value == "Right to left" {
            Direction::RightToLeft
        } else {
            Direction::LeftToRight
        };
    }
    if values.len() >= 10 {
        if let Some(value) = values.get(8) {
            settings.profile = (*value).to_string();
        }
        if let Some(value) = values.get(9).and_then(|value| value.parse().ok()) {
            settings.autostart = value;
        }
    } else if let Some(value) = values.get(8).and_then(|value| value.parse().ok()) {
        settings.autostart = value;
    }
    settings
}

fn color_to_hex(color: egui::Color32) -> String {
    format!("{:02x}{:02x}{:02x}", color.r(), color.g(), color.b())
}

fn preview_colors(settings: &Settings) -> [egui::Color32; 4] {
    if settings.effect == Effect::Off {
        return [egui::Color32::from_rgb(18, 21, 28); 4];
    }
    if settings.effect == Effect::Hue {
        return [
            egui::Color32::from_rgb(255, 45, 95),
            egui::Color32::from_rgb(255, 190, 45),
            egui::Color32::from_rgb(55, 220, 155),
            egui::Color32::from_rgb(90, 130, 255),
        ];
    }
    if settings.effect == Effect::Wave {
        return [
            egui::Color32::from_rgb(255, 55, 100),
            egui::Color32::from_rgb(255, 180, 50),
            egui::Color32::from_rgb(60, 220, 155),
            egui::Color32::from_rgb(90, 130, 255),
        ];
    }
    std::array::from_fn(|index| {
        parse_color(&settings.colors[index])
            .map(|rgb| egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]))
            .unwrap_or(egui::Color32::from_rgb(80, 85, 98))
    })
}

fn draw_keyboard(ui: &mut egui::Ui, settings: &Settings) {
    let width = ui.available_width().max(420.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 198.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 16.0, egui::Color32::from_rgb(20, 24, 34));
    painter.rect_stroke(
        rect,
        16.0,
        egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(55, 63, 79)),
    );
    let colors = preview_colors(settings);
    let zone_width = rect.width() / 4.0;
    for (zone, color) in colors.iter().enumerate() {
        let zone_rect = egui::Rect::from_min_max(
            egui::pos2(
                rect.left() + zone as f32 * zone_width + 10.0,
                rect.top() + 11.0,
            ),
            egui::pos2(
                rect.left() + (zone + 1) as f32 * zone_width - 10.0,
                rect.bottom() - 11.0,
            ),
        );
        painter.rect_filled(zone_rect, 9.0, color.linear_multiply(0.12));
        let key_rows = [10, 10, 9, 8, 6];
        let row_height = 24.0;
        for (row, count) in key_rows.iter().enumerate() {
            let gap = 3.0;
            let key_width = (zone_rect.width() - gap * (*count as f32 - 1.0)) / *count as f32;
            let y = zone_rect.top() + 10.0 + row as f32 * row_height;
            for key in 0..*count {
                let x = zone_rect.left() + key as f32 * (key_width + gap);
                let key_rect =
                    egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(key_width, 17.0));
                painter.rect_filled(key_rect, 3.0, color.linear_multiply(0.72));
            }
        }
        painter.text(
            egui::pos2(zone_rect.center().x, zone_rect.bottom() - 4.0),
            egui::Align2::CENTER_BOTTOM,
            format!("ZONE {}", zone + 1),
            egui::FontId::proportional(10.0),
            color.gamma_multiply(0.85),
        );
    }
}

fn profile_animation_speed(profile: &str) -> f32 {
    let normalized = profile.to_ascii_lowercase();
    if normalized.contains("low") || normalized.contains("quiet") {
        0.45
    } else if normalized.contains("performance") || normalized.contains("extreme") {
        4.2
    } else {
        1.4
    }
}

fn draw_fan_card(ui: &mut egui::Ui, label: &str, status: &ThermalStatus, phase: f32) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.vertical_centered(|ui| {
            ui.label(egui::RichText::new(label).strong());
            let (rect, _) = ui.allocate_exact_size(egui::vec2(280.0, 250.0), egui::Sense::hover());
            let painter = ui.painter_at(rect);
            let center = rect.center();
            let outer = 92.0;
            painter.circle_filled(center, outer, egui::Color32::from_rgb(18, 24, 34));
            painter.circle_stroke(
                center,
                outer,
                egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(61, 76, 96)),
            );
            painter.circle_stroke(
                center,
                74.0,
                egui::Stroke::new(8.0_f32, egui::Color32::from_rgb(34, 48, 62)),
            );
            painter.circle_stroke(
                center,
                57.0,
                egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(77, 199, 190)),
            );
            for blade in 0..14 {
                let angle = phase + blade as f32 * std::f32::consts::TAU / 14.0;
                let tip = center + egui::vec2(angle.cos(), angle.sin()) * 69.0;
                let shoulder =
                    center + egui::vec2((angle - 0.24).cos(), (angle - 0.24).sin()) * 29.0;
                let leading =
                    center + egui::vec2((angle - 0.08).cos(), (angle - 0.08).sin()) * 65.0;
                let trailing =
                    center + egui::vec2((angle + 0.28).cos(), (angle + 0.28).sin()) * 45.0;
                painter.add(egui::Shape::convex_polygon(
                    vec![center, leading, tip, trailing, shoulder],
                    egui::Color32::from_rgb(77, 199, 190),
                    egui::Stroke::NONE,
                ));
            }
            painter.circle_filled(center, 30.0, egui::Color32::from_rgb(24, 38, 49));
            painter.circle_filled(center, 25.0, egui::Color32::from_rgb(145, 231, 208));
            painter.circle_stroke(
                center,
                25.0,
                egui::Stroke::new(3.0_f32, egui::Color32::from_rgb(33, 65, 73)),
            );
            ui.label(status.temperature.map_or("Temperature --".into(), |value| {
                format!("Temperature {value:.1}°C")
            }));
            ui.label(
                status
                    .fan_rpm
                    .map_or("RPM --".into(), |value| format!("RPM {value}")),
            );
        });
    });
}

struct App {
    settings: Settings,
    thermal: ThermalStatus,
    tab: Tab,
    status: String,
    last_applied_signature: String,
    privileged_session: Option<PrivilegedSession>,
    animation_started: Instant,
    last_thermal_refresh: Instant,
}

impl App {
    fn apply(&mut self, persist: bool) {
        let payload = match build_payload(&self.settings) {
            Ok(payload) => payload,
            Err(error) => {
                self.status = error;
                return;
            }
        };
        if persist && let Err(error) = save_settings(&self.settings) {
            self.status = error;
            return;
        }
        if let Err(error) = send_payload_with_privilege(&payload, &mut self.privileged_session) {
            self.status = error;
            return;
        }
        self.status = if persist {
            "Settings saved and hardware updated".into()
        } else {
            "Hardware updated".into()
        };
        self.last_applied_signature = settings_to_text(&self.settings);
    }

    fn apply_profile(&mut self) {
        if self.settings.profile == self.thermal.profile {
            self.status = "Thermal profile is already active".into();
            return;
        }
        match set_profile_with_privilege(&self.settings.profile, &mut self.privileged_session) {
            Ok(()) => {
                self.thermal.profile = self.settings.profile.clone();
                self.status = save_settings(&self.settings)
                    .map(|_| "Thermal profile updated".into())
                    .unwrap_or_else(|error| error);
            }
            Err(error) => self.status = error,
        }
    }

    fn auto_apply(&mut self) {
        let signature = settings_to_text(&self.settings);
        if signature != self.last_applied_signature && build_payload(&self.settings).is_ok() {
            self.apply(false);
        }
        self.last_applied_signature = signature;
    }

    fn reset(&mut self) {
        let autostart = self.settings.autostart;
        self.settings = Settings {
            autostart,
            ..Settings::default()
        };
        self.status = "Preview reset".into();
    }
}

impl eframe::App for App {
    fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        if self.last_thermal_refresh.elapsed() >= Duration::from_secs(1) {
            self.thermal = ThermalStatus::read();
            self.last_thermal_refresh = Instant::now();
        }
        context.request_repaint_after(Duration::from_millis(32));
        egui::CentralPanel::default().show(context, |ui| {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.columns(2, |columns| {
                    let keyboard_fill = if self.tab == Tab::Keyboard {
                        egui::Color32::from_rgb(59, 91, 112)
                    } else {
                        egui::Color32::from_rgb(31, 38, 51)
                    };
                    let fan_fill = if self.tab == Tab::FanControl {
                        egui::Color32::from_rgb(59, 91, 112)
                    } else {
                        egui::Color32::from_rgb(31, 38, 51)
                    };
                    let tab_width = columns[0].available_width();
                    if columns[0]
                        .add_sized(
                            [tab_width, 38.0],
                            egui::Button::new(egui::RichText::new("Keyboard Light").strong())
                                .fill(keyboard_fill),
                        )
                        .clicked()
                    {
                        self.tab = Tab::Keyboard;
                    }
                    if columns[1]
                        .add_sized(
                            [tab_width, 38.0],
                            egui::Button::new(egui::RichText::new("Fan Control").strong())
                                .fill(fan_fill),
                        )
                        .clicked()
                    {
                        self.settings.profile = self.thermal.profile.clone();
                        self.tab = Tab::FanControl;
                    }
                });
            });
            if self.tab == Tab::Keyboard {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.heading("Lenovo Ideapad Manager");
                    ui.label(
                        egui::RichText::new("LIVE PREVIEW")
                            .small()
                            .strong()
                            .color(egui::Color32::from_rgb(95, 210, 170)),
                    );
                });
                ui.label(
                    egui::RichText::new("Design your lighting before sending it to the keyboard")
                        .color(egui::Color32::from_rgb(155, 164, 182)),
                );
                ui.add_space(10.0);
                draw_keyboard(ui, &self.settings);
                ui.add_space(10.0);
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("EFFECT").strong());
                        egui::ComboBox::from_id_salt("effect")
                            .selected_text(self.settings.effect.label())
                            .width(120.0)
                            .show_ui(ui, |ui| {
                                for effect in [
                                    Effect::Static,
                                    Effect::Breath,
                                    Effect::Wave,
                                    Effect::Hue,
                                    Effect::Off,
                                ] {
                                    ui.selectable_value(
                                        &mut self.settings.effect,
                                        effect,
                                        effect.label(),
                                    );
                                }
                            });
                        if self.settings.effect != Effect::Off {
                            ui.label("Speed");
                            ui.add(
                                egui::Slider::new(&mut self.settings.speed, 1..=4).show_value(true),
                            );
                            ui.label("Brightness");
                            ui.add(
                                egui::Slider::new(&mut self.settings.brightness, 1..=2)
                                    .show_value(true),
                            );
                        }
                    });
                    if self.settings.effect == Effect::Wave {
                        ui.horizontal(|ui| {
                            ui.label("Direction");
                            egui::ComboBox::from_id_salt("direction")
                                .selected_text(self.settings.direction.label())
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut self.settings.direction,
                                        Direction::LeftToRight,
                                        Direction::LeftToRight.label(),
                                    );
                                    ui.selectable_value(
                                        &mut self.settings.direction,
                                        Direction::RightToLeft,
                                        Direction::RightToLeft.label(),
                                    );
                                });
                        });
                    }
                });
                if matches!(self.settings.effect, Effect::Static | Effect::Breath) {
                    ui.add_space(8.0);
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.label(egui::RichText::new("KEYBOARD ZONES").strong());
                        ui.label(
                            egui::RichText::new("Pick a color or enter HEX, RGB, or HSV")
                                .small()
                                .color(egui::Color32::from_rgb(155, 164, 182)),
                        );
                        for index in 0..4 {
                            ui.horizontal(|ui| {
                                ui.label(format!("Zone {}", index + 1));
                                let mut color = parse_color(&self.settings.colors[index])
                                    .map(|rgb| egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]))
                                    .unwrap_or(egui::Color32::from_rgb(80, 85, 98));
                                if egui::color_picker::color_edit_button_srgba(
                                    ui,
                                    &mut color,
                                    egui::color_picker::Alpha::Opaque,
                                )
                                .changed()
                                {
                                    self.settings.colors[index] = color_to_hex(color);
                                }
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.settings.colors[index])
                                        .desired_width(170.0),
                                );
                                ui.colored_label(color, "●");
                            });
                        }
                    });
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let mut autostart = self.settings.autostart;
                    if ui
                        .checkbox(&mut autostart, "Apply once at startup")
                        .changed()
                    {
                        self.settings.autostart = autostart;
                        match install_autostart(autostart)
                            .and_then(|_| save_settings(&self.settings))
                        {
                            Ok(()) => self.status = "Startup setting saved".into(),
                            Err(error) => self.status = error,
                        }
                    }
                });
                ui.add_space(6.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_sized(
                            [120.0, 38.0],
                            egui::Button::new(egui::RichText::new("APPLY").strong().size(15.0)),
                        )
                        .clicked()
                    {
                        self.apply(true);
                    }
                    if ui
                        .add_sized(
                            [96.0, 38.0],
                            egui::Button::new(egui::RichText::new("RESET").size(14.0)),
                        )
                        .clicked()
                    {
                        self.reset();
                    }
                    if ui
                        .add_sized(
                            [96.0, 38.0],
                            egui::Button::new(egui::RichText::new("SAVE").size(14.0)),
                        )
                        .clicked()
                    {
                        self.status = save_settings(&self.settings)
                            .err()
                            .unwrap_or_else(|| "Settings saved".into());
                    }
                });
                ui.add_space(3.0);
                ui.label(
                    egui::RichText::new(&self.status)
                        .small()
                        .color(egui::Color32::from_rgb(145, 153, 170)),
                );
            } else {
                ui.add_space(14.0);
                ui.heading("Fan Control");
                ui.label(
                    egui::RichText::new("Manage thermal modes and monitor system cooling")
                        .color(egui::Color32::from_rgb(155, 164, 182)),
                );
                ui.add_space(18.0);
                let rotation_speed = profile_animation_speed(&self.settings.profile);
                ui.columns(2, |columns| {
                    draw_fan_card(
                        &mut columns[0],
                        "FAN 1",
                        &self.thermal,
                        self.animation_started.elapsed().as_secs_f32() * rotation_speed,
                    );
                    draw_fan_card(
                        &mut columns[1],
                        "FAN 2",
                        &self.thermal,
                        self.animation_started.elapsed().as_secs_f32() * rotation_speed + 0.8,
                    );
                });
                ui.add_space(16.0);
                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new("THERMAL MODE").strong());
                    let previous_profile = self.settings.profile.clone();
                    let choices: Vec<String> =
                        self.thermal.choices.iter().take(4).cloned().collect();
                    if choices.is_empty() {
                        ui.label("No thermal profiles detected");
                    } else {
                        ui.horizontal(|ui| {
                            for choice in &choices {
                                let selected = self.settings.profile == *choice;
                                let fill = if selected {
                                    egui::Color32::from_rgb(67, 133, 125)
                                } else {
                                    egui::Color32::from_rgb(31, 38, 51)
                                };
                                let text = egui::RichText::new(choice).strong().size(14.0);
                                if ui
                                    .add_sized([150.0, 52.0], egui::Button::new(text).fill(fill))
                                    .clicked()
                                {
                                    self.settings.profile = choice.clone();
                                }
                            }
                        });
                        if previous_profile != self.settings.profile {
                            self.apply_profile();
                        }
                    }
                });
                ui.add_space(14.0);
                ui.label(
                    egui::RichText::new(&self.status)
                        .small()
                        .color(egui::Color32::from_rgb(145, 153, 170)),
                );
            }
        });
        self.auto_apply();
    }
}

fn main() -> eframe::Result<()> {
    let arguments: Vec<String> = env::args().collect();
    let mut settings = load_settings();
    let thermal = ThermalStatus::read();
    if !thermal
        .choices
        .iter()
        .any(|choice| choice == &settings.profile)
        && !thermal.profile.is_empty()
        && thermal.profile != "Unsupported"
    {
        settings.profile = thermal.profile.clone();
    }
    let initial_signature = settings_to_text(&settings);
    if arguments
        .iter()
        .any(|argument| argument == "--privileged-server")
    {
        return run_privileged_server()
            .map_err(|error| eframe::Error::AppCreation(Box::new(std::io::Error::other(error))));
    }
    if arguments
        .iter()
        .any(|argument| argument == "--apply-payload")
    {
        let payload = arguments
            .iter()
            .position(|argument| argument == "--apply-payload")
            .and_then(|index| arguments.get(index + 1))
            .ok_or_else(|| {
                eframe::Error::AppCreation(Box::new(std::io::Error::other("Missing payload")))
            })?;
        return decode_payload(payload)
            .and_then(|payload| send_payload(&payload))
            .map_err(|error| eframe::Error::AppCreation(Box::new(std::io::Error::other(error))));
    }
    if arguments.iter().any(|argument| argument == "--apply-saved") {
        return build_payload(&settings)
            .map_err(|error| eframe::Error::AppCreation(Box::new(std::io::Error::other(error))))
            .and_then(|payload| {
                send_payload(&payload).map_err(|error| {
                    eframe::Error::AppCreation(Box::new(std::io::Error::other(error)))
                })
            })
            .and_then(|_| {
                set_platform_profile(&settings.profile).map_err(|error| {
                    eframe::Error::AppCreation(Box::new(std::io::Error::other(error)))
                })
            });
    }
    eframe::run_native(
        "Lenovo Ideapad Manager",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([1120.0, 760.0])
                .with_min_inner_size([860.0, 620.0]),
            ..Default::default()
        },
        Box::new(|_creation_context| {
            Ok(Box::new(App {
                settings,
                thermal,
                tab: Tab::Keyboard,
                status: "Ready".into(),
                last_applied_signature: initial_signature,
                privileged_session: None,
                animation_started: Instant::now(),
                last_thermal_refresh: Instant::now(),
            }))
        }),
    )
}
