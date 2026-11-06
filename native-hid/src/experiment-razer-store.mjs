#!/usr/bin/env node
// Experiment (writes!): sets one led of one Razer device to a static colour in a
// chosen *store* (the first argument byte of the effect command: 01 persistent,
// 00 live), reads both stores before and after, holds, then puts the effect back
// to spectrum in both stores. It answers one question the effect read alone
// cannot: which store the visible light follows on a given device. Run it only
// with the owner watching the device.
//
// Usage: node src/experiment-razer-store.mjs <productId hex> <led hex> <store hex|both> <#rrggbb> <holdSeconds>
globalThis.window ??= globalThis;

import { BRAND_DRIVERS } from "./brands.mjs";
import { candidateDevices } from "./hid-device-adapter.mjs";

const [pidText, ledText, storeText, color, holdText] = process.argv.slice(2);
const productId = Number.parseInt(pidText ?? "", 16);
const led = Number.parseInt(ledText ?? "", 16);
const hold = Number.parseInt(holdText ?? "8", 10);
if (!Number.isInteger(productId) || !Number.isInteger(led) || !/^#[0-9a-f]{6}$/i.test(color ?? "") || !storeText) {
  console.error("usage: node src/experiment-razer-store.mjs <productId hex> <led hex> <store hex|both> <#rrggbb> <holdSeconds>");
  process.exit(2);
}
const stores = storeText === "both" ? [0x01, 0x00] : [Number.parseInt(storeText, 16)];
const hex = (value) => `0x${value.toString(16).padStart(2, "0")}`;
const razer = await import("@openmouse/protocol/razer");

async function run(client) {
  const read = async (store) => {
    try {
      const reply = await client.request({ ...razer.razerReadExtendedEffectCommand(led), args: [store, led] });
      return [...reply.slice(0, 9)].map(hex).join(" ");
    } catch (error) {
      return String(error.message).split("\n")[0];
    }
  };
  const write = async (store, effect, options) => {
    const command = razer.razerSetExtendedEffectCommand(effect, { ...options, led });
    command.args[0] = store;
    await client.request(command);
  };
  const show = async (label) => console.log(`${label.padEnd(26)} persistent(01): ${await read(0x01)}   live(00): ${await read(0x00)}`);

  await show("before");
  for (const store of stores) await write(store, "static", { color });
  await show(`after static ${color} -> ${stores.map(hex).join("+")}`);
  // Poll both stores while holding: if something else rewrites the effect, this
  // shows when, and what it wrote.
  console.log(`holding ${hold} s, polling once a second ...`);
  const started = Date.now();
  let last = "";
  while (Date.now() - started < hold * 1000) {
    const now = `persistent(01): ${await read(0x01)}   live(00): ${await read(0x00)}`;
    if (now !== last) console.log(`  t+${((Date.now() - started) / 1000).toFixed(1)}s  ${now}`);
    last = now;
    await new Promise((resolve) => setTimeout(resolve, 600));
  }
  // Put it back in both stores, whichever was written.
  for (const store of [0x01, 0x00]) await write(store, "spectrum", {});
  await show("restored (spectrum, both)");
}

for (const device of candidateDevices(0x1532).filter((candidate) => candidate.productId === productId)) {
  for (const candidate of BRAND_DRIVERS.razer.classes) {
    let client;
    try {
      const module = await import(candidate.module);
      client = new module[candidate.exportName](device);
      await client.open();
      await client.readStatus();
      await run(client);
      process.exit(0);
    } catch {
      // not the control interface for this class; try the next
    } finally {
      await client?.close?.().catch(() => undefined);
      await device.close().catch(() => undefined);
    }
  }
}
console.error("no interface answered");
process.exit(1);
