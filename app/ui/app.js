// The page's half of the app API (docs/specs/desktop-app.md, section 9). It renders
// what the app core says and sends commands; it holds no logic of its own.
//
// Page to core: {id, cmd, args} through window.ipc.postMessage. Core to page:
// window.oli.receive(message), where message is a reply {id, ok} or
// {id, error: {message, action}}, or an event {event, data}.

const pending = new Map();
let nextId = 1;

// Send a command; resolves with the reply's `ok`, rejects with its `error`.
function call(cmd, args) {
  const id = nextId++;
  const message = args === undefined ? { id, cmd } : { id, cmd, args };
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject });
    window.ipc.postMessage(JSON.stringify(message));
  });
}

// Everything the core sends arrives here.
function receive(message) {
  if (typeof message !== "object" || message === null) {
    return;
  }
  if (typeof message.event === "string") {
    document.dispatchEvent(new CustomEvent("oli:" + message.event, { detail: message.data }));
    return;
  }
  const waiting = pending.get(message.id);
  if (waiting === undefined) {
    return;
  }
  pending.delete(message.id);
  if ("error" in message) {
    waiting.reject(message.error);
  } else {
    waiting.resolve(message.ok);
  }
}

window.oli = Object.freeze({ receive });

function showProblem(error) {
  const problem = document.getElementById("problem");
  problem.textContent = error && error.message ? error.message : String(error);
  problem.hidden = false;
}

function byId(id) {
  return document.getElementById(id);
}

// ---------------------------------------------------------------------------
// 1. Where to send
// ---------------------------------------------------------------------------

let changing = false;

// Render step 1 from the core's view of it. Text only, never markup.
function renderWhere(where) {
  const done = where.ready && !changing;
  byId("where-summary").hidden = !done;
  byId("where-summary-text").textContent = where.summary || "";
  byId("where-choose").hidden = done;

  const problem = byId("where-problem");
  problem.textContent = where.problem || "";
  problem.hidden = !where.problem;

  const signedIn = where.mode === "osc" && where.signed_in;
  byId("osc-sign-in").hidden = signedIn;
  byId("osc-workspace").hidden = !signedIn;
  byId("osc-workspace-name").textContent = where.workspace || "(looking it up)";
  const list = byId("osc-instances");
  list.replaceChildren();
  for (const instance of where.instances) {
    const label = document.createElement("label");
    const radio = document.createElement("input");
    radio.type = "radio";
    radio.name = "instance";
    radio.value = instance.url;
    radio.checked = instance.url === where.url;
    radio.addEventListener("change", () => chooseInstance(instance.url));
    label.append(radio, " " + instance.name + " ");
    const address = document.createElement("small");
    address.textContent = instance.url;
    label.append(address);
    list.append(label);
  }
  byId("osc-instances-box").hidden = where.instances.length === 0;

  if (where.mode === "self_hosted" && where.url && !byId("self-hosted-url").value) {
    byId("self-hosted-url").value = where.url;
  }

  // A step below an unfinished one is visible but disabled (spec, section 5).
  const what = byId("what");
  what.classList.toggle("disabled", !where.ready);
  what.setAttribute("aria-disabled", String(!where.ready));
  setGoEnabled(where.ready);
  whereReady = where.ready;
  syncPreview();
}

function clearProblem() {
  byId("problem").hidden = true;
}

// Run a step 1 command with its button disabled; render what comes back.
async function whereCommand(button, cmd, args) {
  clearProblem();
  if (button) {
    button.disabled = true;
  }
  try {
    const result = await call(cmd, args);
    // Sign-in commands answer with step 1's view, the others with the whole model.
    const where = result.where || result;
    if (where.ready) {
      changing = false;
      loadRelays();
    }
    renderWhere(where);
    return true;
  } catch (error) {
    showProblem(error);
    return false;
  } finally {
    if (button) {
      button.disabled = false;
    }
    byId("osc-waiting").hidden = true;
  }
}

function chooseInstance(url) {
  return whereCommand(null, "choose_instance", { url });
}

function wireWhere() {
  byId("where-change").addEventListener("click", async () => {
    changing = true;
    const state = await call("state");
    renderWhere(state.where);
  });
  byId("osc-sign-in").addEventListener("click", (event) =>
    whereCommand(event.currentTarget, "osc_sign_in"),
  );
  byId("osc-change-workspace").addEventListener("click", (event) =>
    whereCommand(event.currentTarget, "osc_workspaces"),
  );
  byId("self-hosted-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const key = byId("self-hosted-key");
    const ok = await whereCommand(event.submitter, "self_hosted", {
      url: byId("self-hosted-url").value,
      api_key: key.value,
    });
    if (ok) {
      key.value = "";
    }
  });
  byId("paste-token-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const token = byId("paste-token-value");
    const ok = await whereCommand(event.submitter, "osc_paste_token", { token: token.value });
    if (ok) {
      token.value = "";
    }
  });
  // The sign-in page is open in the browser; its address is shown too.
  document.addEventListener("oli:osc_sign_in_page", (event) => {
    byId("osc-address").textContent = event.detail.url;
    byId("osc-waiting").hidden = false;
  });
  // The workspace and instance names, looked up after the window opened.
  document.addEventListener("oli:where", (event) => renderWhere(event.detail));
}

// ---------------------------------------------------------------------------
// 2. What to send
// ---------------------------------------------------------------------------

// Spec, section 5. The core builds the rows; the page draws them and
// sends back the ticked ones on every change, which the core saves as [[input]].

let whatToSend = null;
// Audio and label chosen on a row that is not ticked yet: nothing to save until it is.
const drafts = new Map();

function element(tag, properties) {
  return Object.assign(document.createElement(tag), properties);
}

function audioPicker(row, audio) {
  const select = element("select", { className: "audio", disabled: row.test_pattern });
  select.append(element("option", { value: "", textContent: "None" }));
  for (const source of audio) {
    const option = element("option", { value: source.device, textContent: source.name });
    // One audio source goes with one camera: taken by another ticked row, it is shown
    // but cannot be picked here.
    option.disabled = source.paired_with !== null && source.paired_with !== row.device;
    select.append(option);
  }
  // A saved pairing with a source that is not connected now stays chosen, or the next
  // save would drop it.
  if (row.audio !== null && !audio.some((source) => source.device === row.audio)) {
    const saved = element("option", { value: row.audio, disabled: true });
    saved.textContent = row.audio + " (not connected)";
    select.append(saved);
  }
  select.value = row.audio === null ? "" : row.audio;
  return select;
}

function inputRow(row, model) {
  const draft = row.ticked ? undefined : drafts.get(row.device);
  const tr = element("tr", { className: "input-row" });
  tr.dataset.device = row.device;

  const tick = element("input", { type: "checkbox", className: "tick", checked: row.ticked });
  tick.setAttribute("aria-label", "Send " + row.name);

  const name = element("td", { textContent: row.name });
  const note = !row.connected ? "Not connected" : row.virtual ? "Virtual" : "";
  if (note !== "") {
    name.append(element("span", { className: "note", textContent: note }));
  }

  const audio = audioPicker(row, model.audio);
  if (draft !== undefined) {
    audio.value = draft.audio;
  }
  // Only the label is the person's: the prefix is how cleanup finds this gateway's sources.
  const label = element("input", {
    type: "text",
    className: "label",
    value: draft !== undefined ? draft.label : row.label,
  });
  label.setAttribute("aria-label", "Studio name after " + model.name_prefix);
  const studioName = element("span", { className: "studio-name" });
  studioName.append(element("span", { textContent: model.name_prefix }), label);
  const studio = element("td");
  studio.append(studioName);

  const send = element("td");
  send.append(tick);
  const pick = element("td");
  pick.append(audio);
  tr.append(send, name, pick, studio);
  for (const control of [tick, audio, label]) {
    control.addEventListener("change", () => changed(tr));
  }
  return tr;
}

function renderWhat(model) {
  whatToSend = model;
  document.getElementById("show-all").checked = model.show_all;
  const notice = document.getElementById("what-notice");
  notice.textContent = model.notice === null ? "" : model.notice;
  notice.hidden = model.notice === null;
  const body = document.querySelector("#inputs tbody");
  body.replaceChildren(...model.video.flatMap((row) => inputRows(row, model)));
}

// A row, and under a ticked one its picture and level bars (spec, section 5.2).
function inputRows(row, model) {
  const tr = inputRow(row, model);
  if (!row.picture) {
    return [tr];
  }
  const preview = element("tr", { className: "preview-row" });
  const cell = element("td", { colSpan: 3 });
  cell.append(previewBox(row, false));
  preview.append(element("td"), cell);
  return [tr, preview];
}

function rowValues(tr) {
  return {
    device: tr.dataset.device,
    ticked: tr.querySelector(".tick").checked,
    audio: tr.querySelector(".audio").value,
    label: tr.querySelector(".label").value,
  };
}

async function changed(tr) {
  const values = rowValues(tr);
  if (!values.ticked) {
    drafts.set(values.device, { audio: values.audio, label: values.label });
  } else {
    drafts.delete(values.device);
  }
  const inputs = [];
  for (const row of document.querySelectorAll("#inputs tbody tr.input-row")) {
    const v = rowValues(row);
    if (v.ticked) {
      inputs.push({ device: v.device, audio: v.audio === "" ? null : v.audio, label: v.label });
    }
  }
  try {
    renderWhat(await call("set_inputs", { inputs }));
    clearProblem();
  } catch (error) {
    showProblem(error);
    // Back to what is saved.
    if (whatToSend !== null) {
      renderWhat(whatToSend);
    }
  }
}

async function listDevices(showAll) {
  renderWhat(await call("devices", { show_all: showAll }));
  clearProblem();
}

// ---------------------------------------------------------------------------
// The preview: picture and sound, in steps 2 and 3 (spec, section 5.2)
// ---------------------------------------------------------------------------

// The core runs a preview flow per ticked row while the page asks for it: while step
// 2 is shown (step 1 done, not live) and the window is visible. It sends a `preview`
// event per row at each change of its picture, keyed by the row's `device`, one
// `level` event for all rows four times a second, and the rows' notices. The page
// keeps the latest of each and draws them; it sends nothing but `preview {on}`.

// The row's picture: {state, image, message, action}, by device.
const pictures = new Map();
// The row's latest levels: {peak_db: [], rms_db: []}, one value per channel, by device.
let levels = new Map();
// The row's notices (no_sound, wrong_camera): kind to notice, by device.
const rowNotices = new Map();
// Window notices about the preview as a whole: shown in step 2.
const PREVIEW_NOTICES = new Set(["preview_unavailable", "preview_off_other_gateway"]);
const previewNotices = new Map();
// The bars run from -60 to 0 dBFS.
const LEVEL_FLOOR_DB = -60;

let whereReady = false;
let devicesListed = false;
let previewOn = false;

function previewText() {
  return (whatToSend && whatToSend.preview_text) || {
    no_microphone: "",
    camera_off: "",
    camera_off_states: [],
  };
}

// Step 2 is shown: step 1 is done, nothing is live, and the window can be seen.
function previewWanted() {
  return devicesListed && whereReady && !live && document.visibilityState === "visible";
}

// Tells the core when that changes. Not when going live: Go live hands the cameras
// over itself, and the core turns the preview off at the end of the session.
function syncPreview() {
  const on = previewWanted();
  if (on === previewOn) {
    return;
  }
  previewOn = on;
  call("preview", { on }).catch((error) => {
    previewOn = false;
    showProblem(error);
  });
}

// The picture and level bars of one row. `cameraOff`: a step 3 row that is Armed or
// Waiting for production, whose camera is off.
function previewBox(row, cameraOff) {
  const box = element("div", { className: "preview" });
  box.dataset.preview = row.device;
  box.dataset.sound = String(row.sound);
  box.dataset.cameraOff = String(cameraOff);
  const picture = element("div", { className: "picture" });
  picture.append(element("img", { alt: "", hidden: true }));
  picture.append(element("div", { className: "picture-caption" }));
  const side = element("div", { className: "preview-side" });
  side.append(element("div", { className: "meters" }));
  side.append(element("div", { className: "row-notices" }));
  box.append(picture, side);
  drawPicture(box);
  drawLevels(box);
  drawRowNotices(box);
  return box;
}

// Every box that shows `device`'s row: in step 2, and in step 3 while live.
function previewBoxes(device) {
  return [...document.querySelectorAll(".preview")].filter(
    (box) => box.dataset.preview === device,
  );
}

const PICTURE_CAPTION = { starting: "Starting…" };

function drawPicture(box) {
  const picture = box.querySelector(".picture");
  const image = picture.querySelector("img");
  const caption = picture.querySelector(".picture-caption");
  caption.replaceChildren();
  if (box.dataset.cameraOff === "true") {
    image.hidden = true;
    picture.classList.remove("dim");
    caption.textContent = previewText().camera_off;
    return;
  }
  const shown = pictures.get(box.dataset.preview) || { state: null, image: null };
  // No picture: the last one, dimmed, with the line saying so.
  const withImage =
    shown.image !== null && (shown.state === "showing" || shown.state === "no_picture");
  if (withImage && image.getAttribute("src") !== shown.image) {
    image.src = shown.image;
  }
  image.hidden = !withImage;
  picture.classList.toggle("dim", shown.state === "no_picture");
  caption.textContent = shown.message || PICTURE_CAPTION[shown.state] || "";
  // A camera that could not be opened is ticked again by hand, as its line says;
  // `retry` is Go live's action, so it gets no button here.
  const button = shown.action === "retry" ? null : actionButton(shown.action);
  if (button) {
    caption.append(button);
  }
}

// The share of the bar a level fills, -60 dBFS (empty) to 0 dBFS (full).
function levelPercent(db) {
  if (typeof db !== "number") {
    return 0;
  }
  return Math.max(0, Math.min(100, ((db - LEVEL_FLOOR_DB) / -LEVEL_FLOOR_DB) * 100));
}

function drawLevels(box) {
  const meters = box.querySelector(".meters");
  if (box.dataset.cameraOff === "true") {
    meters.replaceChildren();
    return;
  }
  if (box.dataset.sound !== "true") {
    if (!meters.querySelector(".no-microphone")) {
      meters.replaceChildren(
        element("span", { className: "no-microphone", textContent: previewText().no_microphone }),
      );
    }
    return;
  }
  const level = levels.get(box.dataset.preview);
  const peaks = level ? level.peak_db : [];
  const channels = Math.max(1, peaks.length);
  if (meters.children.length !== channels) {
    const bars = [];
    for (let i = 0; i < channels; i++) {
      const bar = element("div", { className: "meter" });
      bar.append(element("div", { className: "peak" }), element("div", { className: "rms" }));
      bars.push(bar);
    }
    meters.replaceChildren(...bars);
  }
  [...meters.children].forEach((bar, i) => {
    bar.querySelector(".peak").style.width = levelPercent(peaks[i]) + "%";
    bar.querySelector(".rms").style.width = levelPercent(level ? level.rms_db[i] : null) + "%";
  });
}

function drawRowNotices(box) {
  const lines = box.querySelector(".row-notices");
  lines.replaceChildren();
  const forRow = rowNotices.get(box.dataset.preview);
  if (!forRow) {
    return;
  }
  for (const notice of forRow.values()) {
    const line = element("p", { className: "row-notice", textContent: notice.message });
    const button = actionButton(notice.action);
    if (button) {
      line.append(" ", button);
    }
    lines.append(line);
  }
}

// A notice for one row (it names a `device`) is drawn on that row.
function rowNotice(notice) {
  const forRow = rowNotices.get(notice.device) || new Map();
  if (notice.clear) {
    forRow.delete(notice.clear);
  } else {
    forRow.set(notice.kind, notice);
  }
  rowNotices.set(notice.device, forRow);
  for (const box of previewBoxes(notice.device)) {
    drawRowNotices(box);
  }
}

function wirePreview() {
  document.addEventListener("oli:preview", (event) => {
    const data = event.detail;
    const old = pictures.get(data.device);
    // `image` is null when the picture did not change: keep the last one. A row
    // that stopped, or whose camera went off, starts again without it.
    const keep = data.state === "showing" || data.state === "no_picture";
    const image = data.image !== null ? data.image : keep && old ? old.image : null;
    pictures.set(data.device, {
      state: data.state,
      image,
      message: data.message || null,
      action: data.action || null,
    });
    for (const box of previewBoxes(data.device)) {
      drawPicture(box);
    }
  });
  document.addEventListener("oli:level", (event) => {
    levels = new Map(event.detail.levels.map((level) => [level.device, level]));
    for (const box of document.querySelectorAll(".preview")) {
      drawLevels(box);
    }
  });
  // Minimised or hidden: the preview stops, and starts again when seen.
  document.addEventListener("visibilitychange", () => {
    syncPreview();
  });
}

// ---------------------------------------------------------------------------
// 3. Go live / Stop
// ---------------------------------------------------------------------------

const STATE_TEXT = {
  registering: "Registering",
  armed: "Armed",
  waiting_for_production: "Waiting for production",
  connecting: "Connecting",
  live: "Live",
  failed: "Failed",
  stopping: "Stopping",
  stopped: "Stopped",
  not_connected: "Not connected",
};

let live = false;
const rows = new Map();
const notices = new Map();

function setGoEnabled(ready) {
  const go = byId("go");
  const enabled = ready || live;
  go.classList.toggle("disabled", !enabled);
  go.setAttribute("aria-disabled", String(!enabled));
  byId("go-button").disabled = !enabled;
}

// Live: the button stops, and the rest of the window is read-only.
function setLive(on) {
  live = on;
  document.body.classList.toggle("live", on);
  byId("go-button").textContent = on ? "Stop" : "Go live";
  if (on) {
    // Go live hands the cameras over itself; the core asks for nothing after it.
    previewOn = false;
  } else {
    syncPreview();
  }
}

// What each action does (spec, section 10). Actions with no command yet show no button.
const ACTIONS = {
  retry: { label: "Go live", run: () => goLive() },
  stop_it: { label: "Stop it", run: () => stopOther() },
  sign_in: { label: "Sign in", run: () => byId("where-change").click() },
  change: { label: "Change", run: () => byId("where-change").click() },
  open_studio: { label: "Open Studio", run: () => call("open_url", { which: "studio" }) },
  open_settings: { label: "Open Settings", run: () => call("open_url", { which: "privacy" }) },
  open_microphone_settings: {
    label: "Open Settings",
    run: () => call("open_url", { which: "privacy_microphone" }),
  },
  help: { label: "Help", run: () => call("open_url", { which: "help" }) },
  advanced: { label: "Advanced", run: () => openAdvanced() },
  show_logs: { label: "Show logs", run: () => call("show_logs", { which: "app" }) },
  show_strom_log: { label: "Show logs", run: () => call("show_logs", { which: "strom" }) },
};

function actionButton(action) {
  const known = action && ACTIONS[action];
  if (!known) {
    return null;
  }
  const button = document.createElement("button");
  button.type = "button";
  button.className = "link";
  button.textContent = known.label;
  button.addEventListener("click", () => Promise.resolve(known.run()).catch(showGoProblem));
  return button;
}

function showGoProblem(error) {
  const text = byId("go-problem-text");
  text.textContent = error && error.message ? error.message : String(error);
  const slot = byId("go-problem-action");
  const button = actionButton(error && error.action);
  slot.hidden = !button;
  if (button) {
    slot.replaceWith(button);
    button.id = "go-problem-action";
  }
  byId("go-problem").hidden = false;
}

function clearGoProblem() {
  byId("go-problem").hidden = true;
}

// The status lines under a row (spec, section 5): the productions holding the input's
// source, their state and the slot, and the link. `productions` is null when unknown.
function productionLine(productions) {
  if (productions === null || productions === undefined) {
    return "Production: unknown";
  }
  if (productions.length === 0) {
    return "Production: none holds this source";
  }
  return (
    "Production: " +
    productions
      .map(
        (p) =>
          p.name +
          " (" +
          p.state +
          "), " +
          (p.slots.length === 1 ? "slot " : "slots ") +
          p.slots.join(", "),
      )
      .join("; ")
  );
}

const LINK_TEXT = {
  not_sending: "not sending",
  connecting: "connecting",
  connected: "connected",
  unknown: "unknown",
};

function linkLine(link) {
  let text = "Link: " + (LINK_TEXT[link.state] || link.state);
  if (typeof link.dropped_pct === "number") {
    text += ", " + link.dropped_pct.toFixed(1) + " % dropped";
  }
  if (typeof link.rtt_ms === "number") {
    text += ", RTT " + Math.round(link.rtt_ms) + " ms";
  }
  return text;
}

function statusLine(text) {
  const line = document.createElement("span");
  line.className = "status-line";
  line.textContent = text;
  return line;
}

function renderRows() {
  const list = byId("go-rows");
  list.replaceChildren();
  for (const row of rows.values()) {
    const item = document.createElement("li");
    item.append(row.input);
    const state = document.createElement("span");
    state.className = "state state-" + row.state;
    state.textContent = STATE_TEXT[row.state] || row.state;
    item.append(state);
    if (typeof row.bitrate_kbps === "number" && row.state === "live") {
      item.append(" " + (row.bitrate_kbps / 1000).toFixed(1) + " Mbps");
    }
    // Where the uplink goes (the caller address, or through Open Live's SRT relay
    // the dial-in address), and the address registered with Open Live as secondary
    // text. Passphrases arrive masked.
    if (row.uplink) {
      const address = document.createElement("span");
      address.className = "detail address selectable";
      address.textContent = row.relay
        ? "Sending via relay " + row.relay + " to " + row.uplink
        : "Sending to " + row.uplink;
      item.append(address);
      if (row.registered) {
        const registered = document.createElement("span");
        registered.className = "detail address selectable";
        registered.textContent = "Registered in Open Live as " + row.registered;
        item.append(registered);
      }
    }
    if (row.message) {
      const detail = document.createElement("span");
      detail.className = "detail";
      detail.textContent = row.message;
      const button = actionButton(row.action);
      if (button) {
        detail.append(" ", button);
      }
      item.append(detail);
    }
    if (row.status && live) {
      item.append(statusLine(productionLine(row.status.productions)));
      item.append(statusLine(linkLine(row.status.link)));
    }
    const preview = livePreview(row);
    if (preview) {
      item.append(preview);
    }
    list.append(item);
  }
}

// The step 3 row's picture and level bars, while live: from the uplink once it runs
// (Connecting, Live); "Camera off until a production goes live" while Armed or
// Waiting for production. The row is found by its Studio name.
function livePreview(row) {
  if (!live || whatToSend === null) {
    return null;
  }
  const page = whatToSend.video.find((r) => r.ticked && r.input === row.input);
  if (!page) {
    return null;
  }
  const cameraOff = previewText().camera_off_states.includes(row.state);
  if (!cameraOff && row.state !== "connecting" && row.state !== "live") {
    return null;
  }
  return previewBox(page, cameraOff);
}

function renderNotices() {
  renderNoticeBox(byId("go-notices"), notices);
  renderNoticeBox(byId("what-notices"), previewNotices);
}

function renderNoticeBox(box, shown) {
  box.replaceChildren();
  for (const notice of shown.values()) {
    const line = document.createElement("p");
    line.className = "notice";
    line.textContent = notice.message;
    const button = actionButton(notice.action);
    if (button) {
      line.append(" ", button);
    }
    box.append(line);
  }
}

async function goLive() {
  clearGoProblem();
  for (const id of ["go-uplink", "go-passphrase"]) {
    const line = byId(id);
    line.textContent = "";
    line.hidden = true;
  }
  rows.clear();
  notices.clear();
  renderRows();
  renderNotices();
  setLive(true);
  try {
    await call("go_live");
  } catch (error) {
    setLive(false);
    showGoProblem(error);
  }
}

async function stopOther() {
  clearGoProblem();
  await call("stop_other");
  await goLive();
}

function wireGoLive() {
  byId("go-button").addEventListener("click", async () => {
    if (live) {
      byId("go-button").disabled = true;
      await call("stop").catch(showGoProblem);
    } else {
      await goLive();
    }
  });
  document.addEventListener("oli:strom", (event) => {
    const words = { starting: "Starting Strom…", ready: "", stopped: "" };
    byId("go-strom").textContent = words[event.detail.state] || "";
  });
  // Through Open Live's SRT relay (OL-66): which one, and the passphrase made for it.
  // Each has its own line: the library says `passphrase` before `uplink`, so one
  // shared line would lose the passphrase text when the relay line is written.
  document.addEventListener("oli:uplink", (event) => {
    const uplink = byId("go-uplink");
    uplink.textContent =
      "Sending via relay " +
      event.detail.relay +
      (event.detail.public ? " at " + event.detail.public : "") +
      ".";
    uplink.hidden = false;
  });
  document.addEventListener("oli:passphrase", (event) => {
    if (event.detail.kept) {
      const line = byId("go-passphrase");
      line.textContent = "A passphrase for the relay was made and saved in the settings.";
      line.hidden = false;
    }
  });
  document.addEventListener("oli:input", (event) => {
    const old = rows.get(event.detail.input) || {};
    rows.set(event.detail.input, { ...old, ...event.detail });
    renderRows();
  });
  document.addEventListener("oli:summary", (event) => {
    for (const started of event.detail.inputs) {
      const old = rows.get(started.input) || { input: started.input };
      rows.set(started.input, {
        ...old,
        uplink: started.uplink,
        registered: started.registered,
        relay: started.relay,
      });
    }
    renderRows();
  });
  document.addEventListener("oli:stats", (event) => {
    for (const stats of event.detail.inputs) {
      const row = rows.get(stats.input);
      if (row) {
        row.bitrate_kbps = stats.bitrate_kbps;
      }
    }
    renderRows();
  });
  document.addEventListener("oli:status", (event) => {
    for (const status of event.detail.inputs) {
      const row = rows.get(status.input);
      if (row) {
        row.status = status;
      }
    }
    renderRows();
  });
  document.addEventListener("oli:notice", (event) => {
    const notice = event.detail;
    if (typeof notice.device === "string") {
      rowNotice(notice);
      return;
    }
    const kind = notice.clear || notice.kind;
    const shown = PREVIEW_NOTICES.has(kind) ? previewNotices : notices;
    if (notice.clear) {
      shown.delete(notice.clear);
    } else {
      shown.set(notice.kind, notice);
    }
    renderNotices();
  });
  document.addEventListener("oli:stopping", (event) => {
    byId("go-strom").textContent =
      "Stopping… " + event.detail.done + " of " + event.detail.total;
  });
  document.addEventListener("oli:stopped", (event) => {
    setLive(false);
    byId("go-button").disabled = false;
    byId("go-strom").textContent = "";
    renderRows();
    notices.clear();
    renderNotices();
    if (event.detail.error) {
      showGoProblem(event.detail.error);
    } else if (event.detail.left_behind.length > 0) {
      showGoProblem({
        message:
          "Still in Studio, held by an active production: " +
          event.detail.left_behind.join(", ") +
          ". Deactivate it in Studio; the next Stop, or open-live-ingest down, removes them.",
        action: "open_studio",
      });
    }
  });
}

// ---------------------------------------------------------------------------
// Advanced: the SRT relay (OL-66)
// ---------------------------------------------------------------------------

// Go live picks the relay by itself. The picker is offered only when Open Live
// reports more than one, or no longer has the one saved; the core decides.
// Opening it asks Open Live again (the toggle handler in wireAdvanced): the relays
// may have changed since the window opened, and a Go live that names an unknown
// relay offers this button. A stale list would keep the picker hidden, or offer
// the names that are gone. Already open: no toggle event, so ask here.
async function openAdvanced() {
  const advanced = byId("advanced");
  const wasOpen = advanced.open;
  advanced.open = true;
  advanced.scrollIntoView();
  if (wasOpen) {
    await loadRelays();
  }
}

async function loadRelays() {
  let relays;
  try {
    relays = await call("relays");
  } catch (error) {
    // Open Live not reachable now: no picker. Go live says what is wrong.
    byId("relay-picker").hidden = true;
    return;
  }
  const select = byId("relay-select");
  select.replaceChildren();
  const names = relays.relays.includes(relays.chosen)
    ? relays.relays
    : [relays.chosen, ...relays.relays];
  for (const name of names) {
    const option = document.createElement("option");
    option.value = name;
    option.textContent = relays.relays.includes(name) ? name : name + " (not in Open Live)";
    option.selected = name === relays.chosen;
    select.append(option);
  }
  byId("relay-picker").hidden = !relays.offer;
}

function wireAdvanced() {
  byId("advanced").addEventListener("toggle", (event) => {
    if (event.target.open) {
      loadRelays();
    }
  });
  byId("relay-select").addEventListener("change", (event) => {
    if (live) {
      return;
    }
    clearProblem();
    call("set_advanced", { uplink: { relay: event.target.value } })
      .then(() => loadRelays())
      .catch(showProblem);
  });
}

// ---------------------------------------------------------------------------
// Quitting while live (spec, section 8)
// ---------------------------------------------------------------------------

// The core decides; the page only asks. Stop and quit runs the Stop teardown and the
// app quits when it is done; Quit anyway is offered when it takes long.
let quitting = false;

function quitQuestion(inputs) {
  if (inputs === 0) {
    return "You are going live. Stop and quit?";
  }
  return "You are live with " + inputs + (inputs === 1 ? " input" : " inputs") + ". Stop and quit?";
}

function askToQuit(inputs) {
  const dialog = byId("quit-dialog");
  if (!quitting) {
    byId("quit-text").textContent = quitQuestion(inputs);
  }
  if (!dialog.open) {
    dialog.showModal();
  }
}

function resetQuit() {
  quitting = false;
  byId("quit-stop").disabled = false;
  byId("quit-cancel").disabled = false;
  byId("quit-anyway").hidden = true;
  byId("quit-progress").hidden = true;
}

function wireQuit() {
  const dialog = byId("quit-dialog");
  document.addEventListener("oli:quit_requested", (event) => askToQuit(event.detail.inputs_live));
  byId("quit-cancel").addEventListener("click", () => dialog.close());
  // Escape closes the question, but not once Stop and quit has begun.
  dialog.addEventListener("cancel", (event) => {
    if (quitting) {
      event.preventDefault();
    }
  });
  byId("quit-stop").addEventListener("click", async () => {
    quitting = true;
    byId("quit-stop").disabled = true;
    byId("quit-cancel").disabled = true;
    byId("quit-progress").textContent = "Stopping…";
    byId("quit-progress").hidden = false;
    try {
      await call("quit", { confirmed: true });
    } catch (error) {
      dialog.close();
      resetQuit();
      showGoProblem(error);
    }
  });
  byId("quit-anyway").addEventListener("click", () => {
    call("quit", { confirmed: true, anyway: true }).catch((error) => {
      byId("quit-progress").textContent = error && error.message ? error.message : String(error);
    });
  });
  document.addEventListener("oli:quit_anyway", () => {
    byId("quit-anyway").hidden = false;
  });
  document.addEventListener("oli:stopping", (event) => {
    if (quitting) {
      byId("quit-progress").textContent =
        "Stopping… " + event.detail.done + " of " + event.detail.total;
    }
  });
}

// ---------------------------------------------------------------------------
// Logs
// ---------------------------------------------------------------------------

function showLog(which) {
  clearProblem();
  call("show_logs", { which }).catch(showProblem);
}

function wireLogs() {
  // macOS has Help > Show logs in the menu bar; Windows has no menu bar, so the
  // link is in the footer there (spec, section 5).
  const showLogs = byId("show-logs");
  showLogs.hidden = !/Windows/.test(navigator.userAgent);
  showLogs.addEventListener("click", () => showLog("app"));
  byId("show-strom-log").addEventListener("click", () => showLog("strom"));
}

// ---------------------------------------------------------------------------
// A newer release
// ---------------------------------------------------------------------------

// The core's update check found a newer release: one quiet line in the footer. The
// link is opened by name; the core builds its address.
function showUpdate(version) {
  if (typeof version !== "string" || version === "") {
    return;
  }
  byId("update-text").textContent = "Version " + version + " is available.";
  byId("update").hidden = false;
}

function wireUpdate() {
  document.addEventListener("oli:update", (event) => {
    showUpdate(event.detail && event.detail.version);
  });
  byId("update-open").addEventListener("click", () => {
    clearProblem();
    call("open_url", { which: "release" }).catch(showProblem);
  });
}

async function start() {
  wireWhere();
  wirePreview();
  wireGoLive();
  wireQuit();
  wireLogs();
  wireUpdate();
  wireAdvanced();
  const state = await call("state");
  document.getElementById("version").textContent = "Version " + state.version;
  showUpdate(state.update);
  setLive(state.live);
  renderWhere(state.where);
  if (state.where.ready) {
    loadRelays();
  }
  byId("show-all").addEventListener("change", (event) => {
    listDevices(event.target.checked).catch(showProblem);
  });
  try {
    await listDevices(false);
  } finally {
    // The first device list holds the local Strom; the preview runs on it.
    devicesListed = true;
    syncPreview();
  }
}

start().catch(showProblem);
