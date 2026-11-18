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

function hideProblem() {
  document.getElementById("problem").hidden = true;
}

// What to send (spec, section 5). The core builds the rows; the page draws them and
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
    hideProblem();
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
  hideProblem();
}

async function start() {
  const state = await call("state");
  document.getElementById("version").textContent = "Version " + state.version;
  document.getElementById("show-all").addEventListener("change", (event) => {
    listDevices(event.target.checked).catch(showProblem);
  });
  await listDevices(false);
}

start().catch(showProblem);
