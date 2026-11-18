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

async function start() {
  wireWhere();
  const state = await call("state");
  document.getElementById("version").textContent = "Version " + state.version;
  renderWhere(state.where);
}

start().catch(showProblem);
