// OpenTune desktop UI. Talks to the Rust core through Tauri commands.
// With `app.withGlobalTauri: true` in tauri.conf.json, `invoke` is available
// on the global object, so no bundler/npm install is needed to run.
const invoke = window.__TAURI__?.core?.invoke;

const driverSel = document.getElementById("driver");
const connectBtn = document.getElementById("connect");
const identifyBtn = document.getElementById("identify");
const readBtn = document.getElementById("read");
const logEl = document.getElementById("log");

function log(msg) {
  logEl.textContent += `\n${msg}`;
}

async function call(cmd, args) {
  if (!invoke) {
    log(`(stub) ${cmd} — run inside the Tauri app to call the core`);
    return null;
  }
  try {
    return await invoke(cmd, args);
  } catch (e) {
    log(`error: ${e}`);
    throw e;
  }
}

// Populate the driver list from the core's registry.
async function loadDrivers() {
  const drivers = (await call("list_drivers")) ?? [
    { id: "mock", display_name: "Mock / file driver" },
  ];
  driverSel.innerHTML = "";
  for (const d of drivers) {
    const opt = document.createElement("option");
    opt.value = d.id;
    opt.textContent = `${d.display_name} (${d.id})`;
    driverSel.appendChild(opt);
  }
}

connectBtn.addEventListener("click", async () => {
  const id = driverSel.value;
  log(`connecting via "${id}"…`);
  await call("connect", { driver: id });
  log("connected.");
  identifyBtn.disabled = false;
  readBtn.disabled = false;
});

identifyBtn.addEventListener("click", async () => {
  log("identifying…");
  const ecu = await call("identify");
  log(`ecu: ${JSON.stringify(ecu, null, 2)}`);
});

readBtn.addEventListener("click", async () => {
  // Demo region; a real app picks this from the selected ECU definition.
  log("reading region 0x0 len 0x100…");
  const bytes = await call("read_region", { address: 0, length: 0x100 });
  log(`read ${bytes?.length ?? 0} bytes.`);
});

loadDrivers().catch((e) => log(`failed to load drivers: ${e}`));
