import { $, el, invoke, toast } from "./util.js";

/** What to tell the user about an instrument a plugin needs but is not set up. */
const MISSING = {
  llm: "Needs an OpenAI key",
};

let plugins = [];

export async function loadPlugins() {
  try {
    plugins = await invoke("list_plugins");
  } catch (error) {
    toast(`Could not load plugins: ${error}`, { error: true });
    plugins = [];
  }
  render();
}

function render() {
  $("plugins-list").replaceChildren(...plugins.map(renderPlugin));
  $("plugins-empty").hidden = plugins.length > 0;
}

function renderPlugin(plugin) {
  const toggle = el("input", {
    type: "checkbox",
    className: "switch",
    checked: plugin.enabled,
    title: plugin.enabled ? "Turn off" : "Turn on",
  });
  toggle.addEventListener("change", () => setEnabled(plugin, toggle.checked));

  const needs = plugin.missing.map((instrument) =>
    el(
      "div",
      { className: "plugin-needs" },
      el("span", { className: "badge warning" }, MISSING[instrument] ?? `Needs ${instrument}`),
      el(
        "button",
        { className: "secondary small", type: "button", onclick: () => (location.hash = "#settings") },
        "Add key",
      ),
    ),
  );

  return el(
    "article",
    { className: "card plugin" },
    el(
      "label",
      { className: "plugin-head" },
      el(
        "div",
        { className: "plugin-text" },
        el("div", { className: "plugin-name" }, plugin.name),
        el("p", { className: "plugin-description" }, plugin.description),
      ),
      toggle,
    ),
    ...needs,
  );
}

async function setEnabled(plugin, enabled) {
  try {
    plugins = await invoke("set_plugin_enabled", { id: plugin.id, enabled });
    toast(`${plugin.name} is ${enabled ? "on" : "off"}`);
  } catch (error) {
    toast(String(error), { error: true });
  }
  render();
}
