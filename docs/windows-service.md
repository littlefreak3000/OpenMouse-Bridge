# Windows service

The tray app runs in a signed-in user's session. Before anyone signs in, after
sign-out, and on setups where lock or sleep disturbs that session, it is not
there to keep device state applied. `openmouse-bridge-service` runs the same
Bridge core (loopback API, native HID, profiles, battery polling) as a
LocalSystem service that starts with Windows.

It adds one thing the tray app does not have: a **device-state keepalive**.

## Device state and replay

Some device state, Razer lighting above all, lives in the device's volatile
memory and cannot be read back, so nothing can tell when it was lost. Instead
Bridge stores what the control panel last set on each device and replays it.

| API | |
| --- | --- |
| `PUT /v1/device-state` | `{ deviceId, deviceName, snapshot }`. Each top-level field of `snapshot` replaces the stored one; `null` removes it. `deviceId` is `<brand>:<name>`, as in profiles. |
| `GET /v1/device-state` | Stored state, whether the watchdog may replay it, and how its last replay went (`lastReplay`: trigger, time, `applied` / `failed` / `noDriver` / `notConnected`, error). |
| `DELETE /v1/device-state/{deviceId}` | Forgets a device's state. |
| `PUT /v1/reassert` | Replays now, even for a device that does not look connected. |

State is stored in `config.json` (`deviceStates`) and replayed through the same
native path as a profile, so the `lighting` and `lightingZones` fields (the
panel's `MouseLighting` objects) reach the driver's `setLighting`.

A replay runs on: service start; a device reappearing on the HID bus; resume
from sleep (from the SCM power event, and from a late tick as a fallback);
session lock, unlock, logon, logoff and console or remote connect (from the SCM
session event); the panel letting go of the device; and a watchdog every
`reassertIntervalSeconds` (default 300, `0` turns it off, minimum 30). Events
replay a few times over the next seconds (a device that just woke can drop the
first write). The watchdog **skips animated lighting**, because re-sending an
effect restarts it. A replay is skipped while the panel holds a HID session
through Bridge, and for a brand with no connected device.

### Learning what vendor software set

`PUT /v1/capture` with `{ "deviceId": "<brand>:<name>" }` reads the effect
**stored on the device** and keeps it as the state to replay. That is the effect
the mouse falls back to when no software is feeding it, which is not always what
you see: vendor software such as Razer Synapse usually paints its colours as a
stream of custom frames (effect `0x08`, visible by reading the live store, byte
`00`, instead of the persistent one, `01`), and those frames cannot be read back.
So capture cannot learn a colour you picked in Synapse. To make the mouse hold
that colour when Synapse stops feeding it (lock, sleep, monitor off), store it
yourself with `PUT /v1/device-state`, using the same colour.

Only a device that reports its own effect can be captured. A Viper Ultimate
does (the extended-matrix effect read, `0x0f`/`0x82`); a model that does not
gets a 400 and nothing is stored, and for those the state is set with
`PUT /v1/device-state`. Capture reads only what the hardware reported, never a
cached guess. Capture while the lighting is the one you want kept: right after a
lock or a sleep, vendor software may have reset it, and that is what would be
stored.

### The lighting client

The service serves a small client at `http://127.0.0.1:17846/`. It lists the
connected Razer devices (found by reading them, which takes a few seconds and is
cached for a minute), shows what each is running, and sets a static colour per
device. It needs no other app and works with or without Synapse.

Two modes, chosen on the page (`GET/PUT /v1/lighting/settings`):

- **Keep Synapse's colours** (default): Synapse keeps setting the colours and the
  service holds the colour you give it as the fallback the device shows when
  Synapse stops feeding it, repairing it if something resets it. Synapse streams
  its colours as custom frames that cannot be read back, so you tell the service
  the same colour you chose in Synapse; it cannot learn it.
- **Use this app instead of Synapse**: the service is the only colour setter.
  The page warns if Synapse is running, since the two fight over the lights.

Two power options, both off by default and applied in either mode:

- **Turn the lights off when the monitor turns off.** The service subscribes to
  the console display state and writes the *off* version of every stored state
  when the monitor goes off, then the stored colours when it comes on. The stored
  state is never changed by this, and the verifier holds the lights at the right
  one of the two, so it does not turn them back on while the monitor is off.
- **Turn the lights off when the PC sleeps.** The service hears the suspend
  notice and writes the off version, then restores on resume. Windows suspends
  shortly after the notice, so a device can occasionally be left lit.

`PUT /v1/lighting/power` with `displayOff`, `displayOn`, `suspend` or `resume`
acts as though Windows had reported that event, to try the options without
waiting; the page's test button uses it.

| API | |
| --- | --- |
| `GET /v1/lighting/devices[?refresh=true]` | Connected devices with zones, current mode and colour, what is stored, and whether Razer software is running. |
| `PUT /v1/lighting/colour` | `{ deviceId, color }`: every zone of the device to one static colour, stored and written now. |
| `GET/PUT /v1/lighting/settings` | `{ mode: "maintain" \| "own", offWhenDisplayOff, offOnSleep }`. |

The page is same-origin: a request whose `Origin` is the loopback host the page
came from is accepted, as before every other origin must be on the allowlist.

### Devices

Lighting is read from and written to the extended-matrix family for the Viper
Ultimate (logo), Mouse Dock Chroma (dock), Goliathus Extended Chroma (mat) and
BlackWidow Elite (keyboard). Each entry comes from read-only probes of a real
device; see `native-hid/src/probe-razer-all.mjs`, which finds the interface that
answers and lists which lighting reads it answers, and is how another model's
entry should be built.

### Verify, then repair

For a device that can report its effect, Bridge does not rewrite it blindly. Every
`verifyIntervalSeconds` (default 60, minimum 15, `0` turns it off) it reads the
device back, compares zone, effect and colour with the stored state (brightness is
left alone, since it is often changed on purpose), and writes only when they
differ. A read does not interrupt vendor software the way a write does, so this
can run far more often than the blind watchdog, and it catches a reset to the
default animation within a minute. Devices the verifier can read are left out of
the watchdog; the watchdog (`reassertIntervalSeconds`) still rewrites devices
that cannot be read back, and never rewrites animated lighting. `lastReplay`
shows `verified` when a check found nothing to fix. A monitor that panics is
logged and restarted.

### Speed and safety of a replay

A replay spawns the Node helper, and probing every Razer interface for the one
that answers took about 9 s on a machine with five Razer devices, which is how
long vendor software's reset stayed visible after a lock. Bridge now remembers
which product id answered for each device and has the helper try it first, which
brought a replay to about 2.4 s. The hint only orders the search, so a wrong or
missing one costs time, never correctness. A replay also writes only to the
device the state names (`exactDevice`): if that device is not the one answering,
it fails rather than writing to another. A trigger that is still replaying is not
queued again, and lock and unlock replay a third time 8 s later, after vendor
software has had its turn.

## Install

Build, then install from an elevated PowerShell:

```powershell
cargo build --release --no-default-features --bin openmouse-bridge-service
(cd native-hid; npm ci)
.\packaging\windows\install-service.ps1
```

`--no-default-features` leaves out the GPUI window code, so the service builds
without it. The script copies the service and the helper (with a bundled
`node.exe`, since SYSTEM has none of your `PATH`) into
`C:\Program Files\OpenMouse Bridge Service`, secures
`%ProgramData%\OpenMouse\Bridge` (SYSTEM and administrators write, everyone
reads), imports your tray app's existing config the first time, registers an
auto-start service with restart-on-failure, and starts it.

`openmouse-bridge-service run` runs it in a console for debugging, and
`OPENMOUSE_BRIDGE_PORT` moves it off 17846 to test beside a running Bridge.
Logs are in `%ProgramData%\OpenMouse\Bridge\logs`.

## Limits

- **Port 17846 is shared.** The tray app and the service cannot both run; the
  second to start finds the port taken. The service has no tray icon, panel,
  notifications or foreground-window detection (Session 0 cannot see your
  desktop). Games are still detected by process; focus-only application
  profiles are not.
- **Vendor software competes.** Razer Synapse and similar tools write their own
  state and hold the control interface. On a machine running Synapse, the
  existing Razer driver cannot even read a device
  (`HidD_SetFeature: Incorrect function`). Quit Synapse, or turn its lighting
  off, for the replay to hold.
- **The API runs as SYSTEM**, with the same loopback and Origin checks as
  before. A local process could already drive the hardware as you; it can now
  do so as SYSTEM, though only through Bridge's fixed helper.
- The service skips the game-catalog refresh at startup so it never waits on
  the network; the catalog cached in `config.json` is used.
