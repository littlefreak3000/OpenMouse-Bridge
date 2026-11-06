//! Keeps the last state the control panel set on each device alive.
//!
//! Some device state (Razer lighting above all) lives in the device's volatile
//! memory and cannot be read back, so nothing can tell when it was lost. The
//! panel posts what it set (`PUT /v1/device-state`); this module replays it
//! whenever it could have gone: at startup, when a device reappears on the bus,
//! after a resume, on session lock, unlock, logon and logoff, after the panel
//! lets go of the device, and on a slow watchdog.
//!
//! The watchdog skips animated lighting, because re-sending an effect restarts
//! it; events replay everything. A replay never runs while the panel holds a HID
//! session through Bridge, since the panel's live writes are then the truth.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use serde::Serialize;
use serde_json::{Map, Value};

use super::{BridgeService, panel_connected};
use crate::{
    config::{DeviceState, LightingSettings, MAX_DEVICE_STATE_BYTES, MAX_DEVICE_STATES},
    drivers,
    local_targets::InvalidInput,
};

/// Why a replay was asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Trigger {
    Startup,
    DeviceArrived,
    PanelReleased,
    Resume,
    SessionLock,
    SessionUnlock,
    SessionLogon,
    SessionLogoff,
    /// Console or remote connect/disconnect, and other session changes.
    SessionChange,
    Watchdog,
    /// Reads a device back and rewrites it only if it no longer matches.
    Verify,
    /// The monitor turned off, or back on (the console display state).
    DisplayOff,
    DisplayOn,
    /// The PC is about to sleep.
    Suspend,
    Manual,
}

impl Trigger {
    /// When to replay after the trigger, as delays from it. A device that was
    /// just woken or re-enumerated can drop the first write, so the events that
    /// cause that replay again a little later.
    fn settle_delays(self) -> &'static [Duration] {
        const NOW: Duration = Duration::from_secs(0);
        const ONE: Duration = Duration::from_secs(1);
        const SOON: Duration = Duration::from_secs(3);
        const FIVE: Duration = Duration::from_secs(5);
        const SIX: Duration = Duration::from_secs(6);
        const LATER: Duration = Duration::from_secs(15);
        const MUCH_LATER: Duration = Duration::from_secs(20);
        // Vendor software that resets lighting on lock or unlock does it at a
        // time of its own choosing, so these replay once more, later.
        const SETTLED: Duration = Duration::from_secs(8);
        match self {
            Self::Startup => &[SOON, LATER],
            Self::DeviceArrived => &[ONE, SIX],
            Self::Resume => &[NOW, FIVE, MUCH_LATER],
            Self::SessionLock
            | Self::SessionUnlock
            | Self::SessionLogon
            | Self::SessionLogoff
            | Self::SessionChange => &[NOW, SOON, SETTLED],
            Self::PanelReleased
            | Self::Watchdog
            | Self::Verify
            | Self::DisplayOff
            | Self::DisplayOn
            | Self::Suspend
            | Self::Manual => &[NOW],
        }
    }
}

/// How the most recent replay of one device went.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayRecord {
    pub trigger: Trigger,
    /// Unix seconds.
    pub at: u64,
    /// `applied`, `failed`, `noDriver` or `notConnected`.
    pub outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A device's stored state as the API reports it.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceStateStatus {
    pub device_id: String,
    pub device_name: String,
    pub snapshot: Map<String, Value>,
    pub updated_at: u64,
    /// Whether the watchdog may replay this state.
    pub watchdog_replayable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_replay: Option<ReplayRecord>,
}

/// The hardware side of a replay, so the logic is testable without a device.
pub(super) trait ReplayDriver: Send + Sync {
    /// The lowercase brands of the mice on the bus right now.
    fn present_brands(&self) -> HashSet<String>;
    /// Applies the state, trying `hint_product_id` (what answered last time)
    /// first. Returns whether a driver claimed the device (`false` means none
    /// did) and the product id that answered, when known.
    fn apply(
        &self,
        state: &DeviceState,
        hint_product_id: Option<u16>,
    ) -> Result<(bool, Option<u16>)>;
    /// Reads the lighting the device is running right now, and the product id
    /// that answered. An empty list means the device did not report any.
    fn read_lighting(
        &self,
        device_id: &str,
        hint_product_id: Option<u16>,
    ) -> Result<(Vec<Value>, Option<u16>)>;
    /// Every connected device that reports lighting zones (`name`, `productId`,
    /// `firmware`, `zones`), read from the hardware.
    fn list_lighting(&self) -> Result<Vec<Value>>;
}

struct NativeDriver;

impl ReplayDriver for NativeDriver {
    fn present_brands(&self) -> HashSet<String> {
        drivers::detect_mice()
            .iter()
            .map(|mouse| super::brand_of(&mouse.device_id).to_ascii_lowercase())
            .collect()
    }

    fn apply(
        &self,
        state: &DeviceState,
        hint_product_id: Option<u16>,
    ) -> Result<(bool, Option<u16>)> {
        drivers::apply_device_state(state, hint_product_id)
    }

    fn read_lighting(
        &self,
        device_id: &str,
        hint_product_id: Option<u16>,
    ) -> Result<(Vec<Value>, Option<u16>)> {
        drivers::read_device_lighting(device_id, hint_product_id)
    }

    fn list_lighting(&self) -> Result<Vec<Value>> {
        drivers::list_device_lighting()
    }
}

/// Shared by every clone of the service.
pub(super) struct Reassert {
    pending: Mutex<Vec<Trigger>>,
    history: Mutex<HashMap<String, ReplayRecord>>,
    driver: Mutex<Arc<dyn ReplayDriver>>,
    /// One replay at a time, so a burst of events plays out in order.
    serial: tokio::sync::Mutex<()>,
    last_change: Mutex<Option<Instant>>,
    /// The product id that answered for each device last time, so the next
    /// replay tries it first instead of probing every interface of the brand.
    hints: Mutex<HashMap<String, u16>>,
    /// Devices whose effect a read has come back for, which the verifier covers
    /// and the blind watchdog therefore leaves alone.
    readable: Mutex<HashSet<String>>,
    /// Triggers whose replay schedule is still playing out.
    running: Arc<Mutex<HashSet<Trigger>>>,
    /// Whether the monitor is off and whether the PC is asleep, which turn the
    /// lights off when the matching option is on.
    power: Mutex<PowerState>,
    /// Wakes the monitor loop the moment something is requested, so a sleep or a
    /// monitor-off is acted on at once instead of on the next tick.
    wake: tokio::sync::Notify,
    /// The last device scan, and when it was made.
    detected: Mutex<Option<(Instant, Vec<Value>)>>,
    /// Whether vendor software looked to be running, and when that was checked.
    synapse: Mutex<Option<(Instant, bool)>>,
}

/// What the power state is right now.
#[derive(Default)]
struct PowerState {
    display_off: bool,
    asleep: bool,
}

impl Default for Reassert {
    fn default() -> Self {
        Self {
            pending: Mutex::default(),
            history: Mutex::default(),
            driver: Mutex::new(Arc::new(NativeDriver)),
            serial: tokio::sync::Mutex::new(()),
            last_change: Mutex::default(),
            hints: Mutex::default(),
            readable: Mutex::default(),
            running: Arc::default(),
            power: Mutex::default(),
            wake: tokio::sync::Notify::new(),
            detected: Mutex::default(),
            synapse: Mutex::default(),
        }
    }
}

/// A scan older than this is made again when a client asks.
const DETECTION_TTL: Duration = Duration::from_secs(60);

/// A connected device as a client sees it.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LightingDevice {
    /// `<brand>:<name>`, the id state is stored under.
    pub id: String,
    pub name: String,
    pub product_id: u64,
    pub firmware: Vec<String>,
    /// The zones it reports, with the mode and colour it is running now.
    pub zones: Vec<Value>,
    /// The zones Bridge keeps for it, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stored: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_replay: Option<ReplayRecord>,
}

/// Turns a zone off, keeping its name, and drops the colour that no longer applies.
fn turn_zone_off(zone: &mut Value) {
    let Some(object) = zone.as_object_mut() else {
        return;
    };
    if object
        .get("mode")
        .and_then(Value::as_str)
        .is_some_and(|mode| !mode.is_empty())
    {
        object.insert("mode".into(), Value::from("Off"));
        for field in ["color", "color2", "speed"] {
            object.remove(field);
        }
    }
}

/// What a device should be running now: its stored state, or the same zones
/// turned off while the monitor is off or the PC is asleep. The stored state
/// itself is never changed, so the colours come back by themselves.
fn desired_state(state: &DeviceState, overridden: bool) -> DeviceState {
    let mut desired = state.clone();
    if !overridden {
        return desired;
    }
    for field in ["lightingZones", "lighting"] {
        match desired.snapshot.get_mut(field) {
            Some(Value::Array(zones)) => zones.iter_mut().for_each(turn_zone_off),
            Some(zone @ Value::Object(_)) => turn_zone_off(zone),
            _ => {}
        }
    }
    desired
}

/// `#rrggbb` in lower case, from `#rrggbb` or `rrggbb` in either case.
fn normalize_color(color: &str) -> Result<String> {
    let digits = color.trim().trim_start_matches('#');
    if digits.len() == 6 && digits.chars().all(|digit| digit.is_ascii_hexdigit()) {
        Ok(format!("#{}", digits.to_ascii_lowercase()))
    } else {
        invalid("a colour must look like #rrggbb")
    }
}

/// Whether Razer Synapse (or its engine) is running, which competes for the lights.
fn razer_software_running() -> bool {
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    system.processes().values().any(|process| {
        let name = process.name().to_string_lossy().to_ascii_lowercase();
        name.contains("razerappengine") || name.contains("razer synapse")
    })
}

/// Marks a trigger's schedule as running until dropped.
pub(super) struct ScheduleGuard {
    trigger: Trigger,
    running: Arc<Mutex<HashSet<Trigger>>>,
}

impl Drop for ScheduleGuard {
    fn drop(&mut self) {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.trigger);
    }
}

impl Reassert {
    /// Whether the lights should be off right now for the options that are on.
    fn override_active(&self, settings: &LightingSettings) -> bool {
        let power = self.power.lock().unwrap_or_else(PoisonError::into_inner);
        (power.display_off && settings.off_when_display_off)
            || (power.asleep && settings.off_on_sleep)
    }

    /// Starts a trigger's replay schedule, or returns `None` when the same
    /// trigger is still replaying: a second lock or resume event while the first
    /// one's replays are queued would only repeat them, and each replay takes
    /// seconds on the device.
    fn begin_schedule(&self, trigger: Trigger) -> Option<ScheduleGuard> {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(trigger)
            .then(|| ScheduleGuard {
                trigger,
                running: Arc::clone(&self.running),
            })
    }

    fn mark_readable(&self, device_id: &str) {
        self.readable
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(device_id.to_ascii_lowercase());
    }

    fn is_readable(&self, device_id: &str) -> bool {
        self.readable
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&device_id.to_ascii_lowercase())
    }

    fn hint(&self, device_id: &str) -> Option<u16> {
        self.hints
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&device_id.to_ascii_lowercase())
            .copied()
    }

    fn learn(&self, device_id: &str, product_id: u16) {
        self.hints
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(device_id.to_ascii_lowercase(), product_id);
    }

    fn driver(&self) -> Arc<dyn ReplayDriver> {
        Arc::clone(&self.driver.lock().unwrap_or_else(PoisonError::into_inner))
    }

    #[cfg(test)]
    pub(super) fn set_driver(&self, driver: Arc<dyn ReplayDriver>) {
        *self.driver.lock().unwrap_or_else(PoisonError::into_inner) = driver;
    }

    fn take_pending(&self) -> Vec<Trigger> {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        let mut seen = HashSet::new();
        pending
            .drain(..)
            .filter(|trigger| seen.insert(*trigger))
            .collect()
    }

    fn record(
        &self,
        device_id: &str,
        trigger: Trigger,
        outcome: &'static str,
        error: Option<String>,
    ) {
        self.history
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                device_id.to_ascii_lowercase(),
                ReplayRecord {
                    trigger,
                    at: unix_now(),
                    outcome,
                    error,
                },
            );
    }

    fn last_replay(&self, device_id: &str) -> Option<ReplayRecord> {
        self.history
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&device_id.to_ascii_lowercase())
            .cloned()
    }

    fn recently_changed(&self) -> bool {
        self.last_change
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some_and(|at| at.elapsed() < RECENT_CHANGE)
    }
}

/// What reading a device back says about its stored state.
enum Verdict {
    /// The device is running what is stored.
    Matches,
    /// It is running something else: vendor software or a reset changed it.
    Differs,
    /// The device cannot report its effect (or the state has nothing to compare).
    Unreadable,
    Failed(String),
}

/// The stored lighting zones that name an effect, whether kept as `lightingZones`
/// or as a single `lighting` zone.
fn stored_zones(state: &DeviceState) -> Vec<&Value> {
    let zones: Vec<&Value> = match state
        .snapshot
        .get("lightingZones")
        .or_else(|| state.snapshot.get("lighting"))
    {
        Some(Value::Array(zones)) => zones.iter().collect(),
        Some(zone @ Value::Object(_)) => vec![zone],
        _ => Vec::new(),
    };
    zones
        .into_iter()
        .filter(|zone| {
            zone.get("mode")
                .and_then(Value::as_str)
                .is_some_and(|mode| !mode.is_empty())
        })
        .collect()
}

/// Whether every stored zone is running on the device: same zone, same effect,
/// and the same colour when one is stored. Brightness is not compared, since it
/// is set apart from the effect and is often changed on purpose.
fn lighting_matches(stored: &[&Value], read: &[Value]) -> bool {
    let text = |value: &Value, field: &str| {
        value
            .get(field)
            .and_then(Value::as_str)
            .map(str::to_ascii_lowercase)
    };
    stored.iter().all(|want| {
        read.iter().any(|got| {
            text(want, "zone") == text(got, "zone")
                && text(want, "mode") == text(got, "mode")
                && (text(want, "color").is_none() || text(want, "color") == text(got, "color"))
        })
    })
}

/// A watchdog replay this soon after the panel posted a change could undo a
/// change the panel is still writing to the device.
const RECENT_CHANGE: Duration = Duration::from_secs(10);

/// A tick that arrives this late means the machine was suspended in between.
const RESUME_GAP: Duration = Duration::from_secs(10);

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn invalid<T>(message: impl Into<String>) -> Result<T> {
    Err(InvalidInput(message.into()).into())
}

/// Checks a state patch before it is merged: a `<brand>:<name>` id, at least one
/// field, and a snapshot that stays within its size limit.
fn validate_id(device_id: &str) -> Result<&str> {
    let device_id = device_id.trim();
    if device_id.len() > 256 {
        return invalid("deviceId is too long");
    }
    match device_id.split_once(':') {
        Some((brand, _)) if !brand.trim().is_empty() => Ok(device_id),
        _ => invalid("deviceId must look like \"<brand>:<name>\""),
    }
}

impl BridgeService {
    /// Asks for a replay of the stored device state. Safe to call from any
    /// thread, including the service control handler's.
    pub fn request_reassert(&self, trigger: Trigger) {
        self.reassert
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(trigger);
        self.reassert.wake.notify_one();
    }

    /// Every stored device state with how its last replay went.
    pub async fn device_states(&self) -> Vec<DeviceStateStatus> {
        let states = self.inner.read().await.config.device_states.clone();
        states
            .into_iter()
            .map(|state| DeviceStateStatus {
                watchdog_replayable: state.is_idempotent(),
                last_replay: self.reassert.last_replay(&state.device_id),
                device_id: state.device_id,
                device_name: state.device_name,
                snapshot: state.snapshot,
                updated_at: state.updated_at,
            })
            .collect()
    }

    /// Merges `patch` into the device's stored state: each top-level field
    /// replaces the stored one, and a `null` removes it. A state left with no
    /// fields is dropped.
    pub async fn merge_device_state(
        &self,
        device_id: &str,
        device_name: &str,
        patch: Map<String, Value>,
    ) -> Result<()> {
        let device_id = validate_id(device_id)?.to_owned();
        if patch.is_empty() {
            return invalid("snapshot needs at least one field");
        }
        let device_name = device_name.trim().to_owned();
        self.update_config(|config| {
            let existing = config
                .device_states
                .iter()
                .position(|state| state.device_id.eq_ignore_ascii_case(&device_id));
            if existing.is_none() && config.device_states.len() >= MAX_DEVICE_STATES {
                return invalid(format!("at most {MAX_DEVICE_STATES} devices keep a state"));
            }
            let mut state = existing
                .map(|index| config.device_states[index].clone())
                .unwrap_or_else(|| DeviceState {
                    device_id: device_id.clone(),
                    device_name: device_name.clone(),
                    snapshot: Map::new(),
                    updated_at: 0,
                });
            if !device_name.is_empty() {
                state.device_name = device_name.clone();
            }
            for (field, value) in patch {
                if value.is_null() {
                    state.snapshot.remove(&field);
                } else {
                    state.snapshot.insert(field, value);
                }
            }
            if serde_json::to_vec(&state.snapshot)?.len() > MAX_DEVICE_STATE_BYTES {
                return invalid(format!(
                    "a device state may hold at most {MAX_DEVICE_STATE_BYTES} bytes"
                ));
            }
            state.updated_at = unix_now();
            match (existing, state.snapshot.is_empty()) {
                (Some(index), true) => {
                    config.device_states.remove(index);
                }
                (Some(index), false) => config.device_states[index] = state,
                (None, true) => {}
                (None, false) => config.device_states.push(state),
            }
            Ok(())
        })
        .await?;
        *self
            .reassert
            .last_change
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Instant::now());
        Ok(())
    }

    /// Reads what the device is running right now (say, a colour just picked in
    /// vendor software) and stores it as the state to replay. Returns how many
    /// zones were stored. Only a device that reports its own effect can be
    /// captured; for one that cannot, nothing is stored and the error says so.
    /// Capture while the lighting is the one you want kept: right after a lock
    /// or a sleep, vendor software may have reset it, and that is what would be
    /// stored.
    pub async fn capture_device_state(&self, device_id: &str) -> Result<usize> {
        let device_id = validate_id(device_id)?.to_owned();
        let _serial = self.reassert.serial.lock().await;
        if crate::hid::client_session_active() {
            return invalid(
                "the control panel holds this device over a HID session; try again when it lets go",
            );
        }
        let driver = self.reassert.driver();
        let hint = self.reassert.hint(&device_id);
        let id = device_id.clone();
        let (zones, answered) =
            tokio::task::spawn_blocking(move || driver.read_lighting(&id, hint))
                .await
                .map_err(|error| anyhow::anyhow!("the capture worker stopped: {error}"))??;
        if let Some(product_id) = answered {
            self.reassert.learn(&device_id, product_id);
        }
        if zones.is_empty() {
            return invalid(
                "the device did not report a lighting effect; it may not support being read, so set its state through PUT /v1/device-state instead",
            );
        }
        let count = zones.len();
        let name = device_id.split_once(':').map_or("", |(_, name)| name);
        // `lightingZones` replaces any single `lighting` zone stored earlier, so the two never disagree.
        let patch = serde_json::json!({ "lightingZones": zones, "lighting": null })
            .as_object()
            .cloned()
            .unwrap_or_default();
        self.merge_device_state(&device_id, name, patch).await?;
        Ok(count)
    }

    /// Records a power event and says whether the lights have to be rewritten
    /// because of it: they do when it switched the lights-off override on or off,
    /// and always on resume, which restores the stored colours as before.
    async fn note_power_event(&self, trigger: Trigger) -> bool {
        let settings = self.inner.read().await.config.lighting.clone();
        let before = self.reassert.override_active(&settings);
        {
            let mut power = self
                .reassert
                .power
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            match trigger {
                Trigger::DisplayOff => power.display_off = true,
                Trigger::DisplayOn => power.display_off = false,
                Trigger::Suspend => power.asleep = true,
                // Waking clears both, so a monitor-off that never reported back
                // cannot leave the lights off.
                Trigger::Resume => {
                    power.asleep = false;
                    power.display_off = false;
                }
                _ => {}
            }
        }
        trigger == Trigger::Resume || before != self.reassert.override_active(&settings)
    }

    /// How lighting is managed.
    pub async fn lighting_settings(&self) -> LightingSettings {
        self.inner.read().await.config.lighting.clone()
    }

    /// Saves the lighting options and applies them: if the monitor is off right
    /// now and the option was just turned on, the lights go off at once.
    pub async fn set_lighting_settings(&self, settings: LightingSettings) -> Result<()> {
        self.update_config(|config| {
            config.lighting = settings;
            Ok(())
        })
        .await?;
        self.request_reassert(Trigger::Manual);
        Ok(())
    }

    /// Whether Razer Synapse looks to be running (checked at most every 5 s).
    pub async fn razer_software_running(&self) -> bool {
        if let Some((at, running)) = *self
            .reassert
            .synapse
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            && at.elapsed() < Duration::from_secs(5)
        {
            return running;
        }
        let running = tokio::task::spawn_blocking(razer_software_running)
            .await
            .unwrap_or(false);
        *self
            .reassert
            .synapse
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some((Instant::now(), running));
        running
    }

    /// The connected devices that report lighting, with what each is running and
    /// what Bridge keeps for it. A scan takes a few seconds, so one is reused for
    /// a minute unless `refresh` asks for a new one.
    pub async fn lighting_devices(&self, refresh: bool) -> Result<Vec<LightingDevice>> {
        let cached = self
            .reassert
            .detected
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .filter(|(at, _)| !refresh && at.elapsed() < DETECTION_TTL)
            .map(|(_, devices)| devices);
        let scanned = match cached {
            Some(devices) => devices,
            None => {
                let _serial = self.reassert.serial.lock().await;
                let driver = self.reassert.driver();
                let devices = tokio::task::spawn_blocking(move || driver.list_lighting())
                    .await
                    .map_err(|error| anyhow::anyhow!("the device scan stopped: {error}"))??;
                *self
                    .reassert
                    .detected
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) =
                    Some((Instant::now(), devices.clone()));
                devices
            }
        };
        let states = self.inner.read().await.config.device_states.clone();
        let mut devices = Vec::new();
        for device in scanned {
            let name = device
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if name.is_empty() {
                continue;
            }
            let id = format!("Razer:{name}");
            let product_id = device.get("productId").and_then(Value::as_u64).unwrap_or(0);
            if let Ok(product_id) = u16::try_from(product_id) {
                self.reassert.learn(&id, product_id);
            }
            let stored = states
                .iter()
                .find(|state| state.device_id.eq_ignore_ascii_case(&id))
                .and_then(|state| state.snapshot.get("lightingZones").cloned());
            devices.push(LightingDevice {
                last_replay: self.reassert.last_replay(&id),
                firmware: device
                    .get("firmware")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| item.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default(),
                zones: device
                    .get("zones")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
                id,
                name,
                product_id,
                stored,
            });
        }
        Ok(devices)
    }

    /// Sets every lighting zone of a connected device to one static colour: the
    /// colour is stored as the state to keep, and written to the device now.
    pub async fn set_lighting_colour(&self, device_id: &str, color: &str) -> Result<()> {
        let color = normalize_color(color)?;
        let wanted = device_id.trim();
        let mut devices = self.lighting_devices(false).await?;
        if !devices
            .iter()
            .any(|device| device.id.eq_ignore_ascii_case(wanted))
        {
            // A device plugged in since the last scan.
            devices = self.lighting_devices(true).await?;
        }
        let Some(device) = devices
            .into_iter()
            .find(|device| device.id.eq_ignore_ascii_case(wanted))
        else {
            return invalid(format!("no connected device is named \"{wanted}\""));
        };
        if device.zones.is_empty() {
            return invalid("this device reports no lighting zones to colour");
        }
        let zones: Vec<Value> = device
            .zones
            .iter()
            .filter_map(|zone| zone.get("zone").and_then(Value::as_str))
            .map(|zone| serde_json::json!({ "zone": zone, "mode": "Static", "color": color }))
            .collect();
        let patch = serde_json::json!({ "lightingZones": zones, "lighting": null })
            .as_object()
            .cloned()
            .unwrap_or_default();
        self.merge_device_state(&device.id, &device.name, patch)
            .await?;
        self.replay_inner(Trigger::Manual, Some(&device.id)).await;
        match self.reassert.last_replay(&device.id) {
            Some(record) if record.outcome == "applied" => Ok(()),
            Some(record) => Err(anyhow::anyhow!(
                "the colour was stored but could not be written: {}",
                record.error.unwrap_or_else(|| record.outcome.to_owned())
            )),
            None => Err(anyhow::anyhow!("the colour was stored but not written")),
        }
    }

    /// Forgets a device's stored state. Returns whether there was one.
    pub async fn forget_device_state(&self, device_id: &str) -> Result<bool> {
        let device_id = device_id.trim().to_owned();
        let mut removed = false;
        self.update_config(|config| {
            let before = config.device_states.len();
            config
                .device_states
                .retain(|state| !state.device_id.eq_ignore_ascii_case(&device_id));
            removed = config.device_states.len() != before;
            Ok(())
        })
        .await?;
        Ok(removed)
    }

    /// Replays the stored state onto every device it names. A device that is
    /// not on the bus is skipped (the event for it plugging in comes later),
    /// and one that fails is logged and left for the next trigger. Returns how
    /// many devices were written.
    pub async fn replay_device_states(&self, trigger: Trigger) -> usize {
        self.replay_inner(trigger, None).await
    }

    /// [`BridgeService::replay_device_states`], for one device when `only` names it.
    async fn replay_inner(&self, trigger: Trigger, only: Option<&str>) -> usize {
        let _serial = self.reassert.serial.lock().await;
        if crate::hid::client_session_active() {
            tracing::debug!(
                ?trigger,
                "not replaying device state: the panel holds a HID session"
            );
            return 0;
        }
        let (states, overridden) = {
            let state = self.inner.read().await;
            let overridden = self.reassert.override_active(&state.config.lighting);
            (state.config.device_states.clone(), overridden)
        };
        // While the monitor is off or the PC asleep (and the option is on) every
        // device is held at the lights-off version of its stored state.
        let states: Vec<DeviceState> = states
            .iter()
            .filter(|state| only.is_none_or(|id| state.device_id.eq_ignore_ascii_case(id)))
            .map(|state| desired_state(state, overridden))
            .collect();
        if states.is_empty() {
            return 0;
        }
        if matches!(trigger, Trigger::Watchdog | Trigger::Verify)
            && self.reassert.recently_changed()
        {
            return 0;
        }
        let driver = self.reassert.driver();
        let present = {
            let driver = Arc::clone(&driver);
            tokio::task::spawn_blocking(move || driver.present_brands())
                .await
                .unwrap_or_default()
        };
        let mut written = 0;
        for state in states {
            // The blind watchdog leaves animated lighting alone, and leaves any
            // device the verifier can read back to the verifier.
            if trigger == Trigger::Watchdog
                && (!state.is_idempotent() || self.reassert.is_readable(&state.device_id))
            {
                continue;
            }
            let brand = state.brand().to_ascii_lowercase();
            let quiet = matches!(trigger, Trigger::Watchdog | Trigger::Verify);
            if trigger != Trigger::Manual && !present.contains(&brand) {
                if !quiet {
                    self.reassert
                        .record(&state.device_id, trigger, "notConnected", None);
                    tracing::info!(?trigger, device = %state.device_id, "not replaying device state: no such device is connected");
                }
                continue;
            }
            if trigger == Trigger::Verify {
                match self.verify_state(&driver, &state).await {
                    Verdict::Matches => {
                        self.reassert
                            .record(&state.device_id, trigger, "verified", None);
                        continue;
                    }
                    Verdict::Unreadable => continue,
                    Verdict::Failed(message) => {
                        tracing::debug!(device = %state.device_id, error = %message, "could not read the device back");
                        continue;
                    }
                    Verdict::Differs => {
                        tracing::info!(device = %state.device_id, "the device no longer matches the stored state; repairing it");
                    }
                }
            }
            let applying = state.clone();
            let applier = Arc::clone(&driver);
            let hint = self.reassert.hint(&state.device_id);
            let outcome = tokio::task::spawn_blocking(move || applier.apply(&applying, hint))
                .await
                .unwrap_or_else(|error| Err(anyhow::anyhow!("the replay worker stopped: {error}")));
            match outcome {
                Ok((true, answered)) => {
                    written += 1;
                    if let Some(product_id) = answered {
                        self.reassert.learn(&state.device_id, product_id);
                    }
                    self.reassert
                        .record(&state.device_id, trigger, "applied", None);
                    if trigger == Trigger::Watchdog {
                        tracing::debug!(device = %state.device_id, "replayed device state");
                    } else {
                        tracing::info!(?trigger, device = %state.device_id, "replayed device state");
                    }
                }
                Ok((false, _)) => {
                    self.reassert
                        .record(&state.device_id, trigger, "noDriver", None);
                    tracing::warn!(?trigger, device = %state.device_id, "no native driver can replay this device's state");
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    self.reassert.record(
                        &state.device_id,
                        trigger,
                        "failed",
                        Some(message.clone()),
                    );
                    tracing::warn!(?trigger, device = %state.device_id, error = %message, "could not replay device state");
                }
            }
        }
        written
    }

    /// Reads the device back and compares it with the stored state.
    async fn verify_state(&self, driver: &Arc<dyn ReplayDriver>, state: &DeviceState) -> Verdict {
        let stored = stored_zones(state);
        // Nothing to read back, or a zone (a single LED) the read cannot report.
        if stored.is_empty()
            || stored
                .iter()
                .any(|zone| zone.get("hardwareZoneId").is_some())
        {
            return Verdict::Unreadable;
        }
        let reader = Arc::clone(driver);
        let id = state.device_id.clone();
        let hint = self.reassert.hint(&id);
        match tokio::task::spawn_blocking(move || reader.read_lighting(&id, hint)).await {
            Ok(Ok((zones, answered))) => {
                if let Some(product_id) = answered {
                    self.reassert.learn(&state.device_id, product_id);
                }
                if zones.is_empty() {
                    return Verdict::Unreadable;
                }
                self.reassert.mark_readable(&state.device_id);
                if lighting_matches(&stored, &zones) {
                    Verdict::Matches
                } else {
                    Verdict::Differs
                }
            }
            Ok(Err(error)) => Verdict::Failed(format!("{error:#}")),
            Err(error) => Verdict::Failed(format!("the read worker stopped: {error}")),
        }
    }

    /// Starts the monitor that turns triggers into replays: it drains the
    /// requests made through [`BridgeService::request_reassert`], notices a
    /// resume from a late tick and the panel letting go of the device, verifies
    /// devices that can be read back, and runs the blind watchdog for the rest.
    /// A monitor that panics is logged and started again, since a keepalive that
    /// silently stops is worse than none.
    pub fn start_reassert_monitor(&self) {
        let service = self.clone();
        tokio::spawn(async move {
            loop {
                let monitor = service.clone();
                match tokio::spawn(async move { monitor.reassert_loop().await }).await {
                    Err(error) if error.is_panic() => {
                        tracing::error!("the reassert monitor panicked; restarting it");
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                    _ => break,
                }
            }
        });
    }

    async fn reassert_loop(&self) {
        let service = self.clone();
        {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut last_wall = SystemTime::now();
            let mut last_watchdog = Instant::now();
            let mut last_verify = Instant::now();
            let mut panel_was_connected = false;
            service.request_reassert(Trigger::Startup);
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = service.reassert.wake.notified() => {}
                }
                let now = SystemTime::now();
                if now
                    .duration_since(last_wall)
                    .is_ok_and(|gap| gap >= RESUME_GAP)
                {
                    tracing::info!("system resume detected");
                    service.request_reassert(Trigger::Resume);
                }
                last_wall = now;

                let (connected, interval, verify_interval) = {
                    let state = service.inner.read().await;
                    (
                        panel_connected(&state),
                        Duration::from_secs(state.config.reassert_interval_seconds),
                        Duration::from_secs(state.config.verify_interval_seconds),
                    )
                };
                if panel_was_connected && !connected {
                    service.request_reassert(Trigger::PanelReleased);
                }
                panel_was_connected = connected;

                if !interval.is_zero() && last_watchdog.elapsed() >= interval {
                    last_watchdog = Instant::now();
                    service.request_reassert(Trigger::Watchdog);
                }
                if !verify_interval.is_zero() && last_verify.elapsed() >= verify_interval {
                    last_verify = Instant::now();
                    service.request_reassert(Trigger::Verify);
                }

                for trigger in service.reassert.take_pending() {
                    // Power events change what the lights should be doing; only
                    // replay when they changed it.
                    if matches!(
                        trigger,
                        Trigger::DisplayOff
                            | Trigger::DisplayOn
                            | Trigger::Suspend
                            | Trigger::Resume
                    ) && !service.note_power_event(trigger).await
                    {
                        continue;
                    }
                    let Some(guard) = service.reassert.begin_schedule(trigger) else {
                        tracing::debug!(?trigger, "already replaying for this trigger");
                        continue;
                    };
                    let service = service.clone();
                    tokio::spawn(async move {
                        let _guard = guard;
                        let mut waited = Duration::ZERO;
                        for delay in trigger.settle_delays() {
                            tokio::time::sleep(delay.saturating_sub(waited)).await;
                            waited = *delay;
                            service.replay_device_states(trigger).await;
                        }
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::config::BridgeConfig;

    struct FakeDriver {
        brands: Vec<&'static str>,
        calls: Mutex<Vec<String>>,
        hints: Mutex<Vec<Option<u16>>>,
        reading: Mutex<Vec<Value>>,
        listing: Mutex<Vec<Value>>,
        applied_states: Mutex<Vec<DeviceState>>,
        fail: bool,
        applied: AtomicUsize,
    }

    impl FakeDriver {
        fn new(brands: &[&'static str]) -> Arc<Self> {
            Arc::new(Self {
                brands: brands.to_vec(),
                calls: Mutex::default(),
                hints: Mutex::default(),
                reading: Mutex::default(),
                listing: Mutex::default(),
                applied_states: Mutex::default(),
                fail: false,
                applied: AtomicUsize::new(0),
            })
        }
    }

    impl ReplayDriver for FakeDriver {
        fn present_brands(&self) -> HashSet<String> {
            self.brands
                .iter()
                .map(|brand| brand.to_ascii_lowercase())
                .collect()
        }

        fn apply(
            &self,
            state: &DeviceState,
            hint_product_id: Option<u16>,
        ) -> Result<(bool, Option<u16>)> {
            self.calls.lock().unwrap().push(state.device_id.clone());
            self.applied_states.lock().unwrap().push(state.clone());
            self.hints.lock().unwrap().push(hint_product_id);
            self.applied.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                anyhow::bail!("the mouse did not answer");
            }
            Ok((true, Some(0x007b)))
        }

        fn read_lighting(
            &self,
            _device_id: &str,
            hint_product_id: Option<u16>,
        ) -> Result<(Vec<Value>, Option<u16>)> {
            self.hints.lock().unwrap().push(hint_product_id);
            if self.fail {
                anyhow::bail!("the mouse did not answer");
            }
            Ok((self.reading.lock().unwrap().clone(), Some(0x007b)))
        }

        fn list_lighting(&self) -> Result<Vec<Value>> {
            if self.fail {
                anyhow::bail!("the scan failed");
            }
            Ok(self.listing.lock().unwrap().clone())
        }
    }

    fn bridge() -> BridgeService {
        let path = std::env::temp_dir().join(format!(
            "bridge-reassert-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        BridgeService::new(BridgeConfig::default(), path.join("config.json"))
    }

    fn patch(value: Value) -> Map<String, Value> {
        value.as_object().cloned().expect("an object")
    }

    fn zone(mode: &str) -> Value {
        serde_json::json!({ "zone": "Logo", "mode": mode, "color": "#ff0000" })
    }

    #[tokio::test]
    async fn merging_replaces_fields_and_null_removes_them() {
        let bridge = bridge();
        bridge
            .merge_device_state(
                "Razer:Basilisk V3",
                "Basilisk V3",
                patch(serde_json::json!({ "lighting": zone("Static"), "dpi": 800 })),
            )
            .await
            .unwrap();
        bridge
            .merge_device_state(
                "razer:basilisk v3",
                "",
                patch(serde_json::json!({ "dpi": null, "lighting": zone("Off") })),
            )
            .await
            .unwrap();
        let states = bridge.device_states().await;
        assert_eq!(states.len(), 1, "the id matches without regard to case");
        assert_eq!(
            states[0].device_name, "Basilisk V3",
            "a blank name keeps the stored one"
        );
        assert!(!states[0].snapshot.contains_key("dpi"));
        assert_eq!(states[0].snapshot["lighting"]["mode"], "Off");
        assert!(states[0].updated_at > 0);
    }

    #[tokio::test]
    async fn a_state_with_no_fields_left_is_dropped_and_survives_a_restart_otherwise() {
        let bridge = bridge();
        bridge
            .merge_device_state(
                "Razer:M",
                "M",
                patch(serde_json::json!({ "lighting": zone("Static") })),
            )
            .await
            .unwrap();
        let saved: BridgeConfig =
            serde_json::from_slice(&std::fs::read(&*bridge.config_path).unwrap()).unwrap();
        assert_eq!(saved.device_states.len(), 1, "persisted to the config file");
        assert_eq!(
            saved.device_states[0].snapshot["lighting"]["mode"],
            "Static"
        );
        bridge
            .merge_device_state(
                "Razer:M",
                "M",
                patch(serde_json::json!({ "lighting": null })),
            )
            .await
            .unwrap();
        assert!(bridge.device_states().await.is_empty());
    }

    #[tokio::test]
    async fn bad_ids_empty_patches_and_oversized_states_are_invalid_input() {
        let bridge = bridge();
        let one = || patch(serde_json::json!({ "dpi": 800 }));
        for id in ["", "NoColon", ":NoBrand", &"x:".repeat(200)] {
            let error = bridge.merge_device_state(id, "n", one()).await.unwrap_err();
            assert!(error.downcast_ref::<InvalidInput>().is_some(), "{id:?}");
        }
        let error = bridge
            .merge_device_state("Razer:M", "n", Map::new())
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<InvalidInput>().is_some());
        let huge = patch(serde_json::json!({ "blob": "x".repeat(MAX_DEVICE_STATE_BYTES) }));
        let error = bridge
            .merge_device_state("Razer:M", "n", huge)
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<InvalidInput>().is_some());
        assert!(
            bridge.device_states().await.is_empty(),
            "nothing was stored"
        );
    }

    #[tokio::test]
    async fn events_replay_everything_but_the_watchdog_leaves_animated_lighting_alone() {
        let bridge = bridge();
        let driver = FakeDriver::new(&["Razer"]);
        bridge.reassert.set_driver(driver.clone());
        for (id, mode) in [
            ("Razer:Static Mouse", "Static"),
            ("Razer:Wave Mouse", "Wave"),
        ] {
            bridge
                .merge_device_state(id, id, patch(serde_json::json!({ "lighting": zone(mode) })))
                .await
                .unwrap();
        }
        *bridge.reassert.last_change.lock().unwrap() = None;

        assert_eq!(bridge.replay_device_states(Trigger::SessionUnlock).await, 2);
        assert_eq!(driver.calls.lock().unwrap().len(), 2);

        driver.calls.lock().unwrap().clear();
        assert_eq!(bridge.replay_device_states(Trigger::Watchdog).await, 1);
        assert_eq!(*driver.calls.lock().unwrap(), ["Razer:Static Mouse"]);
    }

    #[tokio::test]
    async fn the_watchdog_waits_out_a_change_the_panel_is_still_writing() {
        let bridge = bridge();
        let driver = FakeDriver::new(&["Razer"]);
        bridge.reassert.set_driver(driver.clone());
        bridge
            .merge_device_state(
                "Razer:M",
                "M",
                patch(serde_json::json!({ "lighting": zone("Static") })),
            )
            .await
            .unwrap();
        assert_eq!(bridge.replay_device_states(Trigger::Watchdog).await, 0);
        assert_eq!(
            bridge.replay_device_states(Trigger::Resume).await,
            1,
            "an event still replays right away"
        );
    }

    #[tokio::test]
    async fn a_device_that_is_not_connected_is_skipped_unless_asked_for_by_hand() {
        let bridge = bridge();
        let driver = FakeDriver::new(&["Logitech"]);
        bridge.reassert.set_driver(driver.clone());
        bridge
            .merge_device_state(
                "Razer:M",
                "M",
                patch(serde_json::json!({ "lighting": zone("Static") })),
            )
            .await
            .unwrap();
        *bridge.reassert.last_change.lock().unwrap() = None;

        assert_eq!(bridge.replay_device_states(Trigger::Resume).await, 0);
        assert!(driver.calls.lock().unwrap().is_empty());
        let status = bridge.device_states().await;
        assert_eq!(
            status[0].last_replay.as_ref().unwrap().outcome,
            "notConnected"
        );

        assert_eq!(bridge.replay_device_states(Trigger::Manual).await, 1);
        assert_eq!(status.len(), 1);
    }

    #[tokio::test]
    async fn a_failed_replay_is_recorded_with_its_error_and_does_not_stop_the_others() {
        let bridge = bridge();
        let driver = Arc::new(FakeDriver {
            brands: vec!["Razer"],
            calls: Mutex::default(),
            hints: Mutex::default(),
            reading: Mutex::default(),
            listing: Mutex::default(),
            applied_states: Mutex::default(),
            fail: true,
            applied: AtomicUsize::new(0),
        });
        bridge.reassert.set_driver(driver.clone());
        for id in ["Razer:A", "Razer:B"] {
            bridge
                .merge_device_state(
                    id,
                    id,
                    patch(serde_json::json!({ "lighting": zone("Static") })),
                )
                .await
                .unwrap();
        }
        *bridge.reassert.last_change.lock().unwrap() = None;
        assert_eq!(bridge.replay_device_states(Trigger::Resume).await, 0);
        assert_eq!(driver.applied.load(Ordering::SeqCst), 2, "both were tried");
        let status = bridge.device_states().await;
        let record = status[0].last_replay.as_ref().unwrap();
        assert_eq!(record.outcome, "failed");
        assert_eq!(record.error.as_deref(), Some("the mouse did not answer"));
    }

    #[test]
    fn requests_coalesce_by_trigger() {
        let service = bridge();
        service.request_reassert(Trigger::Resume);
        service.request_reassert(Trigger::SessionUnlock);
        service.request_reassert(Trigger::Resume);
        assert_eq!(
            service.reassert.take_pending(),
            [Trigger::Resume, Trigger::SessionUnlock]
        );
        assert!(service.reassert.take_pending().is_empty());
    }

    #[test]
    fn a_trigger_already_replaying_is_not_queued_again_until_it_finishes() {
        let service = bridge();
        let first = service.reassert.begin_schedule(Trigger::SessionLock);
        assert!(first.is_some());
        assert!(
            service
                .reassert
                .begin_schedule(Trigger::SessionLock)
                .is_none(),
            "a second lock while the first is still replaying only repeats it"
        );
        assert!(
            service
                .reassert
                .begin_schedule(Trigger::SessionUnlock)
                .is_some(),
            "a different trigger is its own schedule"
        );
        drop(first);
        assert!(
            service
                .reassert
                .begin_schedule(Trigger::SessionLock)
                .is_some()
        );
    }

    #[tokio::test]
    async fn the_product_that_answered_is_tried_first_next_time() {
        let bridge = bridge();
        let driver = FakeDriver::new(&["Razer"]);
        bridge.reassert.set_driver(driver.clone());
        bridge
            .merge_device_state(
                "Razer:Razer Viper Ultimate",
                "Razer Viper Ultimate",
                patch(serde_json::json!({ "lighting": zone("Static") })),
            )
            .await
            .unwrap();
        *bridge.reassert.last_change.lock().unwrap() = None;

        bridge.replay_device_states(Trigger::Resume).await;
        bridge.replay_device_states(Trigger::Resume).await;

        assert_eq!(
            *driver.hints.lock().unwrap(),
            [None, Some(0x007b)],
            "the first replay has nothing to go on, the second starts with what answered"
        );
    }

    #[test]
    fn lock_and_unlock_replay_again_after_vendor_software_has_had_its_turn() {
        for trigger in [Trigger::SessionLock, Trigger::SessionUnlock] {
            let delays = trigger.settle_delays();
            assert_eq!(delays[0], Duration::ZERO);
            assert!(
                delays
                    .last()
                    .is_some_and(|last| *last >= Duration::from_secs(8))
            );
        }
    }

    #[tokio::test]
    async fn capture_stores_what_the_device_reports_and_replaces_a_single_lighting_zone() {
        let bridge = bridge();
        let driver = FakeDriver::new(&["Razer"]);
        *driver.reading.lock().unwrap() = vec![
            serde_json::json!({ "zone": "Logo", "mode": "Static", "color": "#ff0000", "brightness": 33 }),
        ];
        bridge.reassert.set_driver(driver.clone());
        bridge
            .merge_device_state(
                "Razer:Razer Viper Ultimate",
                "Razer Viper Ultimate",
                patch(serde_json::json!({ "lighting": zone("Static"), "dpi": 800 })),
            )
            .await
            .unwrap();

        let zones = bridge
            .capture_device_state("Razer:Razer Viper Ultimate")
            .await
            .unwrap();

        assert_eq!(zones, 1);
        let state = bridge.device_states().await.remove(0);
        assert_eq!(state.snapshot["lightingZones"][0]["color"], "#ff0000");
        assert!(
            !state.snapshot.contains_key("lighting"),
            "the older single zone is gone"
        );
        assert_eq!(state.snapshot["dpi"], 800, "other stored fields are kept");
    }

    #[tokio::test]
    async fn capture_creates_a_state_for_a_device_that_had_none_and_remembers_what_answered() {
        let bridge = bridge();
        let driver = FakeDriver::new(&["Razer"]);
        *driver.reading.lock().unwrap() =
            vec![serde_json::json!({ "zone": "Logo", "mode": "Off" })];
        bridge.reassert.set_driver(driver.clone());

        bridge
            .capture_device_state("Razer:Razer Viper Ultimate")
            .await
            .unwrap();
        bridge
            .capture_device_state("Razer:Razer Viper Ultimate")
            .await
            .unwrap();

        let state = bridge.device_states().await.remove(0);
        assert_eq!(state.device_name, "Razer Viper Ultimate");
        assert_eq!(*driver.hints.lock().unwrap(), [None, Some(0x007b)]);
    }

    #[tokio::test]
    async fn capture_of_a_device_that_reports_nothing_stores_nothing_and_says_why() {
        let bridge = bridge();
        bridge.reassert.set_driver(FakeDriver::new(&["Razer"]));

        let error = bridge
            .capture_device_state("Razer:Goliathus")
            .await
            .unwrap_err();

        assert!(error.downcast_ref::<InvalidInput>().is_some());
        assert!(
            error
                .to_string()
                .contains("did not report a lighting effect")
        );
        assert!(bridge.device_states().await.is_empty());
    }

    #[tokio::test]
    async fn capture_failures_and_bad_ids_leave_the_stored_state_alone() {
        let bridge = bridge();
        let driver = Arc::new(FakeDriver {
            brands: vec!["Razer"],
            calls: Mutex::default(),
            hints: Mutex::default(),
            reading: Mutex::default(),
            listing: Mutex::default(),
            applied_states: Mutex::default(),
            fail: true,
            applied: AtomicUsize::new(0),
        });
        bridge.reassert.set_driver(driver);
        bridge
            .merge_device_state(
                "Razer:M",
                "M",
                patch(serde_json::json!({ "lighting": zone("Static") })),
            )
            .await
            .unwrap();

        assert!(bridge.capture_device_state("Razer:M").await.is_err());
        assert!(
            bridge
                .capture_device_state("no colon")
                .await
                .unwrap_err()
                .downcast_ref::<InvalidInput>()
                .is_some()
        );
        assert_eq!(
            bridge.device_states().await[0].snapshot["lighting"]["mode"],
            "Static"
        );
    }

    fn purple() -> Value {
        serde_json::json!({ "zone": "Logo", "mode": "Static", "color": "#760088" })
    }

    async fn viper_with_state(reading: Vec<Value>) -> (BridgeService, Arc<FakeDriver>) {
        let bridge = bridge();
        let driver = FakeDriver::new(&["Razer"]);
        *driver.reading.lock().unwrap() = reading;
        bridge.reassert.set_driver(driver.clone());
        bridge
            .merge_device_state(
                "Razer:Razer Viper Ultimate",
                "Razer Viper Ultimate",
                patch(serde_json::json!({ "lightingZones": [purple()] })),
            )
            .await
            .unwrap();
        *bridge.reassert.last_change.lock().unwrap() = None;
        (bridge, driver)
    }

    #[tokio::test]
    async fn verify_leaves_a_device_alone_when_it_is_running_the_stored_effect() {
        let (bridge, driver) = viper_with_state(vec![purple()]).await;

        assert_eq!(bridge.replay_device_states(Trigger::Verify).await, 0);

        assert!(
            driver.calls.lock().unwrap().is_empty(),
            "nothing was written"
        );
        let status = bridge.device_states().await;
        assert_eq!(status[0].last_replay.as_ref().unwrap().outcome, "verified");
    }

    #[tokio::test]
    async fn verify_repairs_a_device_that_was_reset_to_something_else() {
        // The mouse fell back to Razer's default spectrum cycling.
        let (bridge, driver) = viper_with_state(vec![
            serde_json::json!({ "zone": "Logo", "mode": "Spectrum" }),
        ])
        .await;

        assert_eq!(bridge.replay_device_states(Trigger::Verify).await, 1);

        assert_eq!(
            driver.calls.lock().unwrap().len(),
            1,
            "the stored effect was written back"
        );
        let status = bridge.device_states().await;
        assert_eq!(
            status[0].last_replay.as_ref().unwrap().trigger,
            Trigger::Verify
        );
        assert_eq!(status[0].last_replay.as_ref().unwrap().outcome, "applied");
    }

    #[tokio::test]
    async fn verify_notices_a_different_colour_and_ignores_case_and_brightness() {
        let (bridge, driver) = viper_with_state(vec![
            serde_json::json!({ "zone": "Logo", "mode": "static", "color": "#760088", "brightness": 100 }),
        ])
        .await;
        assert_eq!(
            bridge.replay_device_states(Trigger::Verify).await,
            0,
            "case and brightness are not drift"
        );

        *driver.reading.lock().unwrap() =
            vec![serde_json::json!({ "zone": "Logo", "mode": "Static", "color": "#ff0000" })];
        assert_eq!(
            bridge.replay_device_states(Trigger::Verify).await,
            1,
            "a different colour is"
        );
    }

    #[tokio::test]
    async fn an_unreadable_device_is_left_to_the_blind_watchdog() {
        let (bridge, driver) = viper_with_state(Vec::new()).await;

        assert_eq!(bridge.replay_device_states(Trigger::Verify).await, 0);
        assert!(
            driver.calls.lock().unwrap().is_empty(),
            "verify never writes blind"
        );

        assert_eq!(
            bridge.replay_device_states(Trigger::Watchdog).await,
            1,
            "the watchdog still covers it"
        );
    }

    #[tokio::test]
    async fn the_watchdog_leaves_a_device_the_verifier_can_read_to_the_verifier() {
        let (bridge, driver) = viper_with_state(vec![purple()]).await;
        bridge.replay_device_states(Trigger::Verify).await;

        assert_eq!(bridge.replay_device_states(Trigger::Watchdog).await, 0);
        assert!(driver.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_failed_read_does_not_write_and_does_not_hide_the_last_real_outcome() {
        let (bridge, _working) = viper_with_state(vec![purple()]).await;
        bridge.replay_device_states(Trigger::Manual).await;
        let driver = Arc::new(FakeDriver {
            brands: vec!["Razer"],
            calls: Mutex::default(),
            hints: Mutex::default(),
            reading: Mutex::default(),
            listing: Mutex::default(),
            applied_states: Mutex::default(),
            fail: true,
            applied: AtomicUsize::new(0),
        });
        bridge.reassert.set_driver(driver.clone());

        assert_eq!(bridge.replay_device_states(Trigger::Verify).await, 0);

        assert_eq!(
            driver.applied.load(Ordering::SeqCst),
            0,
            "a device that did not answer is not written to"
        );
        let status = bridge.device_states().await;
        assert_eq!(
            status[0].last_replay.as_ref().unwrap().trigger,
            Trigger::Manual
        );
    }

    #[test]
    fn a_state_with_a_single_led_zone_cannot_be_verified() {
        let state = DeviceState {
            device_id: "Razer:Mouse".into(),
            device_name: "Mouse".into(),
            snapshot: patch(serde_json::json!({
                "lightingZones": [{ "zone": "LED 0", "mode": "Static", "color": "#fff", "hardwareZoneId": 0 }]
            })),
            updated_at: 0,
        };
        assert!(
            stored_zones(&state)
                .iter()
                .any(|zone| zone.get("hardwareZoneId").is_some())
        );
    }

    fn power_options(display: bool, sleep: bool) -> LightingSettings {
        LightingSettings {
            off_when_display_off: display,
            off_on_sleep: sleep,
            ..LightingSettings::default()
        }
    }

    fn last_applied_zone(driver: &FakeDriver) -> Value {
        let applied = driver.applied_states.lock().unwrap();
        applied.last().expect("something was written").snapshot["lightingZones"][0].clone()
    }

    fn listed(name: &str, product: u64, zones: &[&str]) -> Value {
        serde_json::json!({
            "name": name,
            "productId": product,
            "firmware": ["Firmware 1.0"],
            "zones": zones.iter().map(|zone| serde_json::json!({ "zone": zone, "mode": "Spectrum", "readable": true })).collect::<Vec<_>>(),
        })
    }

    #[tokio::test]
    async fn the_lights_go_off_with_the_monitor_and_the_stored_colour_comes_back_with_it() {
        let (bridge, driver) = viper_with_state(Vec::new()).await;
        bridge
            .set_lighting_settings(power_options(true, false))
            .await
            .unwrap();

        assert!(
            bridge.note_power_event(Trigger::DisplayOff).await,
            "turning the lights off needs a write"
        );
        bridge.replay_device_states(Trigger::DisplayOff).await;
        let off = last_applied_zone(&driver);
        assert_eq!(off["mode"], "Off");
        assert!(off.get("color").is_none(), "an off zone carries no colour");
        assert_eq!(
            bridge.device_states().await[0].snapshot["lightingZones"][0]["color"],
            "#760088",
            "the stored colour is untouched"
        );

        assert!(bridge.note_power_event(Trigger::DisplayOn).await);
        bridge.replay_device_states(Trigger::DisplayOn).await;
        let back = last_applied_zone(&driver);
        assert_eq!(
            (back["mode"].as_str(), back["color"].as_str()),
            (Some("Static"), Some("#760088"))
        );
    }

    #[tokio::test]
    async fn with_the_option_off_a_monitor_event_changes_nothing() {
        let (bridge, driver) = viper_with_state(Vec::new()).await;
        bridge
            .set_lighting_settings(power_options(false, true))
            .await
            .unwrap();

        assert!(!bridge.note_power_event(Trigger::DisplayOff).await);
        assert!(!bridge.note_power_event(Trigger::DisplayOn).await);
        assert!(driver.applied_states.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn sleep_turns_the_lights_off_and_resume_restores_them() {
        let (bridge, driver) = viper_with_state(Vec::new()).await;
        bridge
            .set_lighting_settings(power_options(false, true))
            .await
            .unwrap();

        assert!(bridge.note_power_event(Trigger::Suspend).await);
        bridge.replay_device_states(Trigger::Suspend).await;
        assert_eq!(last_applied_zone(&driver)["mode"], "Off");

        assert!(
            bridge.note_power_event(Trigger::Resume).await,
            "resume always restores"
        );
        bridge.replay_device_states(Trigger::Resume).await;
        assert_eq!(last_applied_zone(&driver)["mode"], "Static");
    }

    #[tokio::test]
    async fn sleep_with_the_option_off_does_not_turn_anything_off() {
        let (bridge, driver) = viper_with_state(Vec::new()).await;
        bridge
            .set_lighting_settings(power_options(true, false))
            .await
            .unwrap();
        assert!(!bridge.note_power_event(Trigger::Suspend).await);
        assert!(driver.applied_states.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn waking_clears_a_monitor_off_that_never_reported_back() {
        let (bridge, driver) = viper_with_state(Vec::new()).await;
        bridge
            .set_lighting_settings(power_options(true, false))
            .await
            .unwrap();
        bridge.note_power_event(Trigger::DisplayOff).await;

        assert!(bridge.note_power_event(Trigger::Resume).await);
        bridge.replay_device_states(Trigger::Resume).await;

        assert_eq!(last_applied_zone(&driver)["mode"], "Static");
    }

    #[tokio::test]
    async fn verify_holds_the_lights_off_while_the_monitor_is_off() {
        let (bridge, driver) =
            viper_with_state(vec![serde_json::json!({ "zone": "Logo", "mode": "Off" })]).await;
        bridge
            .set_lighting_settings(power_options(true, false))
            .await
            .unwrap();
        bridge.note_power_event(Trigger::DisplayOff).await;

        assert_eq!(
            bridge.replay_device_states(Trigger::Verify).await,
            0,
            "an off device matches the off it should be in, so it is left alone"
        );
        assert!(driver.applied_states.lock().unwrap().is_empty());

        *driver.reading.lock().unwrap() = vec![purple()];
        assert_eq!(
            bridge.replay_device_states(Trigger::Verify).await,
            1,
            "a lit device is put out"
        );
        assert_eq!(last_applied_zone(&driver)["mode"], "Off");
    }

    #[test]
    fn a_zone_turned_off_keeps_its_name_and_loses_its_colour() {
        let state = DeviceState {
            device_id: "Razer:M".into(),
            device_name: "M".into(),
            snapshot: patch(serde_json::json!({
                "lightingZones": [
                    { "zone": "Logo", "mode": "Static", "color": "#fff", "color2": "#000", "speed": 2, "brightness": 40 },
                    { "zone": "Idle", "mode": null }
                ],
                "dpi": 800
            })),
            updated_at: 0,
        };
        let off = desired_state(&state, true);
        assert_eq!(
            off.snapshot["lightingZones"][0],
            serde_json::json!({ "zone": "Logo", "mode": "Off", "brightness": 40 })
        );
        assert_eq!(
            off.snapshot["lightingZones"][1]["mode"],
            Value::Null,
            "a zone with no effect is left alone"
        );
        assert_eq!(off.snapshot["dpi"], 800);
        assert_eq!(desired_state(&state, false), state);
    }

    #[test]
    fn colours_are_normalised_and_bad_ones_refused() {
        assert_eq!(normalize_color("#00FF88").unwrap(), "#00ff88");
        assert_eq!(normalize_color(" 760088 ").unwrap(), "#760088");
        for bad in ["", "#fff", "#gg0000", "#1234567", "red"] {
            assert!(normalize_color(bad).is_err(), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn detection_lists_devices_by_id_with_what_is_stored_and_reuses_a_recent_scan() {
        let bridge = bridge();
        let driver = FakeDriver::new(&["Razer"]);
        *driver.listing.lock().unwrap() = vec![
            listed("Razer Viper Ultimate", 0x7b, &["Logo"]),
            listed("Razer Mouse Dock Chroma", 0x7e, &["Dock"]),
        ];
        bridge.reassert.set_driver(driver.clone());
        bridge
            .merge_device_state(
                "Razer:Razer Viper Ultimate",
                "Razer Viper Ultimate",
                patch(serde_json::json!({ "lightingZones": [purple()] })),
            )
            .await
            .unwrap();

        let devices = bridge.lighting_devices(false).await.unwrap();

        assert_eq!(
            devices
                .iter()
                .map(|device| device.id.as_str())
                .collect::<Vec<_>>(),
            [
                "Razer:Razer Viper Ultimate",
                "Razer:Razer Mouse Dock Chroma"
            ]
        );
        assert_eq!(devices[0].stored.as_ref().unwrap()[0]["color"], "#760088");
        assert!(devices[1].stored.is_none());

        driver.listing.lock().unwrap().clear();
        assert_eq!(
            bridge.lighting_devices(false).await.unwrap().len(),
            2,
            "a recent scan is reused"
        );
        assert!(
            bridge.lighting_devices(true).await.unwrap().is_empty(),
            "a refresh scans again"
        );
    }

    #[tokio::test]
    async fn setting_a_colour_stores_it_on_every_zone_and_writes_it() {
        let bridge = bridge();
        let driver = FakeDriver::new(&["Razer"]);
        *driver.listing.lock().unwrap() =
            vec![listed("Razer Goliathus Extended Chroma", 0xc02, &["Mat"])];
        bridge.reassert.set_driver(driver.clone());

        bridge
            .set_lighting_colour("razer:razer goliathus extended chroma", "00FF00")
            .await
            .unwrap();

        let stored = &bridge.device_states().await[0];
        assert_eq!(stored.device_id, "Razer:Razer Goliathus Extended Chroma");
        assert_eq!(
            stored.snapshot["lightingZones"],
            serde_json::json!([{ "zone": "Mat", "mode": "Static", "color": "#00ff00" }])
        );
        assert_eq!(last_applied_zone(&driver)["color"], "#00ff00");
    }

    #[tokio::test]
    async fn setting_a_colour_refuses_bad_input_without_storing_anything() {
        let bridge = bridge();
        let driver = FakeDriver::new(&["Razer"]);
        *driver.listing.lock().unwrap() = vec![
            listed("Razer Mouse", 1, &["Logo"]),
            listed("Razer Plain Mouse", 2, &[]),
        ];
        bridge.reassert.set_driver(driver.clone());

        for (device, color) in [
            ("Razer:Razer Mouse", "nope"),
            ("Razer:Razer Missing", "#ffffff"),
            ("Razer:Razer Plain Mouse", "#ffffff"),
        ] {
            let error = bridge.set_lighting_colour(device, color).await.unwrap_err();
            assert!(
                error.downcast_ref::<InvalidInput>().is_some(),
                "{device} {color}: {error}"
            );
        }
        assert!(bridge.device_states().await.is_empty());
        assert!(driver.applied_states.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_lighting_settings_are_saved_and_read_back() {
        let bridge = bridge();
        assert_eq!(
            bridge.lighting_settings().await,
            LightingSettings::default()
        );
        bridge
            .set_lighting_settings(LightingSettings {
                mode: crate::config::LightingMode::Own,
                off_when_display_off: true,
                off_on_sleep: true,
            })
            .await
            .unwrap();
        let saved: crate::config::BridgeConfig =
            serde_json::from_slice(&std::fs::read(&*bridge.config_path).unwrap()).unwrap();
        assert!(saved.lighting.off_on_sleep && saved.lighting.off_when_display_off);
        assert_eq!(saved.lighting.mode, crate::config::LightingMode::Own);
    }
}
