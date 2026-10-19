import { $, el, icon, invoke, toast, copyText } from "./util.js";

const PROMPTS = [
  "What did I dictate today?",
  "Summarize what I dictated this week.",
  "Turn yesterday's dictations into a to-do list.",
  "Find the dictation where I talked about the release plan.",
];

let view = null;
let busy = false;

export function initMcp() {
  $("mcp-prompts").replaceChildren(
    ...PROMPTS.map((prompt) =>
      el(
        "button",
        {
          className: "prompt-chip",
          type: "button",
          title: "Copy prompt",
          onclick: () => copyText(prompt),
        },
        el("span", {}, prompt),
        icon("copy"),
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
  const installed = view.clients.filter((client) => client.status !== "missing");
  const missing = view.clients.filter((client) => client.status === "missing");
  $("mcp-clients").replaceChildren(...[...installed, ...missing].map(renderClient));
}

function renderClient(client) {
  const button = (text, className, busyText, name) =>
    el(
      "button",
      {
        className: `${className} small`.trim(),
        type: "button",
        disabled: name === "connect_mcp_client" && !view.server_exists,
        onclick: (event) => act(event.currentTarget, busyText, client, name),
      },
      text,
    );
  const disconnect = () =>
    button("Disconnect", "ghost", "Disconnecting…", "disconnect_mcp_client");

  const [note, actions] = {
    connected: [
      null,
      [el("span", { className: "connection-state connected" }, icon("check"), "Connected"), disconnect()],
    ],
    outdated: [
      client.hint,
      [button("Update", "", "Updating…", "connect_mcp_client"), disconnect()],
    ],
    disconnected: [null, [button("Connect", "", "Connecting…", "connect_mcp_client")]],
    missing: [null, [el("span", { className: "connection-state" }, "Not installed")]],
  }[client.status];

  return el(
    "article",
    { className: `connection-row ${client.status}` },
    el(
      "div",
      { className: "connection-text" },
      el("div", { className: "connection-name" }, client.name),
      note ? el("div", { className: "connection-note" }, note) : null,
    ),
    el("div", { className: "connection-actions" }, ...actions),
  );
}

async function act(button, busyLabel, client, command) {
  busy = true;
  for (const other of $("mcp-clients").querySelectorAll("button")) other.disabled = true;
  button.textContent = busyLabel;
  try {
    view = await invoke(command, { id: client.id });
    if (command === "connect_mcp_client") {
      const updated = view.clients.find((other) => other.id === client.id);
      const hint = updated?.status === "connected" ? updated.hint : "";
      toast(`${client.name} connected. ${hint}`.trim(), { duration: 5000 });
    } else {
      toast(`${client.name} disconnected`);
    }
  } catch (error) {
    toast(String(error), { error: true, duration: 6000 });
  } finally {
    busy = false;
    render();
  }
}
