#!/usr/bin/env node
// Read-only: asks one Razer mouse which lighting reads it answers, so a lighting
// entry for a new model is built from what the device says rather than from a
// reference table. Sends brightness *reads* only (extended-matrix 0x0f/0x84 for
// a range of led ids, and the standard backlight read 0x03/0x83) on the product's
// transaction id; nothing is written, so the lighting does not change.
//
// Usage: node src/probe-razer-leds.mjs <productId hex, e.g. 007b>
globalThis.window ??= globalThis;

import { BRAND_DRIVERS } from "./brands.mjs";
import { candidateDevices } from "./hid-device-adapter.mjs";

const productId = Number.parseInt(process.argv[2] ?? "", 16);
if (!Number.isInteger(productId)) {
  console.error("usage: node src/probe-razer-leds.mjs <productId hex>");
  process.exit(2);
}

const { razerReadExtendedBrightnessCommand, RAZER_BACKLIGHT_BRIGHTNESS_READ } = await import("@openmouse/protocol/razer");
const hex = (value) => `0x${value.toString(16).padStart(2, "0")}`;

for (const device of candidateDevices(0x1532).filter((candidate) => candidate.productId === productId)) {
  for (const candidate of BRAND_DRIVERS.razer.classes) {
    let client;
    try {
      const module = await import(candidate.module);
      client = new module[candidate.exportName](device);
      await client.open();
      const status = await client.readStatus();
      console.log(`answering interface: ${status.name} via ${candidate.exportName}, firmware ${status.firmware?.join(" ")}`);
      const queries = [
        ...[0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07].map((led) => ({
          label: `extended brightness, led ${hex(led)}`,
          command: razerReadExtendedBrightnessCommand(led),
        })),
        { label: "standard backlight brightness (led 0x05)", command: RAZER_BACKLIGHT_BRIGHTNESS_READ },
      ];
      for (const transactionId of [undefined, 0x3f, 0x1f, 0xff]) {
        console.log(`-- transaction id ${transactionId === undefined ? "(product default)" : hex(transactionId)}`);
        for (const { label, command } of queries) {
          try {
            const reply = await client.request(command, transactionId);
            console.log(`   ${label}: ok, args ${[...reply.slice(0, 4)].map(hex).join(" ")}  brightness ${Math.round((reply[2] * 100) / 255)}%`);
          } catch (error) {
            console.log(`   ${label}: ${String(error.message).split("\n")[0].slice(0, 90)}`);
          }
        }
      }
      process.exit(0);
    } catch {
      // Not the control interface for this class; try the next.
    } finally {
      await client?.close?.().catch(() => undefined);
      await device.close().catch(() => undefined);
    }
  }
}
console.error(`no interface answered for product ${hex(productId)}`);
process.exit(1);
