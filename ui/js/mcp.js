import { $, el, invoke, toast, copyText } from "./util.js";

const PROMPTS = [
  "What did I dictate today?",
  "Summarize what I dictated this week.",
  "Turn yesterday's dictations into a to-do list.",
  "Find the dictation where I talked about the release plan.",
];
const BADGES = {
  connected: ["success", "Connected"],
  outdated: ["warning", "Needs update"],
  disconnected: ["", "Not connected"],
  missing: ["", "Not found"],
};

let view = null;
let busy = false;

export function initMcp() {
  $("mcp-prompts").replaceChildren(
    ...PROMPTS.map((prompt) =>
      el(
        "div",
        { className: "row" },
        el("div", { className: "row-title" }, prompt),
        el(
          "button",
          { className: "secondary small", type: "button", onclick: () => copyText(prompt) },
          "Copy",
        ),
      ),
    ),
  );
}

export async function loadMcp() {
  // A reload mid-action would replace the busy button.
  if (busy) return;
  try {
    view = await invoke("list_mcp_clients");
  } catch (error) {
    toast(`Could not load apps: ${error}`, { error: true });
    return;
  }
  if (!busy) render();
}

function render() {
  $("mcp-callouts").replaceChildren(
    ...(view.server_exists
      ? []
      : [
          el(
            "div",
            { className: "callout warning" },
            el(
              "p",
              {},
              `The MCP server is missing from ${view.server_path}. Reinstall Touchless, or run \`cargo build --bin touchless-mcp\` when running from source.`,
            ),
          ),
        ]),
  );
  $("mcp-clients").replaceChildren(...view.clients.map(renderClient));
}

function renderClient(client) {
  const [kind, label] = BADGES[client.status];
  const button = (text, className, action) =>
    el("button", { className: `${className} small`, type: "button", onclick: action }, text);
  const command = (name, busyText, done) => (event) =>
    act(event.currentTarget, busyText, () => invoke(name, { id: client.id }), `${client.name} ${done}`);
  const connect = (text, busyText) =>
    button(text, "", command("connect_mcp_client", busyText, "connected"));
  const disconnect = button(
    "Disconnect",
    "ghost danger",
    command("disconnect_mcp_client", "Disconnecting…", "disconnected"),
  );

  const actions = {
    connected: [disconnect],
    outdated: [connect("Update", "Updating…"), disconnect],
    disconnected: [connect("Connect", "Connecting…")],
    missing: [],
  }[client.status];
  for (const action of actions) {
    if (action !== disconnect && !view.server_exists) action.disabled = true;
  }

  return el(
    "div",
    { className: "row wrap" },
    el(
      "div",
      { className: "row-text" },
      el("div", { className: "row-title" }, client.name),
      el("div", { className: "row-subtitle" }, client.hint),
    ),
    el(
      "div",
      { className: "row-actions" },
      el("span", { className: `badge ${kind}` }, label),
      ...actions,
    ),
  );
}

async function act(button, busyLabel, action, done) {
  busy = true;
  for (const other of $("mcp-clients").querySelectorAll("button")) other.disabled = true;
  button.textContent = busyLabel;
  try {
    view = await action();
    toast(done);
  } catch (error) {
    toast(String(error), { error: true, duration: 6000 });
  } finally {
    busy = false;
    render();
  }
}
