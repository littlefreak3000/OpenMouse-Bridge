import test from "node:test";
import assert from "node:assert/strict";

import { listLighting } from "./list-lighting.mjs";

const entry = { vendorIds: [0x1532], classes: [{ exportName: "A" }, { exportName: "B" }] };

/** interfaces: [{ productId, answers: status | null, cls? }] */
function fakeIo(interfaces) {
  const closed = [];
  return {
    closed,
    candidates: () => interfaces.map((spec) => ({ productId: spec.productId, productName: "x", spec })),
    probe: async ({ spec }, candidate) => {
      if (!spec.answers || candidate.exportName !== (spec.cls ?? "A")) throw new Error("no answer");
      return { client: { close: async () => closed.push(spec.productId) }, status: spec.answers };
    },
  };
}

const zone = (name, mode, extra = {}) => ({ zone: name, mode, color: "#112233", brightness: 50, writeOnly: false, modes: ["Off", "Static"], ...extra });

test("one entry per product, from the interface that answers, with trimmed zones", async () => {
  const io = fakeIo([
    { productId: 0x007e, answers: null },
    { productId: 0x007e, answers: { name: "Razer Mouse Dock Chroma", firmware: ["Firmware 1.2"], lightingZones: [zone("Dock", "Spectrum")] } },
    { productId: 0x0c02, answers: { name: "Razer Goliathus", firmware: ["Firmware 2.0"], lightingZones: [zone("Mat", "Static")] } },
  ]);

  const devices = await listLighting(entry, io);

  assert.deepEqual(devices.map((device) => [device.productId, device.name]), [[0x007e, "Razer Mouse Dock Chroma"], [0x0c02, "Razer Goliathus"]]);
  assert.deepEqual(devices[0].zones, [{ zone: "Dock", mode: "Spectrum", color: "#112233", brightness: 50, readable: true }]);
  assert.deepEqual(io.closed.sort(), [0x007e, 0x0c02], "every opened device was closed");
});

test("a zone the driver could only cache is marked unreadable", async () => {
  const io = fakeIo([{ productId: 1, answers: { name: "Old mouse", lightingZones: [zone("Logo", null, { writeOnly: true })] } }]);
  const [device] = await listLighting(entry, io);
  assert.equal(device.zones[0].readable, false);
  assert.equal("mode" in device.zones[0], false, "a null mode is left out");
});

test("a device with no lighting is still listed, with no zones", async () => {
  const io = fakeIo([{ productId: 2, answers: { name: "A mouse without lights" } }]);
  assert.deepEqual((await listLighting(entry, io)).map((device) => [device.name, device.zones.length]), [["A mouse without lights", 0]]);
});

test("a device that nothing answers is not listed, and later classes are tried", async () => {
  const io = fakeIo([
    { productId: 3, answers: null },
    { productId: 4, answers: { name: "Second class", lightingZones: [] }, cls: "B" },
  ]);
  assert.deepEqual((await listLighting(entry, io)).map((device) => device.name), ["Second class"]);
});
