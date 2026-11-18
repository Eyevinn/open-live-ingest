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

async function start() {
  const state = await call("state");
  document.getElementById("version").textContent = "Version " + state.version;
}

start().catch(showProblem);
