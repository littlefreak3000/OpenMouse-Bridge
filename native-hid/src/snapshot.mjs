// Applies the panel's stored MouseStatus fields to a live driver client.
//
// Bridge keeps a profile's (or a device's last) settings as the control panel's
// opaque `Partial<MouseStatus>` map. Only fields with a feature-detected driver
// method are written; the rest are reported back so the caller can say what was
// skipped. DPI and polling rate travel beside the snapshot, not in it, so they
// are accepted here without being treated as unknown.

const HANDLED_ELSEWHERE = new Set(["dpi", "pollingRateHz"]);
const KNOWN_FIELDS = new Set(["lightforceSwitchMode", "lighting", "lightingZones", ...HANDLED_ELSEWHERE]);

/** The lighting zones a snapshot carries, whole-mouse effects before per-LED
 * ones: a whole-mouse effect overrides every individual cell, so the cells
 * have to go on afterwards. Zones with no effect chosen are left out. */
export function lightingZonesOf(settings) {
  const zones = Array.isArray(settings.lightingZones)
    ? settings.lightingZones
    : settings.lighting && typeof settings.lighting === "object"
      ? [settings.lighting]
      : [];
  const usable = zones.filter((zone) => zone && typeof zone === "object" && typeof zone.mode === "string" && zone.mode !== "");
  return [...usable.filter((zone) => zone.hardwareZoneId === undefined), ...usable.filter((zone) => zone.hardwareZoneId !== undefined)];
}

/**
 * @param client   a probed driver client (mouse-protocol SupportedClient)
 * @param settings the stored snapshot
 * @param run      `(promise, label) => promise`, the caller's timeout wrapper
 * @returns {{applied: string[], skipped: string[]}}
 * @throws when a field the driver supports could not be written; every field is
 *         still attempted first, so one bad zone does not hide the rest
 */
export async function applySnapshotSettings(client, settings, run) {
  const applied = [];
  const skipped = [];
  const failures = [];

  if (typeof settings.lightforceSwitchMode === "string") {
    if (typeof client.setLightforceSwitchMode === "function") {
      try {
        await run(client.setLightforceSwitchMode(settings.lightforceSwitchMode), "setLightforceSwitchMode");
        applied.push("lightforceSwitchMode");
      } catch (error) {
        failures.push(`lightforceSwitchMode: ${error.message}`);
      }
    } else {
      skipped.push("lightforceSwitchMode");
    }
  }

  const zones = lightingZonesOf(settings);
  if (zones.length > 0) {
    if (typeof client.setLighting !== "function") {
      skipped.push("lighting");
    } else {
      for (const zone of zones) {
        const label = `${zone.zone ?? "lighting"} ${zone.mode}`;
        try {
          await run(client.setLighting(zone), `setLighting(${label})`);
          applied.push(`lighting:${label}`);
        } catch (error) {
          failures.push(`lighting ${label}: ${error.message}`);
        }
      }
    }
  }

  for (const [field, value] of Object.entries(settings)) {
    if (!KNOWN_FIELDS.has(field) && value !== null && value !== undefined) skipped.push(field);
  }

  if (failures.length > 0) {
    const error = new Error(failures.join("; "));
    error.applied = applied;
    error.skipped = skipped;
    throw error;
  }
  return { applied, skipped };
}

const CAPTURED_FIELDS = ["zone", "mode", "color", "color2", "speed", "brightness", "hardwareZoneId"];

/**
 * The lighting a device itself reported, trimmed to what a replay needs.
 * Only zones the driver read from the hardware (`writeOnly === false`) with an
 * effect chosen are kept: a write-only zone only holds what this process last
 * wrote, which is nothing in a fresh helper, so capturing it would store a guess.
 */
export function capturedLightingZones(status) {
  const zones = Array.isArray(status?.lightingZones) ? status.lightingZones : status?.lighting ? [status.lighting] : [];
  return zones
    .filter((zone) => zone && typeof zone === "object" && zone.writeOnly === false && typeof zone.mode === "string" && zone.mode !== "")
    .map((zone) => Object.fromEntries(CAPTURED_FIELDS.filter((field) => zone[field] !== undefined && zone[field] !== null).map((field) => [field, zone[field]])));
}
