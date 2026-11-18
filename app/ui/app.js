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

async function start() {
  wireWhere();
  wireLogs();
  const state = await call("state");
  document.getElementById("version").textContent = "Version " + state.version;
  renderWhere(state.where);
  byId("show-all").addEventListener("change", (event) => {
    listDevices(event.target.checked).catch(showProblem);
  });
  await listDevices(false);
}

start().catch(showProblem);
