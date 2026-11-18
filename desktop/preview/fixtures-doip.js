// Demo fixture for the sidebar's DoIP Discover button (doip_discover). Two
// fictional DoIP entities on the RFC 5737 documentation network, placeholder
// VINs and locally administered EIDs: nothing here is a real vehicle.
"use strict";
(function () {
  const F = window.__PREVIEW_FIXTURES__;
  if (!F) return;
  const wait = (ms) => new Promise((r) => setTimeout(r, ms));
  const ENTITIES = [
    { ip: "192.0.2.10", udp_port: 13400, vin: "WVWZZZ00000000000", logical_address: "0x0010", eid: "02:00:00:00:00:10", gid: "02:00:00:00:00:10", further_action: "no further action required" },
    { ip: "192.0.2.11", udp_port: 13400, vin: "WVWZZZ00000000000", logical_address: "0x0020", eid: "02:00:00:00:00:20", gid: "02:00:00:00:00:10", further_action: "no further action required" },
  ].map((e) => Object.assign(e, { spec: `${e.ip}?target=${e.logical_address}` }));

  F.doip_discover = async ({ port }) => {
    await wait(400);
    const host = String(port || "").split("?")[0].split(":")[0].trim();
    const dest = host && host !== "discover" ? `${host}:13400` : "255.255.255.255:13400";
    const entities = host && host !== "discover" ? ENTITIES.filter((e) => e.ip === host) : ENTITIES;
    const log = window.__PREVIEW_LOG__;
    if (log) log(`doip discover ${dest}: ${entities.length} entit${entities.length === 1 ? "y" : "ies"} (demo)`);
    return { dest, entities, nacks: [], duplicates: 0, rejected: 0 };
  };
})();
