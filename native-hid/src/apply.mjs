#!/usr/bin/env node
// CLI entry point Bridge (Rust) spawns to push a saved profile's DPI/polling
// rate to a mouse over native HID, reusing OpenMouse's own hardware-verified
// WebHID driver classes instead of a bespoke reimplementation.
//
// Usage: node apply.mjs < request.json
//   stdin: {"brand": "Razer", "dpi": 800, "pollingRateHz": 1000}
//   (dpi / pollingRateHz are each optional — omit to leave that setting alone)
//   stdin: {"brand": "Razer", "dpi": 800, "settings": {"lightforceSwitchMode": "Optical"}}
//   (settings carries the panel's stored MouseStatus fields; ones the driver
//   supports are applied, the rest are reported and skipped)
//   stdin: {"brand": "Razer", "action": "status"}
//   reads the mouse's status instead and prints it as JSON on stdout:
//   {"batteryPercent": 41, "batteryState": "Discharging",
//    "deviceName": "G Pro X Superlight 2", "dpi": 800, "pollingRateHz": 1000,
//    "switchMode": "Optical"}  (settings fields null when unreadable)
//
// Exit codes (Bridge's src/drivers/native_hid.rs depends on this contract):
//   0  — a device was found and the requested settings were applied (or, in
//        status mode, its status was read and printed).
//   3  — no native driver is registered for this brand (not an error: most
//        devices are still driven by the OpenMouse web app over WebHID).
//   1  — a driver is registered for this brand, but no matching device
//        answered, or applying the settings failed. Details on stderr.
// mouse-protocol's driver classes were written for a browser and several of
// them (17 of ~18 brands as of this writing) time their HID++/vendor
// request-response waits with `window.setTimeout`/`window.clearTimeout`.
// `window` doesn't exist in Node, so that throws a ReferenceError inside the
// Promise executor — which JS silently turns into an instant rejection, not
// a timeout, before any real reply can arrive. Concretely, this made
// LogitechHidppClient misread that instant non-timeout rejection as "the
// receiver answered but this slot has no sensor" on every pairing slot, so
// it reported a connected mouse as "not a mouse" — nothing to do with HID
// permissions or the mouse being asleep. `window` aliased to `globalThis`
// fixes every driver relying on this pattern at once. Must run before any
// `@openmouse/protocol` driver module is imported, including the dynamic
// imports in probe() below.
globalThis.window ??= globalThis;

import { BRAND_DRIVERS, brandKey } from "./brands.mjs";
import { applyX11PollingRate, x11DeviceInfos } from "./attack-shark-x11.mjs";
import { candidateDevices, probeVerifiedDevice } from "./hid-device-adapter.mjs";
import { devices } from "node-hid";
import { automate } from "./automation.mjs";
import { applySnapshotSettings, capturedLightingZones } from "./snapshot.mjs";
import { selectDevice } from "./select-device.mjs";
import { listLighting } from "./list-lighting.mjs";

const EXIT_APPLIED = 0;
const EXIT_FAILED = 1;
const EXIT_NO_DRIVER = 3;

const PROBE_TIMEOUT_MS = 3000;

async function readStdin() {
  const chunks = [];
  for await (const chunk of process.stdin) chunks.push(chunk);
  return Buffer.concat(chunks).toString("utf8");
}

function withTimeout(promise, ms, label) {
  let timer;
  const timeout = new Promise((_resolve, reject) => {
    timer = setTimeout(() => reject(new Error(`${label} timed out after ${ms}ms`)), ms);
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

/** Tries one candidate class against one open device. Resolves with the
 * live, probed client and the status it read on success; rejects (and closes
 * the device) on any failure, so the caller can move on to the next
 * candidate. */
async function probe(device, candidate) {
  const module = await import(candidate.module);
  const Client = module[candidate.exportName];
  const client = new Client(device);
  try {
    await withTimeout(client.open(), PROBE_TIMEOUT_MS, `${candidate.exportName}.open()`);
    // Every SupportedClient implements readStatus() (mouse-protocol's
    // shared client contract) — a cheap, read-only way to confirm this is
    // really the right class for this interface before writing anything.
    const status = await withTimeout(client.readStatus(), PROBE_TIMEOUT_MS, `${candidate.exportName}.readStatus()`);
    return { client, status };
  } catch (error) {
    await device.close().catch(() => undefined);
    throw error;
  }
}

async function main() {
  const input = JSON.parse(await readStdin());
  const brand = typeof input.brand === "string" ? input.brand : "";
  const normalizedBrand = brandKey(brand);
  const entry = BRAND_DRIVERS[normalizedBrand];
  if (input.action === "automatic-read" || input.action === "automatic-apply") {
    const reply = await automate(input, {
      list: () => devices(),
      probe: (info, identity) => probeVerifiedDevice(info, identity, (device) => probe(device, entry.classes[0])),
    });
    process.stdout.write(JSON.stringify(reply));
    process.exit(EXIT_APPLIED);
  }
  if (!entry) {
    console.error(`[native-hid] no driver registered for brand "${brand}"`);
    process.exit(EXIT_NO_DRIVER);
  }

  // Every connected device of the brand with the lighting zones it reports, so a
  // client can offer them; nothing is written.
  if (input.action === "list-lighting") {
    const devices = await listLighting(entry, { candidates: candidateDevices, probe: (device, candidate) => probe(device, candidate) });
    process.stdout.write(JSON.stringify({ devices }));
    process.exit(EXIT_APPLIED);
  }

  // The Attack Shark X11 family's settings channel is not visible through
  // WebHID, but it is reachable over native HID. Polling-only requests take
  // the verified direct feature-report path below; anything involving DPI
  // falls through to the generic driver path, where AttackSharkHidClient
  // drives the 0x04 six-stage table (enabled for model 0x55 in
  // mouse-protocol #174), preserving the stages the request does not touch.
  if (normalizedBrand === "attack shark" && x11DeviceInfos().length > 0) {
    if (Number.isFinite(input.pollingRateHz) && !Number.isFinite(input.dpi)) {
      applyX11PollingRate(input.pollingRateHz);
      process.exit(EXIT_APPLIED);
    }
  }

  // `device` names the one to write to when a brand has several (a Razer mat
  // answers beside a Razer mouse); without it the first device that answers wins.
  const wantedName = typeof input.device === "string" ? input.device.trim() : "";
  let client = null;
  let status = null;
  let answeredProductId = null;
  try {
    ({ client, status, productId: answeredProductId } = await selectDevice(
      entry,
      wantedName,
      {
        candidates: candidateDevices,
        probe: async (device, candidate) => ({ ...(await probe(device, candidate)), productId: device.productId }),
      },
      {
        // Replaying stored state must not fall back to some other device.
        exact: input.exactDevice === true,
        hintProductId: Number.isInteger(input.hintProductId) ? input.hintProductId : undefined,
      },
    ));
  } catch (error) {
    console.error(`[native-hid] no ${brand} device answered: ${error.message}`);
    process.exit(EXIT_FAILED);
  }

  // What the device is running right now, as the driver read it from the
  // hardware; nothing is written.
  if (input.action === "read-lighting") {
    process.stdout.write(JSON.stringify({ productId: answeredProductId, lightingZones: capturedLightingZones(status) }));
    await client.close().catch(() => undefined);
    process.exit(EXIT_APPLIED);
  }

  if (input.action === "status") {
    process.stdout.write(
      JSON.stringify({
        batteryPercent: status?.batteryPercent ?? null,
        batteryState: status?.batteryState ?? null,
        // The driver's own model name (MouseStatus.name) — better than the
        // receiver-level name Bridge's discovery gets from the HID
        // descriptor (a LIGHTSPEED receiver reports "USB Receiver").
        deviceName: status?.name ?? null,
        // The settings currently in the device's onboard memory, for the
        // tray panel's device summary. MouseStatus reports them whenever
        // the driver can read them.
        dpi: status?.dpi ?? null,
        pollingRateHz: status?.pollingRateHz ?? null,
        switchMode: status?.lightforceSwitchMode ?? null,
      }),
    );
    await client.close().catch(() => undefined);
    process.exit(EXIT_APPLIED);
  }

  // ── Wooting 60HE+ ───────────────────────────────────────────────────
  // RAM-only switch needs no confirm; flash saves require confirmed: true
  // in stdin (headless gate — no modal possible), else exit 1 unsent.
  if (input.action === "switch-profile") {
    if (!Number.isInteger(input.slot)) {
      console.error("[native-hid] switch-profile needs an integer slot.");
      await client.close().catch(() => undefined);
      process.exit(EXIT_FAILED);
    }
    try {
      const slot = await withTimeout(client.switchProfile(input.slot), PROBE_TIMEOUT_MS, "switchProfile");
      process.stdout.write(JSON.stringify({ activeProfile: slot }));
    } catch (error) {
      console.error(`[native-hid] could not switch profile: ${error.message}`);
      await client.close().catch(() => undefined);
      process.exit(EXIT_FAILED);
    }
    await client.close().catch(() => undefined);
    process.exit(EXIT_APPLIED);
  }

  if (typeof input.action === "string" && input.action.startsWith("save-")) {
    if (input.confirmed !== true) {
      console.error(`[native-hid] ${input.action} overwrites onboard flash: pass confirmed: true.`);
      await client.close().catch(() => undefined);
      process.exit(EXIT_FAILED);
    }
    const saveCommands = { "save-rgb": 0x08, "save-keyboard": 0x2a, "save-dks": 0x2f, "save-akc": 0x35 };
    const commandId = saveCommands[input.action];
    if (commandId === undefined || !Number.isInteger(input.slot)) {
      console.error(`[native-hid] unknown save action "${input.action}" (want save-rgb/save-keyboard/save-dks/save-akc + integer slot).`);
      await client.close().catch(() => undefined);
      process.exit(EXIT_FAILED);
    }
    try {
      await withTimeout(client.saveProfile(commandId, input.slot, true), PROBE_TIMEOUT_MS, "saveProfile");
    } catch (error) {
      console.error(`[native-hid] could not save: ${error.message}`);
      await client.close().catch(() => undefined);
      process.exit(EXIT_FAILED);
    }
    await client.close().catch(() => undefined);
    process.exit(EXIT_APPLIED);
  }

  try {
    if (Number.isFinite(input.dpi)) {
      await withTimeout(client.setDpi(input.dpi), PROBE_TIMEOUT_MS, "setDpi");
      if (Number.isFinite(input.pollingRateHz)) {
        await new Promise((resolve) => setTimeout(resolve, 150));
      }
    }
    if (Number.isFinite(input.pollingRateHz)) {
      await withTimeout(client.setPollingRate(input.pollingRateHz), PROBE_TIMEOUT_MS, "setPollingRate");
    }
  } catch (error) {
    console.error(`[native-hid] could not apply settings: ${error.message}`);
    await client.close().catch(() => undefined);
    process.exit(EXIT_FAILED);
  }

  // ── Panel snapshot fields ────────────────────────────────────────────
  // Bridge stores the panel's opaque MouseStatus fields alongside the
  // numeric settings and hands them back here for onboard-memory mode.
  // Only fields with a feature-detected driver method are applied; the
  // rest are reported and skipped.
  if (input.settings && typeof input.settings === "object" && !Array.isArray(input.settings)) {
    let outcome;
    try {
      outcome = await applySnapshotSettings(client, input.settings, (promise, label) =>
        withTimeout(promise, PROBE_TIMEOUT_MS, label));
    } catch (error) {
      console.error(`[native-hid] could not apply settings: ${error.message}`);
      if (error.skipped?.length > 0) {
        console.error(`[native-hid] no native support for: ${error.skipped.join(", ")} — skipped.`);
      }
      await client.close().catch(() => undefined);
      process.exit(EXIT_FAILED);
    }
    if (outcome.skipped.length > 0) {
      console.error(`[native-hid] no native support for: ${outcome.skipped.join(", ")} — skipped.`);
    }
  }

  await client.close().catch(() => undefined);
  // Which product answered, so Bridge can try it first next time.
  process.stdout.write(JSON.stringify({ productId: answeredProductId }));
  process.exit(EXIT_APPLIED);
}

main().catch((error) => {
  console.error(`[native-hid] unexpected error: ${error.stack ?? error.message}`);
  process.exit(EXIT_FAILED);
});
