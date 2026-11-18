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
  const tr = element("tr");
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
  body.replaceChildren(...model.video.map((row) => inputRow(row, model)));
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
  for (const row of document.querySelectorAll("#inputs tbody tr")) {
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
// 3. Go live / Stop
// ---------------------------------------------------------------------------

const STATE_TEXT = {
  registering: "Registering",
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
}

// What each action does (spec, section 10). Actions with no command yet show no button.
const ACTIONS = {
  retry: { label: "Go live", run: () => goLive() },
  stop_it: { label: "Stop it", run: () => stopOther() },
  sign_in: { label: "Sign in", run: () => byId("where-change").click() },
  change: { label: "Change", run: () => byId("where-change").click() },
  open_studio: { label: "Open Studio", run: () => call("open_url", { which: "studio" }) },
  open_settings: { label: "Open Settings", run: () => call("open_url", { which: "privacy" }) },
  help: { label: "Help", run: () => call("open_url", { which: "help" }) },
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
    list.append(item);
  }
}

function renderNotices() {
  const box = byId("go-notices");
  box.replaceChildren();
  for (const notice of notices.values()) {
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
  document.addEventListener("oli:input", (event) => {
    const old = rows.get(event.detail.input) || {};
    rows.set(event.detail.input, { ...old, ...event.detail });
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
  document.addEventListener("oli:notice", (event) => {
    if (event.detail.clear) {
      notices.delete(event.detail.clear);
    } else {
      notices.set(event.detail.kind, event.detail);
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
    notices.clear();
    renderNotices();
    if (event.detail.error) {
      showGoProblem(event.detail.error);
    } else if (event.detail.left_behind.length > 0) {
      showGoProblem({
        message:
          "Still in Studio, held by an active production: " +
          event.detail.left_behind.join(", ") +
          ". Deactivate it in Studio; the next Go live removes them.",
        action: "open_studio",
      });
    }
  });
}

async function start() {
  wireWhere();
  wireGoLive();
  const state = await call("state");
  document.getElementById("version").textContent = "Version " + state.version;
  setLive(state.live);
  renderWhere(state.where);
  byId("show-all").addEventListener("change", (event) => {
    listDevices(event.target.checked).catch(showProblem);
  });
  await listDevices(false);
}

start().catch(showProblem);
