use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

const DEFAULT_BATTERY_THRESHOLD: u8 = 20;
const DEFAULT_ALERT_COOLDOWN_MINUTES: u64 = 360;
const DEFAULT_NOTIFICATION_VOLUME: u8 = 60;
const DEFAULT_REASSERT_INTERVAL_SECONDS: u64 = 300;
const DEFAULT_VERIFY_INTERVAL_SECONDS: u64 = 60;
const MIN_VERIFY_INTERVAL_SECONDS: u64 = 15;
/// A watchdog faster than this would keep the device bus and the Node helper busy for nothing.
const MIN_REASSERT_INTERVAL_SECONDS: u64 = 30;
const OFFICIAL_ORIGINS: &[&str] = &[
    "https://control.openmouse.app",
    "https://dev.openmouse.app",
    "https://openmouse.app",
    "https://www.openmouse.app",
];

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeConfig {
    #[serde(default = "default_battery_threshold")]
    pub battery_threshold_percent: u8,
    #[serde(default = "default_alert_cooldown")]
    pub alert_cooldown_minutes: u64,
    #[serde(default)]
    pub automatic_updates: bool,
    /// Whether the on-screen banner plays a chime, and how loud (0–100).
    #[serde(default = "enabled_by_default")]
    pub notification_sound: bool,
    #[serde(default = "default_notification_volume")]
    pub notification_volume: u8,
    /// What the tray battery icon shows: `"glyph"` (the native battery icon)
    /// or `"percentage"` (just the number). Other values read as `"glyph"`.
    #[serde(default = "default_tray_battery_style")]
    pub tray_battery_style: String,
    /// Hybrid device mode: while the OpenMouse web panel is disconnected,
    /// Bridge drives the mouse itself — the active profile's stored settings
    /// (DPI, polling rate, switch mode) go to the onboard memory and the
    /// battery is read natively — and hands the mouse over to the panel the
    /// moment it connects, applying the stored settings again on disconnect.
    #[serde(default = "enabled_by_default")]
    pub hybrid_mode: bool,
    #[serde(default)]
    pub games: Vec<GameConfig>,
    /// Games the user added by hand. Kept apart from `games` because the
    /// catalog replaces that list on every start.
    #[serde(default)]
    pub custom_games: Vec<GameConfig>,
    #[serde(default)]
    pub local_applications: Vec<LocalApplication>,
    #[serde(default)]
    pub profiles: Vec<ApplicationProfile>,
    #[serde(default)]
    pub default_profile: Option<ApplicationProfile>,
    /// The last state the control panel set on each device (lighting, mainly),
    /// which Bridge replays whenever it could have been lost: at startup, when
    /// the device reappears, on resume, and on session lock, unlock, logon and
    /// logoff. Lives apart from profiles because it applies all the time, not
    /// per game.
    #[serde(default)]
    pub device_states: Vec<DeviceState>,
    /// How often, in seconds, replayable state is re-sent as a watchdog; 0
    /// turns the watchdog off (events still replay). Animated lighting is never
    /// replayed by the watchdog, since re-sending an effect restarts it.
    #[serde(default = "default_reassert_interval")]
    pub reassert_interval_seconds: u64,
    /// How often, in seconds, Bridge reads back the effect a device that can
    /// report it is running and rewrites it only if it differs from the stored
    /// state; 0 turns that off. Reading never interrupts vendor software the way
    /// a write does, so this can be much faster than the blind watchdog, and
    /// devices it covers are left out of the watchdog.
    #[serde(default = "default_verify_interval")]
    pub verify_interval_seconds: u64,
    /// How lighting is managed and when it is turned off; see [`LightingSettings`].
    #[serde(default)]
    pub lighting: LightingSettings,
    #[serde(default = "default_origins")]
    pub allowed_origins: Vec<String>,
    /// Whether Bridge has already turned launch-at-login on by default, so a
    /// user who later turns it off keeps it off.
    #[serde(default)]
    pub autostart_configured: bool,
    /// Whether the tray panel has been shown once. Bridge stays in the tray
    /// after that; the first launch opens the panel so a window actually appears.
    #[serde(default)]
    pub tray_hint_shown: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GameConfig {
    pub name: String,
    pub executables: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LocalApplication {
    pub id: String,
    pub name: String,
    pub executable: String,
    pub path: String,
    pub kind: LocalApplicationKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation: Option<LocalApplicationActivation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_identifier: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum LocalApplicationActivation {
    Running,
    Focused,
}

impl LocalApplication {
    pub fn effective_activation(&self) -> LocalApplicationActivation {
        self.activation.unwrap_or(match self.kind {
            LocalApplicationKind::Game => LocalApplicationActivation::Running,
            LocalApplicationKind::App => LocalApplicationActivation::Focused,
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum LocalApplicationKind {
    Game,
    App,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationProfile {
    pub application: ProfileApplication,
    pub device: ProfileDevice,
    pub settings: ProfileSettings,
    /// A disabled profile is kept (so the control panel can store a game's
    /// settings while its automatic apply is off) but never matched.
    /// Configs written before this field existed only held active profiles.
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}

fn enabled_by_default() -> bool {
    true
}

impl ApplicationProfile {
    /// Which application the profile is for, as the matcher sees it: the full
    /// path when there is one (Windows application profiles), otherwise the
    /// executable, otherwise the name (game profiles from the control panel,
    /// which only know a game's executables). Lowercased, as matching is.
    pub fn application_key(&self) -> String {
        let application = &self.application;
        if let Some(id) = &application.local_id {
            return id.clone();
        }
        if !application.path.is_empty() {
            return crate::games::path_key(&application.path);
        }
        [
            &application.path,
            &application.executable,
            &application.name,
        ]
        .into_iter()
        .find(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_default()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProfileApplication {
    pub name: String,
    pub executable: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation: Option<LocalApplicationActivation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_identifier: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProfileDevice {
    pub id: String,
    pub name: String,
}

/// Who sets the colours.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LightingMode {
    /// Vendor software (Razer Synapse) sets the colours, and Bridge holds the
    /// same colour as the fallback the device shows when that software stops
    /// feeding it, repairing it if something resets it. Vendor software's own
    /// colours cannot be read back, so the colour is told to Bridge, not learned.
    #[default]
    Maintain,
    /// Bridge is the colour client and vendor software is not running.
    Own,
}

/// Lighting behaviour for every device Bridge keeps a state for.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LightingSettings {
    #[serde(default)]
    pub mode: LightingMode,
    /// Turn the lights off while the monitor is off and bring the stored colours
    /// back when it comes on. The stored state is never changed by this.
    #[serde(default)]
    pub off_when_display_off: bool,
    /// Turn the lights off as the PC goes to sleep and bring them back on resume.
    #[serde(default)]
    pub off_on_sleep: bool,
}

/// The most devices whose state Bridge keeps.
pub const MAX_DEVICE_STATES: usize = 32;
/// The largest serialized snapshot kept per device, in bytes.
pub const MAX_DEVICE_STATE_BYTES: usize = 64 * 1024;

/// What the control panel last set on one device, as its `Partial<MouseStatus>`
/// field map, keyed by the same `"<brand>:<name>"` id profiles use.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DeviceState {
    pub device_id: String,
    pub device_name: String,
    pub snapshot: serde_json::Map<String, serde_json::Value>,
    /// Unix seconds of the last change.
    #[serde(default)]
    pub updated_at: u64,
}

impl DeviceState {
    /// The brand half of the device id, as the drivers dispatch on it.
    pub fn brand(&self) -> &str {
        self.device_id.split(':').next().unwrap_or_default()
    }

    /// Whether re-sending this state is invisible. A static or off effect looks
    /// the same however often it is written; an animated one (cycling, wave,
    /// breathing, reactive) restarts, so only events replay it, never the
    /// watchdog. State with no lighting at all is replayable.
    pub fn is_idempotent(&self) -> bool {
        let zones = self
            .snapshot
            .get("lightingZones")
            .or_else(|| self.snapshot.get("lighting"));
        let animated = |zone: &serde_json::Value| {
            zone.get("mode")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|mode| !matches!(mode.to_ascii_lowercase().as_str(), "static" | "off"))
        };
        match zones {
            Some(serde_json::Value::Array(zones)) => !zones.iter().any(animated),
            Some(zone @ serde_json::Value::Object(_)) => !animated(zone),
            _ => true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProfileSettings {
    pub dpi: Option<u32>,
    pub polling_rate_hz: Option<u32>,
    /// Every other device setting the profile carries, as the control
    /// panel's `Partial<MouseStatus>` field map. Bridge never interprets it:
    /// it only stores it and hands it back in `/v1/status`'s activeProfile so
    /// an open OpenMouse tab can apply it over WebHID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<serde_json::Value>,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            battery_threshold_percent: DEFAULT_BATTERY_THRESHOLD,
            alert_cooldown_minutes: DEFAULT_ALERT_COOLDOWN_MINUTES,
            automatic_updates: false,
            notification_sound: true,
            notification_volume: DEFAULT_NOTIFICATION_VOLUME,
            tray_battery_style: default_tray_battery_style(),
            hybrid_mode: true,
            games: Vec::new(),
            custom_games: Vec::new(),
            local_applications: Vec::new(),
            profiles: Vec::new(),
            default_profile: None,
            device_states: Vec::new(),
            reassert_interval_seconds: DEFAULT_REASSERT_INTERVAL_SECONDS,
            verify_interval_seconds: DEFAULT_VERIFY_INTERVAL_SECONDS,
            lighting: LightingSettings::default(),
            allowed_origins: default_origins(),
            autostart_configured: false,
            tray_hint_shown: false,
        }
    }
}

impl BridgeConfig {
    pub fn normalized(mut self) -> Self {
        self.battery_threshold_percent = self.battery_threshold_percent.min(100);
        self.notification_volume = self.notification_volume.min(100);
        self.alert_cooldown_minutes = self.alert_cooldown_minutes.max(1);
        // Only the two known tray styles are honored; anything else (a
        // hand-edited config, an older panel) reads as the default glyph.
        self.tray_battery_style = match self.tray_battery_style.trim().to_ascii_lowercase().as_str()
        {
            "percentage" => "percentage".to_owned(),
            _ => "glyph".to_owned(),
        };
        normalize_games(&mut self.games, &CATALOG_LIMITS);
        normalize_games(&mut self.custom_games, &CUSTOM_GAME_LIMITS);
        for profile in &mut self.profiles {
            profile.application.name = profile.application.name.trim().to_owned();
            profile.application.executable = profile.application.executable.trim().to_owned();
            profile.application.path = profile.application.path.trim().to_owned();
            profile.device.id = profile.device.id.trim().to_owned();
            profile.device.name = profile.device.name.trim().to_owned();
        }
        for profile in self
            .profiles
            .iter_mut()
            .chain(self.default_profile.iter_mut())
        {
            let _ = crate::local_targets::canonicalize_profile(profile, &self.local_applications);
        }
        // Game profiles from the control panel carry no path (a game is known
        // by its executables), so a profile only needs something to match on.
        self.profiles.retain(|profile| {
            !profile.application_key().is_empty() && !profile.device.id.is_empty()
        });
        let mut identities = std::collections::HashSet::new();
        self.profiles.retain(|profile| {
            identities.insert((profile.application_key(), profile.device.id.clone()))
        });
        if let Some(profile) = &mut self.default_profile {
            profile.application.name = profile.application.name.trim().to_owned();
            profile.device.id = profile.device.id.trim().to_owned();
            profile.device.name = profile.device.name.trim().to_owned();
            if profile.application.name.is_empty()
                || profile.device.id.is_empty()
                || profile.device.name.is_empty()
            {
                self.default_profile = None;
            }
        }
        if self.reassert_interval_seconds != 0 {
            self.reassert_interval_seconds = self
                .reassert_interval_seconds
                .max(MIN_REASSERT_INTERVAL_SECONDS);
        }
        if self.verify_interval_seconds != 0 {
            self.verify_interval_seconds = self
                .verify_interval_seconds
                .max(MIN_VERIFY_INTERVAL_SECONDS);
        }
        for state in &mut self.device_states {
            state.device_id = state.device_id.trim().to_owned();
            state.device_name = state.device_name.trim().to_owned();
        }
        let mut seen_states = std::collections::HashSet::new();
        self.device_states.retain(|state| {
            !state.device_id.is_empty()
                && !state.snapshot.is_empty()
                && seen_states.insert(state.device_id.to_ascii_lowercase())
        });
        self.device_states.truncate(MAX_DEVICE_STATES);
        for origin in OFFICIAL_ORIGINS {
            if !self.allowed_origins.iter().any(|entry| entry == origin) {
                self.allowed_origins.push((*origin).to_owned());
            }
        }
        self.allowed_origins.sort();
        self.allowed_origins.dedup();
        self
    }

    pub fn tracked_games(&self) -> Vec<GameConfig> {
        let mut games = self.games.clone();
        games.extend(
            self.custom_games
                .iter()
                .filter(|custom| {
                    !self
                        .games
                        .iter()
                        .any(|game| game.name.eq_ignore_ascii_case(&custom.name))
                })
                .cloned(),
        );
        games
    }
}

const MAX_GAMES: usize = 200;
const MAX_NAME_CHARS: usize = 80;
const MAX_EXECUTABLES: usize = 20;
const MAX_EXECUTABLE_CHARS: usize = 260;

struct GameLimits {
    games: usize,
    name_chars: usize,
    executables: usize,
    executable_chars: usize,
}

/// What a user may add by hand.
const CUSTOM_GAME_LIMITS: GameLimits = GameLimits {
    games: MAX_GAMES,
    name_chars: MAX_NAME_CHARS,
    executables: MAX_EXECUTABLES,
    executable_chars: MAX_EXECUTABLE_CHARS,
};

/// The catalog is already bounded when it is fetched; these match that parser.
const CATALOG_LIMITS: GameLimits = GameLimits {
    games: crate::games::MAX_CATALOG_GAMES,
    name_chars: crate::games::MAX_CATALOG_STRING,
    executables: crate::games::MAX_EXECUTABLES,
    executable_chars: crate::games::MAX_CATALOG_STRING,
};

fn normalize_games(games: &mut Vec<GameConfig>, limits: &GameLimits) {
    for game in games.iter_mut() {
        game.name = game.name.trim().chars().take(limits.name_chars).collect();
        game.executables = game
            .executables
            .iter()
            .map(|entry| entry.trim().to_ascii_lowercase())
            .filter(|entry| !entry.is_empty() && entry.chars().count() <= limits.executable_chars)
            .collect();
        game.executables.sort();
        game.executables.dedup();
        game.executables.truncate(limits.executables);
    }
    games.retain(|game| !game.name.is_empty() && !game.executables.is_empty());
    // Sorted by the folded name, so names that differ only in case sit side by side for the dedup.
    games.sort_by_key(|game| game.name.to_ascii_lowercase());
    games.dedup_by(|left, right| left.name.eq_ignore_ascii_case(&right.name));
    games.truncate(limits.games);
}

pub fn config_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os("OPENMOUSE_BRIDGE_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    let dirs = ProjectDirs::from("io", "OpenMouse", "OpenMouse Bridge")
        .context("the operating system did not provide an application-data directory")?;
    Ok(dirs.config_dir().join("config.json"))
}

pub fn log_dir() -> Result<PathBuf> {
    let config = config_path()?;
    let parent = config
        .parent()
        .context("the Bridge config path has no parent directory")?;
    Ok(parent.join("logs"))
}

pub fn load_or_create() -> Result<(BridgeConfig, PathBuf)> {
    let path = config_path()?;
    if path.exists() {
        let bytes =
            fs::read(&path).with_context(|| format!("could not read {}", path.display()))?;
        let config = serde_json::from_slice::<BridgeConfig>(&bytes)
            .with_context(|| format!("could not parse {}", path.display()))?
            .normalized();
        return Ok((config, path));
    }
    let config = BridgeConfig::default();
    save(&path, &config)?;
    Ok((config, path))
}

pub async fn load_with_catalog() -> Result<(BridgeConfig, PathBuf)> {
    let (mut config, path) = load_or_create()?;
    match crate::games::fetch_catalog().await {
        Ok(games) => {
            let cached_games = std::mem::replace(&mut config.games, games);
            config = config.normalized();
            let changed = config.games != cached_games;
            if changed {
                save(&path, &config)?;
            }
            tracing::info!(
                url = %crate::games::catalog_url(),
                games = config.games.len(),
                changed,
                "game catalog loaded"
            );
        }
        Err(error) => {
            tracing::warn!(
                %error,
                cached_games = config.games.len(),
                "game catalog unavailable; using cached catalog"
            );
        }
    }
    Ok((config, path))
}

pub fn save(path: &std::path::Path, config: &BridgeConfig) -> Result<()> {
    let json = serde_json::to_vec_pretty(config)?;
    write_atomically(path, &json)
}

/// Writes `bytes` to `path` through a temporary file and a rename, so a reader
/// never sees a truncated file. Creates the parent directory when needed.
pub(crate) fn write_atomically(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use std::{
        io::Write,
        sync::atomic::{AtomicU64, Ordering},
    };
    static NEXT_SAVE: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("could not create {}", parent.display()))?;
    let name = path
        .file_name()
        .context("config path has no filename")?
        .to_string_lossy();
    let temporary = parent.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        NEXT_SAVE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .with_context(|| format!("could not create {}", temporary.display()))?;
    let result: std::io::Result<()> = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.with_context(|| format!("could not write {}", path.display()))
}

const fn default_battery_threshold() -> u8 {
    DEFAULT_BATTERY_THRESHOLD
}

const fn default_alert_cooldown() -> u64 {
    DEFAULT_ALERT_COOLDOWN_MINUTES
}

const fn default_notification_volume() -> u8 {
    DEFAULT_NOTIFICATION_VOLUME
}

const fn default_reassert_interval() -> u64 {
    DEFAULT_REASSERT_INTERVAL_SECONDS
}

const fn default_verify_interval() -> u64 {
    DEFAULT_VERIFY_INTERVAL_SECONDS
}

fn default_tray_battery_style() -> String {
    "glyph".to_owned()
}

fn default_origins() -> Vec<String> {
    vec![
        "https://control.openmouse.app".to_owned(),
        "https://dev.openmouse.app".to_owned(),
        "https://openmouse.app".to_owned(),
        "https://www.openmouse.app".to_owned(),
        "http://localhost:5173".to_owned(),
        "http://127.0.0.1:5173".to_owned(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_that_differ_only_in_case_are_one_game_wherever_they_sort() {
        let game = |name: &str| GameConfig {
            name: name.into(),
            executables: vec!["g.exe".into()],
        };
        // Sorted by plain bytes these would be Alpha, Beta, alpha: the two Alphas apart.
        let mut games = vec![game("alpha"), game("Beta"), game("Alpha")];
        normalize_games(&mut games, &CUSTOM_GAME_LIMITS);
        assert_eq!(games.len(), 2);
    }

    #[test]
    fn game_lists_have_a_size_limit() {
        let many = |count: usize| -> Vec<GameConfig> {
            (0..count)
                .map(|i| GameConfig {
                    name: format!("game {i:04}"),
                    executables: vec![format!("g{i}.exe")],
                })
                .collect()
        };
        let mut games = many(MAX_GAMES + 50);
        normalize_games(&mut games, &CUSTOM_GAME_LIMITS);
        assert_eq!(games.len(), MAX_GAMES);

        let mut one = vec![GameConfig {
            name: "n".repeat(500),
            executables: (0..40)
                .map(|i| format!("{i}.exe"))
                .chain([format!("{}.exe", "x".repeat(300))])
                .collect(),
        }];
        normalize_games(&mut one, &CUSTOM_GAME_LIMITS);
        assert_eq!(one[0].name.chars().count(), MAX_NAME_CHARS);
        assert_eq!(one[0].executables.len(), MAX_EXECUTABLES);
        assert!(
            one[0]
                .executables
                .iter()
                .all(|entry| entry.chars().count() <= MAX_EXECUTABLE_CHARS)
        );
    }

    #[test]
    fn the_catalog_is_not_held_to_the_custom_game_limits() {
        let config = BridgeConfig {
            games: (0..MAX_GAMES + 50)
                .map(|i| GameConfig {
                    name: format!("game {i:04}"),
                    executables: (0..MAX_EXECUTABLES + 5)
                        .map(|e| format!("g{i}-{e}.exe"))
                        .collect(),
                })
                .collect(),
            ..BridgeConfig::default()
        }
        .normalized();
        assert_eq!(config.games.len(), MAX_GAMES + 50);
        assert_eq!(config.games[0].executables.len(), MAX_EXECUTABLES + 5);
    }

    #[test]
    fn saving_replaces_the_file_instead_of_truncating_an_existing_reader() {
        use std::io::Read;
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("atomic-config-test-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("config.json");
        fs::write(&path, b"old configuration").unwrap();
        let mut original = fs::File::open(&path).unwrap();
        save(&path, &BridgeConfig::default()).unwrap();
        let mut bytes = Vec::new();
        original.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"old configuration");
        assert!(serde_json::from_slice::<BridgeConfig>(&fs::read(&path).unwrap()).is_ok());
        drop(original);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn local_profile_metadata_is_canonical_after_config_reload() {
        let json = serde_json::json!({"localApplications":[{"id":"local:editor","name":" Editor ","executable":"editor.exe","path":"C:/Tools/editor.exe","kind":"app"}],"profiles":[{"application":{"localId":"local:editor","name":"Old name","executable":"old.exe","path":"C:/Old/old.exe"},"device":{"id":"d","name":"Mouse"},"settings":{"dpi":800}}]});
        let config: BridgeConfig = serde_json::from_value(json).unwrap();
        let config = config.normalized();
        assert_eq!(
            config.profiles[0].application.name,
            config.local_applications[0].name
        );
        assert_eq!(config.profiles[0].application.executable, "editor.exe");
        assert_eq!(config.profiles[0].application.path, "C:/Tools/editor.exe");
    }

    #[test]
    fn local_profile_identity_is_not_its_display_name_or_executable() {
        let json = serde_json::json!({"profiles": [
            {"application":{"name":"Editor","executable":"editor.exe","path":"","localId":"local:one"}, "device":{"id":"d","name":"Mouse"},"settings":{}},
            {"application":{"name":"Editor","executable":"editor.exe","path":"","localId":"local:two"}, "device":{"id":"d","name":"Mouse"},"settings":{}},
            {"application":{"name":"Editor","executable":"editor.exe","path":""}, "device":{"id":"d","name":"Mouse"},"settings":{}}
        ]});
        let config: BridgeConfig = serde_json::from_value(json).unwrap();
        let saved = serde_json::to_value(config.normalized()).unwrap();
        assert_eq!(saved["profiles"].as_array().unwrap().len(), 3);
        assert!(
            saved["profiles"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["application"]["localId"] == "local:one")
        );
    }

    #[test]
    fn local_targets_survive_config_roundtrip_and_catalog_refresh() {
        let json = serde_json::json!({"localApplications": [{"id":"local:editor", "name":"Editor", "executable":"editor.exe", "path":"C:/Tools/editor.exe", "kind":"app"}]});
        let mut config: BridgeConfig = serde_json::from_value(json.clone()).unwrap();
        config.games = vec![GameConfig {
            name: "Preset".into(),
            executables: vec!["preset.exe".into()],
        }];
        let saved = serde_json::to_value(config.normalized()).unwrap();
        assert_eq!(saved["localApplications"], json["localApplications"]);
        let legacy: BridgeConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(
            serde_json::to_value(legacy).unwrap()["localApplications"],
            serde_json::json!([])
        );
    }

    #[test]
    fn normalization_clamps_threshold_and_cleans_executables() {
        let config = BridgeConfig {
            battery_threshold_percent: 140,
            alert_cooldown_minutes: 0,
            games: vec![GameConfig {
                name: " Valorant ".into(),
                executables: vec![" VALORANT-Win64-Shipping.exe ".into(), "".into()],
            }],
            automatic_updates: false,
            notification_sound: true,
            notification_volume: 180,
            custom_games: Vec::new(),
            tray_battery_style: " percentage ".to_owned(),
            hybrid_mode: true,
            profiles: Vec::new(),
            default_profile: None,
            device_states: Vec::new(),
            reassert_interval_seconds: DEFAULT_REASSERT_INTERVAL_SECONDS,
            verify_interval_seconds: DEFAULT_VERIFY_INTERVAL_SECONDS,
            lighting: LightingSettings::default(),
            allowed_origins: Vec::new(),
            local_applications: Vec::new(),
            autostart_configured: false,
            tray_hint_shown: false,
        }
        .normalized();
        assert_eq!(config.battery_threshold_percent, 100);
        assert_eq!(config.alert_cooldown_minutes, 1);
        assert_eq!(config.notification_volume, 100);
        assert_eq!(config.tray_battery_style, "percentage");
        let valorant = config
            .games
            .iter()
            .find(|game| game.name == "Valorant")
            .expect("configured game should survive normalization");
        assert_eq!(valorant.executables, ["valorant-win64-shipping.exe"]);
        assert!(
            config
                .allowed_origins
                .iter()
                .any(|origin| origin == "https://control.openmouse.app")
        );
    }

    #[test]
    fn custom_games_are_tracked_beside_the_catalog_and_the_catalog_wins_name_clashes() {
        let game = |name: &str, exe: &str| GameConfig {
            name: name.into(),
            executables: vec![exe.into()],
        };
        let config = BridgeConfig {
            games: vec![game("Valorant", "valorant.exe")],
            custom_games: vec![
                game("My Game", " MyGame.EXE "),
                game("valorant", "other.exe"),
            ],
            ..BridgeConfig::default()
        }
        .normalized();
        let names: Vec<_> = config
            .tracked_games()
            .into_iter()
            .map(|game| game.name)
            .collect();
        assert_eq!(names, ["Valorant", "My Game"]);
        assert_eq!(config.custom_games[0].executables, ["mygame.exe"]);
    }

    fn device_state(id: &str, snapshot: serde_json::Value) -> DeviceState {
        DeviceState {
            device_id: id.into(),
            device_name: "Mouse".into(),
            snapshot: snapshot.as_object().cloned().expect("an object"),
            updated_at: 0,
        }
    }

    #[test]
    fn only_static_or_off_lighting_is_safe_for_the_watchdog() {
        let zone = |mode: &str| serde_json::json!({ "zone": "Logo", "mode": mode });
        let idempotent = |snapshot| device_state("Razer:Mouse", snapshot).is_idempotent();
        assert!(idempotent(
            serde_json::json!({ "lighting": zone("Static") })
        ));
        assert!(idempotent(serde_json::json!({ "lighting": zone("Off") })));
        assert!(idempotent(
            serde_json::json!({ "lightingZones": [zone("Static"), zone("off")] })
        ));
        assert!(!idempotent(serde_json::json!({ "lighting": zone("Wave") })));
        assert!(
            !idempotent(serde_json::json!({ "lightingZones": [zone("Static"), zone("Cycling")] })),
            "one animated zone is enough to keep the watchdog away"
        );
        assert!(
            idempotent(serde_json::json!({ "lightingZones": [{ "zone": "Logo", "mode": null }] })),
            "a zone with no effect chosen has nothing to restart"
        );
        assert!(
            idempotent(serde_json::json!({ "dpi": 800 })),
            "state without lighting is replayable"
        );
    }

    #[test]
    fn device_states_are_deduplicated_capped_and_the_watchdog_has_a_floor() {
        let mut states: Vec<DeviceState> = (0..MAX_DEVICE_STATES + 5)
            .map(|i| {
                device_state(
                    &format!("Razer:Mouse {i}"),
                    serde_json::json!({ "dpi": 800 }),
                )
            })
            .collect();
        states.push(device_state(
            " razer:mouse 0 ",
            serde_json::json!({ "dpi": 1600 }),
        ));
        states.push(device_state("Razer:Empty", serde_json::json!({})));
        let config = BridgeConfig {
            device_states: states,
            reassert_interval_seconds: 5,
            ..BridgeConfig::default()
        }
        .normalized();
        assert_eq!(config.device_states.len(), MAX_DEVICE_STATES);
        assert!(
            config
                .device_states
                .iter()
                .all(|state| state.device_id != "Razer:Empty")
        );
        assert_eq!(
            config.device_states[0].snapshot["dpi"], 800,
            "the first entry for a device wins"
        );
        assert_eq!(config.reassert_interval_seconds, 30);
        let off = BridgeConfig {
            reassert_interval_seconds: 0,
            ..BridgeConfig::default()
        }
        .normalized();
        assert_eq!(off.reassert_interval_seconds, 0, "zero stays off");
    }

    #[test]
    fn configs_saved_before_device_state_load_with_the_defaults() {
        let config: BridgeConfig = serde_json::from_str("{}").expect("empty config parses");
        assert!(config.device_states.is_empty());
        assert_eq!(config.reassert_interval_seconds, 300);
        assert_eq!(config.verify_interval_seconds, 60);
    }

    #[test]
    fn lighting_settings_default_to_maintaining_with_both_power_options_off() {
        let config: BridgeConfig = serde_json::from_str("{}").expect("empty config parses");
        assert_eq!(config.lighting.mode, LightingMode::Maintain);
        assert!(!config.lighting.off_when_display_off && !config.lighting.off_on_sleep);
        let saved: BridgeConfig =
            serde_json::from_str(r#"{"lighting":{"mode":"own","offWhenDisplayOff":true}}"#)
                .expect("a partial lighting block parses");
        assert_eq!(saved.lighting.mode, LightingMode::Own);
        assert!(saved.lighting.off_when_display_off);
        assert!(!saved.lighting.off_on_sleep, "an unset option stays off");
        assert!(serde_json::from_str::<BridgeConfig>(r#"{"lighting":{"mode":"bogus"}}"#).is_err());
    }

    #[test]
    fn the_verify_interval_has_a_floor_and_zero_turns_it_off() {
        let floor = BridgeConfig {
            verify_interval_seconds: 3,
            ..BridgeConfig::default()
        }
        .normalized();
        assert_eq!(floor.verify_interval_seconds, 15);
        let off = BridgeConfig {
            verify_interval_seconds: 0,
            ..BridgeConfig::default()
        }
        .normalized();
        assert_eq!(off.verify_interval_seconds, 0);
    }

    #[test]
    fn hybrid_mode_defaults_to_on_and_normalizes_to_a_bool() {
        let config: BridgeConfig = serde_json::from_str("{}").expect("empty config parses");
        assert!(config.hybrid_mode);
        let config: BridgeConfig =
            serde_json::from_str(r#"{"hybridMode": false}"#).expect("off config parses");
        assert!(!config.hybrid_mode);
    }

    #[test]
    fn profile_settings_keep_the_snapshot_and_accept_profiles_without_one() {
        let json =
            r#"{"dpi":800,"pollingRateHz":null,"snapshot":{"lightforceSwitchMode":"Optical"}}"#;
        let settings: ProfileSettings =
            serde_json::from_str(json).expect("snapshot profile parses");
        assert_eq!(
            settings.snapshot,
            Some(serde_json::json!({ "lightforceSwitchMode": "Optical" }))
        );
        assert_eq!(serde_json::to_string(&settings).unwrap(), json);

        let legacy: ProfileSettings = serde_json::from_str(r#"{"dpi":800,"pollingRateHz":1000}"#)
            .expect("legacy profile parses");
        assert_eq!(legacy.snapshot, None);
        assert!(!serde_json::to_string(&legacy).unwrap().contains("snapshot"));
    }

    fn game_profile(name: &str, executable: &str, device: &str) -> ApplicationProfile {
        ApplicationProfile {
            application: ProfileApplication {
                name: name.into(),
                executable: executable.into(),
                path: String::new(),
                local_id: None,
                activation: None,
                bundle_path: None,
                bundle_identifier: None,
            },
            device: ProfileDevice {
                id: device.into(),
                name: "Mouse".into(),
            },
            settings: ProfileSettings {
                dpi: Some(800),
                polling_rate_hz: None,
                snapshot: None,
            },
            enabled: true,
        }
    }

    #[test]
    fn normalization_keeps_path_less_game_profiles_and_dedupes_by_executable() {
        let config = BridgeConfig {
            profiles: vec![
                game_profile("Apex Legends", "r5apex.exe", "Logitech:Mouse"),
                game_profile("Apex Legends", "R5Apex.exe", "Logitech:Mouse"),
                game_profile("VALORANT", "valorant.exe", "Logitech:Mouse"),
                game_profile("", "", "Logitech:Mouse"),
            ],
            ..BridgeConfig::default()
        };
        let names: Vec<_> = config
            .normalized()
            .profiles
            .into_iter()
            .map(|profile| profile.application.name)
            .collect();
        assert_eq!(names, ["Apex Legends", "VALORANT"]);
    }

    #[test]
    fn profiles_saved_before_the_enabled_flag_load_as_enabled() {
        let json = r#"{"application":{"name":"Apex Legends","executable":"r5apex.exe","path":""},"device":{"id":"d","name":"Mouse"},"settings":{"dpi":800,"pollingRateHz":null}}"#;
        let profile: ApplicationProfile =
            serde_json::from_str(json).expect("legacy profile parses");
        assert!(profile.enabled);
    }
}
