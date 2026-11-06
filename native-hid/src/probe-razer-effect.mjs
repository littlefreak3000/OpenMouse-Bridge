#!/usr/bin/env node
// Read-only: asks one Razer mouse whether it will report the lighting effect it
// is running right now, by sending the extended-matrix "get effect" read
// (class 0x0f, id 0x82) for one led. Whether a model answers decides whether a
// service can learn the colour vendor software set, or has to be told it.
// Nothing is written. A model that does not know the command answers status
// 0x05 ("not supported"), which is reported, not an error.
//
// Usage: node src/probe-razer-effect.mjs <productId hex> [led hex, default 04]
globalThis.window ??= globalThis;

import { BRAND_DRIVERS } from "./brands.mjs";
import { candidateDevices } from "./hid-device-adapter.mjs";

const productId = Number.parseInt(process.argv[2] ?? "", 16);
const led = Number.parseInt(process.argv[3] ?? "04", 16);
if (!Number.isInteger(productId)) {
  console.error("usage: node src/probe-razer-effect.mjs <productId hex> [led hex]");
  process.exit(2);
}
const hex = (value) => `0x${value.toString(16).padStart(2, "0")}`;

for (const device of candidateDevices(0x1532).filter((candidate) => candidate.productId === productId)) {
  for (const candidate of BRAND_DRIVERS.razer.classes) {
    let client;
    try {
      const module = await import(candidate.module);
      client = new module[candidate.exportName](device);
      await client.open();
      const status = await client.readStatus();
      console.log(`answering interface: ${status.name} via ${candidate.exportName}`);
      // `--watch <seconds>` repeats the read about once a second, to see whether
      // something else (vendor software) changes the effect while it runs.
      const watchIndex = process.argv.indexOf("--watch");
      const seconds = watchIndex > 0 ? Number.parseInt(process.argv[watchIndex + 1] ?? "10", 10) : 0;
      const readOnce = async () => {
        try {
          const storeIndex = process.argv.indexOf("--store");
          const store = storeIndex > 0 ? Number.parseInt(process.argv[storeIndex + 1] ?? "01", 16) : 0x01;
          const reply = await client.request({ commandClass: 0x0f, commandId: 0x82, dataSize: 0x0c, args: [store, led] });
          return `answered, ${reply.length} bytes: ${[...reply].map(hex).join(" ")}`;
        } catch (error) {
          return String(error.message).split("\n")[0];
        }
      };
      const started = Date.now();
      do {
        console.log(`${String(Math.round((Date.now() - started) / 100) / 10).padStart(5)}s  get effect, led ${hex(led)}: ${await readOnce()}`);
        if (seconds) await new Promise((resolve) => setTimeout(resolve, 700));
      } while (seconds && Date.now() - started < seconds * 1000);
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
