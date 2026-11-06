import test from "node:test";
import assert from "node:assert/strict";

import { candidateDevices } from "./hid-device-adapter.mjs";

// One Razer-shaped device: the control report lives on the interface-0 mouse
// collection, beside a keyboard collection and a vendor-page one.
const info = (interfaceNumber, usagePage, usage, path) => ({
  vendorId: 0x1532, productId: 0x007b, interface: interfaceNumber, usagePage, usage, path,
});
const list = () => [info(0, 1, 2, "mouse0"), info(2, 1, 6, "kbd2"), info(1, 0xff00, 1, "vendor1")];

test("off macOS the mouse and keyboard collections stay candidates", () => {
  const adapters = candidateDevices(0x1532, { platform: "win32", list });
  assert.equal(adapters.length, 3, "one logical device per interface, none skipped");
});

test("on macOS they are skipped, as before", () => {
  const adapters = candidateDevices(0x1532, { platform: "darwin", list });
  assert.equal(adapters.length, 1, "only the vendor-page interface remains");
});

test("other vendors are never candidates", () => {
  assert.equal(candidateDevices(0x046d, { platform: "win32", list }).length, 0);
});
