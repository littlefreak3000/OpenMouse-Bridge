#!/usr/bin/env node
// Read-only diagnostic: lists every Razer HID interface and what each of the
// existing driver classes makes of it. Opens each interface, reads its status
// (firmware, battery, DPI, lighting capability), and closes it. It sends no
// write command, so it is safe beside a running device; it is what to run first
// when a Razer device will not answer, and to learn what a model reports before
// any lighting support is added for it.
//
// Usage: node src/probe-razer.mjs
globalThis.window ??= globalThis;

import { BRAND_DRIVERS } from "./brands.mjs";
import { candidateDevices } from "./hid-device-adapter.mjs";

const TIMEOUT_MS = 3000;

function withTimeout(promise, label) {
  let timer;
  const timeout = new Promise((_resolve, reject) => {
    timer = setTimeout(() => reject(new Error(`${label} timed out`)), TIMEOUT_MS);
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

const summarize = (status) => ({
  name: status.name,
  firmware: status.firmware,
  battery: status.batteryPercent ?? null,
  dpi: status.dpi ?? null,
  pollingRateHz: status.pollingRateHz ?? null,
  lightingZones: (status.lightingZones ?? (status.lighting ? [status.lighting] : [])).map((zone) => ({
    zone: zone.zone,
    modes: zone.modes,
  })),
});

const results = [];
for (const device of candidateDevices(0x1532)) {
  const label = device.productName || "unknown device";
  for (const candidate of BRAND_DRIVERS.razer.classes) {
    let client;
    try {
      const module = await import(candidate.module);
      client = new module[candidate.exportName](device);
      await withTimeout(client.open(), "open");
      const status = await withTimeout(client.readStatus(), "readStatus");
      results.push({ device: label, class: candidate.exportName, ok: true, status: summarize(status) });
      break;
    } catch (error) {
      results.push({ device: label, class: candidate.exportName, ok: false, error: String(error.message).split("\n")[0].slice(0, 160) });
    } finally {
      await client?.close?.().catch(() => undefined);
      await device.close().catch(() => undefined);
    }
  }
}
console.log(JSON.stringify(results, null, 2));
process.exit(0);
