import { $, el, icon, invoke, toast } from "./util.js";

/** User-facing setup details for instruments an enhancement needs. */
const DEPENDENCIES = {
  llm: {
    status: "Needs OpenAI",
    action: "Set up OpenAI",
  },
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
  const enabled = plugins.filter((plugin) => plugin.enabled);
  const available = plugins.filter((plugin) => !plugin.enabled);
  const group = (label, items) =>
    items.length
      ? [el("div", { className: "collection-label" }, label), ...items.map(renderPlugin)]
      : [];

  $("plugins-list").replaceChildren(...group("On", enabled), ...group("Available", available));
  $("plugins-empty").hidden = plugins.length > 0;
}

function renderPlugin(plugin) {
  const missingDependencies = plugin.missing ?? [];
  const needsSetup = missingDependencies.length > 0;
  const toggle = el("input", {
    type: "checkbox",
    className: "switch",
    checked: plugin.enabled,
    disabled: needsSetup && !plugin.enabled,
    ariaLabel: `${plugin.enabled ? "Turn off" : "Turn on"} ${plugin.name}`,
    title: plugin.enabled ? "Turn off" : "Turn on",
  });
  toggle.addEventListener("change", () => setEnabled(plugin, toggle.checked));

  const status = needsSetup
    ? ["warning", DEPENDENCIES[missingDependencies[0]]?.status ?? "Setup required"]
    : plugin.enabled
      ? ["success", "On"]
      : ["", "Available"];

  const setup = missingDependencies.map((instrument) => {
    const dependency = DEPENDENCIES[instrument];
    return el(
      "button",
      {
        className: "secondary small enhancement-setup",
        type: "button",
        onclick: openServiceSettings,
      },
      dependency?.action ?? `Set up ${instrument}`,
    );
  });

  const example =
    plugin.id === "english_teacher"
      ? el(
          "div",
          { className: "enhancement-example" },
          el(
            "div",
            { className: "enhancement-example-change" },
            el("span", { className: "enhancement-before" }, "simple"),
            el("span", { className: "enhancement-arrow" }, icon("arrow-right")),
            el("span", { className: "enhancement-after" }, "casual"),
          ),
          el("p", {}, "relaxed and informal · Uses OpenAI after English dictations."),
        )
      : null;

  return el(
    "article",
    { className: "enhancement-row" },
    el(
      "div",
      { className: "enhancement-main" },
      el(
        "div",
        { className: "enhancement-text" },
        el("div", { className: "enhancement-name" }, plugin.name),
        el("p", { className: "enhancement-description" }, plugin.description),
      ),
      el(
        "div",
        { className: "enhancement-controls" },
        el("span", { className: `badge ${status[0]}` }, status[1]),
        ...setup,
        el("label", { className: "enhancement-toggle", title: toggle.title }, toggle),
      ),
    ),
    example,
  );
}

function openServiceSettings() {
  location.hash = "#settings";
  document.querySelector('[data-settings-tab="services"]')?.click();
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
