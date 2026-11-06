// Lists every connected device of one brand that answers its driver, with the
// lighting zones it reports. This is how a client finds out what can be coloured:
// one entry per product id (a device exposes several HID interfaces, and only one
// answers), each opened, read once and closed. Nothing is written.

const sameZoneFields = ["zone", "mode", "color", "brightness"];

const zoneOf = (zone) => ({
  ...Object.fromEntries(sameZoneFields.filter((field) => zone[field] !== undefined && zone[field] !== null).map((field) => [field, zone[field]])),
  // Whether the mode and colour were read from the hardware or are a guess.
  readable: zone.writeOnly === false,
});

/**
 * @param entry  the brand's BRAND_DRIVERS entry ({ vendorIds, classes })
 * @param io     { candidates(vendorId) -> device[], probe(device, candidate) -> {client, status} }
 * @returns {Promise<Array<{productId: number, name: string, firmware: string[], zones: object[]}>>}
 */
export async function listLighting(entry, io) {
  const byProduct = new Map();
  for (const vendorId of entry.vendorIds) {
    for (const device of io.candidates(vendorId)) {
      byProduct.set(device.productId, [...(byProduct.get(device.productId) ?? []), device]);
    }
  }
  const found = [];
  for (const [productId, devices] of [...byProduct].sort((left, right) => left[0] - right[0])) {
    let answered = null;
    for (const device of devices) {
      for (const candidate of entry.classes) {
        try {
          const probed = await io.probe(device, candidate);
          const status = probed.status ?? {};
          const zones = Array.isArray(status.lightingZones) ? status.lightingZones : status.lighting ? [status.lighting] : [];
          answered = { productId, name: String(status.name ?? ""), firmware: status.firmware ?? [], zones: zones.map(zoneOf) };
          await probed.client.close().catch(() => undefined);
          break;
        } catch {
          // This class does not drive this interface; try the next.
        }
      }
      if (answered) break;
    }
    if (answered) found.push(answered);
  }
  return found;
}
