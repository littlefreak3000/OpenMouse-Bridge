import test from "node:test";
import assert from "node:assert/strict";

import { applySnapshotSettings, lightingZonesOf } from "./snapshot.mjs";

const run = (promise) => promise;
const zone = (name, mode, extra = {}) => ({ zone: name, mode, color: "#ff0000", ...extra });

function fakeClient({ failOn } = {}) {
  const calls = [];
  return {
    calls,
    async setLighting(lighting) {
      calls.push(["setLighting", lighting.zone, lighting.mode]);
      if (failOn === lighting.zone) throw new Error("the mouse said no");
    },
    async setLightforceSwitchMode(mode) {
      calls.push(["setLightforceSwitchMode", mode]);
    },
  };
}

test("a single stored zone is written", async () => {
  const client = fakeClient();
  const outcome = await applySnapshotSettings(client, { lighting: zone("Logo", "Static") }, run);
  assert.deepEqual(client.calls, [["setLighting", "Logo", "Static"]]);
  assert.deepEqual(outcome.applied, ["lighting:Logo Static"]);
  assert.deepEqual(outcome.skipped, []);
});

test("lightingZones wins over lighting, and zones with no effect chosen are left alone", async () => {
  const client = fakeClient();
  await applySnapshotSettings(
    client,
    {
      lighting: zone("Ignored", "Static"),
      lightingZones: [zone("Logo", "Static"), zone("Scroll", null), zone("Side", "")],
    },
    run,
  );
  assert.deepEqual(client.calls, [["setLighting", "Logo", "Static"]]);
});

test("whole-mouse effects go on before per-LED cells so the cells are not overridden", () => {
  const zones = lightingZonesOf({
    lightingZones: [zone("LED 0", "Static", { hardwareZoneId: 0 }), zone("Logo", "Static"), zone("LED 1", "Off", { hardwareZoneId: 1 })],
  });
  assert.deepEqual(zones.map((entry) => entry.zone), ["Logo", "LED 0", "LED 1"]);
});

test("one failing zone does not stop the others, and the error names it", async () => {
  const client = fakeClient({ failOn: "Logo" });
  await assert.rejects(
    applySnapshotSettings(client, { lightingZones: [zone("Logo", "Static"), zone("Scroll", "Static")] }, run),
    (error) => {
      assert.match(error.message, /Logo Static: the mouse said no/);
      assert.deepEqual(error.applied, ["lighting:Scroll Static"]);
      return true;
    },
  );
  assert.deepEqual(client.calls.map((call) => call[1]), ["Logo", "Scroll"]);
});

test("a driver without lighting reports it as skipped instead of failing", async () => {
  const outcome = await applySnapshotSettings({}, { lighting: zone("Logo", "Static"), lightforceSwitchMode: "Optical" }, run);
  assert.deepEqual(outcome.applied, []);
  assert.deepEqual(outcome.skipped, ["lightforceSwitchMode", "lighting"]);
});

test("dpi and pollingRate travel beside the snapshot and are not reported as unknown", async () => {
  const outcome = await applySnapshotSettings(fakeClient(), { dpi: 800, pollingRateHz: 1000, mystery: 1, gone: null }, run);
  assert.deepEqual(outcome.skipped, ["mystery"]);
});

test("the timeout wrapper is applied to every write", async () => {
  const labels = [];
  const client = fakeClient();
  await applySnapshotSettings(client, { lightforceSwitchMode: "Optical", lighting: zone("Logo", "Static") }, (promise, label) => {
    labels.push(label);
    return promise;
  });
  assert.deepEqual(labels, ["setLightforceSwitchMode", "setLighting(Logo Static)"]);
});

import { capturedLightingZones } from "./snapshot.mjs";

test("capture keeps only zones the device reported, trimmed to what a replay needs", () => {
  const status = {
    lightingZones: [
      { zone: "Logo", mode: "Static", color: "#ff0000", color2: null, speed: null, brightness: 33, writeOnly: false, modes: ["Off", "Static"], brightnessLevels: [25, 50] },
      { zone: "Scroll", mode: "Static", color: "#00ff00", writeOnly: true },
      { zone: "Side", mode: null, writeOnly: false },
    ],
  };
  assert.deepEqual(capturedLightingZones(status), [{ zone: "Logo", mode: "Static", color: "#ff0000", brightness: 33 }]);
});

test("capture of a device with no lighting, or only write-only lighting, is empty", () => {
  assert.deepEqual(capturedLightingZones({}), []);
  assert.deepEqual(capturedLightingZones({ lighting: { zone: "Mouse", mode: "Static", color: "#fff", writeOnly: true } }), []);
  assert.deepEqual(capturedLightingZones(undefined), []);
});
