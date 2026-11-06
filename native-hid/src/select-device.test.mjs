import test from "node:test";
import assert from "node:assert/strict";

import { selectDevice } from "./select-device.mjs";

const entry = { vendorIds: [0x1532], classes: [{ exportName: "ClassA" }, { exportName: "ClassB" }] };

/** devices: [{ label, answers: name | null, failsWith }]; records which clients were closed. */
function fakeIo(devices) {
  const closed = [];
  return {
    closed,
    candidates: () => devices.map((device) => ({ productName: device.label, device })),
    probe: async ({ device }, candidate) => {
      if (device.answers === null || candidate.exportName !== (device.cls ?? "ClassA")) {
        throw new Error(device.failsWith ?? "did not answer");
      }
      return { client: { name: device.answers, close: async () => closed.push(device.answers) }, status: { name: device.answers } };
    },
  };
}

test("without a name the first device that answers is used, and nothing else is probed", async () => {
  const io = fakeIo([{ label: "mat", answers: "Goliathus" }, { label: "mouse", answers: "Viper Ultimate" }]);
  const found = await selectDevice(entry, "", io);
  assert.equal(found.status.name, "Goliathus");
  assert.deepEqual(io.closed, []);
});

test("a name picks the matching device among several, ignoring case and spacing, and closes the others", async () => {
  const io = fakeIo([{ label: "mat", answers: "Goliathus" }, { label: "mouse", answers: "Razer Viper Ultimate" }]);
  const found = await selectDevice(entry, "  razer viper ULTIMATE ", io);
  assert.equal(found.status.name, "Razer Viper Ultimate");
  assert.deepEqual(io.closed, ["Goliathus"]);
});

test("a lone answering device is used whatever its name", async () => {
  const io = fakeIo([{ label: "dead", answers: null }, { label: "mouse", answers: "Viper Ultimate" }]);
  const found = await selectDevice(entry, "Something Else", io);
  assert.equal(found.status.name, "Viper Ultimate");
});

test("several answering devices and no match is an error that names them, not a guess", async () => {
  const io = fakeIo([{ label: "mat", answers: "Goliathus" }, { label: "mouse", answers: "Viper Ultimate" }]);
  await assert.rejects(selectDevice(entry, "Basilisk", io), /none of the 2 connected devices \(Goliathus, Viper Ultimate\) is named "Basilisk"/);
  assert.deepEqual(io.closed.sort(), ["Goliathus", "Viper Ultimate"], "nothing is left open");
});

test("later classes are tried on the same device when the first does not fit", async () => {
  const io = fakeIo([{ label: "mouse", answers: "Cobra", cls: "ClassB" }]);
  const found = await selectDevice(entry, "Cobra", io);
  assert.equal(found.status.name, "Cobra");
});

test("with nothing answering the error carries every attempt", async () => {
  const io = fakeIo([{ label: "mat", answers: null, failsWith: "Incorrect function" }]);
  await assert.rejects(selectDevice(entry, "", io), /no device answered[\s\S]*Incorrect function/);
});

test("exact refuses a lone device that is not the one named, instead of writing to it", async () => {
  const io = fakeIo([{ label: "dead", answers: null }, { label: "mat", answers: "Goliathus" }]);
  await assert.rejects(selectDevice(entry, "Razer Viper Ultimate", io, { exact: true }), /the one connected device \(Goliathus\) is not named "Razer Viper Ultimate"/);
  assert.deepEqual(io.closed, ["Goliathus"], "the refused device is closed");
});

test("exact still accepts the device that is named", async () => {
  const io = fakeIo([{ label: "mat", answers: "Goliathus" }, { label: "mouse", answers: "Razer Viper Ultimate" }]);
  const found = await selectDevice(entry, "Razer Viper Ultimate", io, { exact: true });
  assert.equal(found.status.name, "Razer Viper Ultimate");
});

test("the hinted product id is probed first, so the usual case never waits on the others", async () => {
  const order = [];
  const io = {
    candidates: () => [
      { productId: 0x0228, productName: "keyboard" },
      { productId: 0x0c02, productName: "mat" },
      { productId: 0x007b, productName: "viper" },
    ],
    probe: async (device) => {
      order.push(device.productName);
      return { client: { close: async () => {} }, status: { name: device.productName } };
    },
  };
  const found = await selectDevice(entry, "viper", io, { hintProductId: 0x007b });
  assert.equal(found.status.name, "viper");
  assert.deepEqual(order, ["viper"], "nothing else was probed");
});

test("a wrong hint only changes the order: the device is still found", async () => {
  const order = [];
  const io = {
    candidates: () => [{ productId: 1, productName: "mat" }, { productId: 2, productName: "viper" }],
    probe: async (device) => {
      order.push(device.productName);
      return { client: { close: async () => {} }, status: { name: device.productName } };
    },
  };
  const found = await selectDevice(entry, "viper", io, { hintProductId: 99 });
  assert.equal(found.status.name, "viper");
  assert.deepEqual(order, ["mat", "viper"]);
});
