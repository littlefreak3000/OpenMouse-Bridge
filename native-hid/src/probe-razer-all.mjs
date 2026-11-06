#!/usr/bin/env node
// Read-only survey of every connected Razer device: finds the interface that
// answers Razer's control protocol, then asks it which lighting *reads* it
// answers, in both lighting command families (standard `0x03`, extended
// `0x0f`), for a range of led ids and on each transaction id that works.
// Only read commands are sent (command ids with the 0x80 bit); nothing is
// written, so the lighting does not change. The answers are what a new
// device's table entry is built from, instead of a reference table.
//
// Usage: node src/probe-razer-all.mjs [productId hex ...]
globalThis.window ??= globalThis;

import { candidateDevices } from "./hid-device-adapter.mjs";

const razer = await import("@openmouse/protocol/razer");
const { encodeRazerRequest, decodeRazerResponse, RAZER_REPORT_ID } = razer;

const hex = (value) => `0x${value.toString(16).padStart(2, "0")}`;
const wanted = process.argv.slice(2).map((value) => Number.parseInt(value, 16));
const TRANSACTION_IDS = [0x3f, 0x1f, 0xff];
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function exchange(device, command, transactionId) {
  const request = encodeRazerRequest(command, transactionId);
  await device.sendFeatureReport(RAZER_REPORT_ID, request);
  for (let attempt = 0; attempt < 6; attempt += 1) {
    await sleep(60);
    const view = await device.receiveFeatureReport(RAZER_REPORT_ID);
    const reply = new Uint8Array(view.buffer, view.byteOffset, view.byteLength);
    try {
      return decodeRazerResponse(reply, command);
    } catch (error) {
      if (error.status === 0x01) continue; // busy: read again
      throw error;
    }
  }
  throw new Error("stayed busy");
}

const reason = (error) => {
  const status = error.status ?? null;
  if (status === 0x05) return "unsupported";
  if (status === 0x03) return "failed(0x03)";
  if (status === 0x04) return "timeout(0x04)";
  return String(error.message).replace(/^Class .*? command .*? /, "").slice(0, 40);
};

const read = (commandClass, commandId, dataSize, ...args) => ({ commandClass, commandId, dataSize, args });
// RAZER_LEDS=0a,0b adds led ids to try, for a device that echoes an id outside 0-7.
const extraLeds = (process.env.RAZER_LEDS ?? "").split(",").filter(Boolean).map((value) => Number.parseInt(value, 16));
const catalog = [];
for (const led of [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, ...extraLeds]) {
  catalog.push([`ext brightness 0f/84 led ${hex(led)}`, read(0x0f, 0x84, 0x03, 0x01, led)]);
}
for (const led of [0x00, 0x01, 0x04, 0x05, ...extraLeds]) {
  catalog.push([`ext effect     0f/82 led ${hex(led)} store 01`, read(0x0f, 0x82, 0x0c, 0x01, led)]);
  catalog.push([`ext effect     0f/82 led ${hex(led)} store 00`, read(0x0f, 0x82, 0x0c, 0x00, led)]);
}
for (const led of [0x01, 0x04, 0x05]) {
  catalog.push([`std brightness 03/83 led ${hex(led)}`, read(0x03, 0x83, 0x03, 0x01, led)]);
  catalog.push([`std effect     03/82 led ${hex(led)}`, read(0x03, 0x82, 0x03, 0x01, led)]);
  catalog.push([`std rgb        03/81 led ${hex(led)}`, read(0x03, 0x81, 0x05, 0x01, led)]);
}

const byProduct = new Map();
for (const device of candidateDevices(0x1532)) {
  if (wanted.length && !wanted.includes(device.productId)) continue;
  byProduct.set(device.productId, [...(byProduct.get(device.productId) ?? []), device]);
}

for (const [productId, devices] of [...byProduct].sort((left, right) => left[0] - right[0])) {
  console.log(`\n=== product ${hex(productId >> 8)}${hex(productId & 0xff).slice(2)}  ${devices.find((device) => device.productName)?.productName ?? ""}`);
  let control = null;
  for (const device of devices) {
    try {
      await device.open();
    } catch {
      continue;
    }
    const answering = [];
    for (const transactionId of TRANSACTION_IDS) {
      try {
        const firmware = await exchange(device, read(0x00, 0x81, 0x02), transactionId);
        answering.push({ transactionId, firmware: `${firmware[0]}.${firmware[1]}` });
      } catch {
        // not this id on this interface
      }
    }
    if (answering.length > 0) {
      control = { device, answering };
      break;
    }
    await device.close().catch(() => undefined);
  }
  if (!control) {
    console.log("  no interface answered the firmware read on any transaction id");
    continue;
  }
  const { device, answering } = control;
  console.log(`  control interface answers firmware ${answering[0].firmware} on tx ids: ${answering.map((entry) => hex(entry.transactionId)).join(", ")}`);
  const transactionId = answering[0].transactionId;
  const outcomes = new Map();
  for (const [label, command] of catalog) {
    try {
      const reply = await exchange(device, command, transactionId);
      console.log(`  OK   ${label}: ${[...reply.slice(0, 12)].map(hex).join(" ")}`);
    } catch (error) {
      outcomes.set(reason(error), 1 + (outcomes.get(reason(error)) ?? 0));
    }
  }
  console.log(`  other reads: ${[...outcomes].map(([why, count]) => `${count}x ${why}`).join(", ") || "none"}`);
  await device.close().catch(() => undefined);
  for (const other of devices) await other.close().catch(() => undefined);
}
process.exit(0);
