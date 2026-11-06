// Picks which of a brand's connected devices a request is for.
//
// A request names a brand, and optionally a device (the part of a profile's
// `<brand>:<name>` id after the colon). With no name the first interface whose
// driver answers is used, as before. With one, every answering interface is
// probed and the one whose reported name matches wins.
//
// Two options tighten and speed that up for replaying stored state:
//  - `exact`: the name must match. Without it a lone answering device is used
//    whatever it calls itself, so a differently spelled name never leaves a
//    mouse unreachable; replaying must never write to a device it was not
//    asked about, so it refuses instead.
//  - `hintProductId`: the product id that answered last time. Its interfaces
//    are tried first, so the usual case never waits on the others (a Razer
//    desk answers one interface in about a second and fails the rest slowly).
//    It only orders the search; nothing is skipped.

const sameName = (left, right) => String(left ?? "").trim().toLowerCase() === String(right ?? "").trim().toLowerCase();

/**
 * @param entry       the brand's BRAND_DRIVERS entry ({ vendorIds, classes })
 * @param wantedName  the device name from the request, or ""
 * @param io          { candidates(vendorId) -> device[], probe(device, candidate) -> {client, status} }
 * @param options     { exact?: boolean, hintProductId?: number }
 * @returns {Promise<{client, status, attempts: string[]}>}
 */
export async function selectDevice(entry, wantedName, io, { exact = false, hintProductId } = {}) {
  const attempts = [];
  const others = [];
  const closeOthers = () => Promise.all(others.map((other) => other.client.close().catch(() => undefined)));
  for (const vendorId of entry.vendorIds) {
    const hinted = (device) => (hintProductId !== undefined && device.productId === hintProductId ? 0 : 1);
    // Array.prototype.sort is stable, so enumeration order is kept within each group.
    const devices = [...io.candidates(vendorId)].sort((left, right) => hinted(left) - hinted(right));
    for (const device of devices) {
      for (const candidate of entry.classes) {
        try {
          const found = await io.probe(device, candidate);
          if (!wantedName) return { ...found, attempts };
          if (sameName(found.status?.name, wantedName)) {
            await closeOthers();
            return { ...found, attempts };
          }
          others.push(found);
          break;
        } catch (error) {
          attempts.push(`${candidate.exportName} on ${device.productName || "unknown device"}: ${error.message}`);
        }
      }
    }
  }
  if (others.length === 1 && !exact) return { ...others[0], attempts };
  await closeOthers();
  if (others.length > 0) {
    const names = others.map((other) => other.status?.name ?? "unnamed").join(", ");
    throw new Error(
      others.length === 1
        ? `the one connected device (${names}) is not named "${wantedName}"`
        : `none of the ${others.length} connected devices (${names}) is named "${wantedName}"`,
    );
  }
  throw new Error(attempts.length ? `no device answered.\n  ${attempts.join("\n  ")}` : "no matching HID interface was found.");
}
