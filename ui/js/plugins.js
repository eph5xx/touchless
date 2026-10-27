import { $, el, invoke, toast } from "./util.js";

/** Buttons that set up instruments an enhancement needs. */
const SETUP_ACTIONS = {
  llm: "Connect OpenAI",
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

  const setup = missingDependencies.map((instrument) =>
    el(
      "button",
      {
        className: "secondary small enhancement-setup",
        type: "button",
        onclick: openServiceSettings,
      },
      SETUP_ACTIONS[instrument] ?? `Set up ${instrument}`,
    ),
  );

  const settings =
    plugin.enabled && plugin.settings?.length
      ? el(
          "div",
          { className: "enhancement-settings" },
          ...plugin.settings.flatMap((setting) => renderSetting(plugin, setting)),
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
        ...setup,
        el("label", { className: "enhancement-toggle", title: toggle.title }, toggle),
      ),
    ),
    settings,
  );
}

/** The label and control of one setting, as two cells of the settings grid. */
function renderSetting(plugin, setting) {
  const label = el(
    "div",
    { className: "enhancement-setting-label", title: setting.hint ?? "" },
    setting.label,
  );

  if (setting.kind.type === "range") {
    const { min, max, step, unit } = setting.kind;
    const value = el("span", { className: "enhancement-range-value" }, `${setting.value} ${unit}`);
    const input = el("input", {
      type: "range",
      min,
      max,
      step,
      value: setting.value,
      ariaLabel: setting.label,
    });
    input.addEventListener("input", () => {
      value.textContent = `${input.value} ${unit}`;
    });
    input.addEventListener("change", () => saveSetting(plugin, setting, Number(input.value)));
    return [label, el("div", { className: "enhancement-range" }, input, value)];
  }

  if (setting.kind.type === "any_of") {
    const chosen = new Set(setting.value);
    const pills = setting.kind.choices.map((choice) => {
      const on = chosen.has(choice.id);
      const last = on && chosen.size === 1;
      return el(
        "button",
        {
          type: "button",
          className: "enhancement-pill",
          "aria-pressed": String(on),
          disabled: last,
          title: last ? "Keep at least one on" : (choice.hint ?? ""),
          onclick: () => {
            const next = setting.kind.choices
              .map(({ id }) => id)
              .filter((id) => (id === choice.id ? !on : chosen.has(id)));
            saveSetting(plugin, setting, next);
          },
        },
        choice.label,
      );
    });
    return [
      label,
      el("div", { className: "enhancement-pills", role: "group", ariaLabel: setting.label }, ...pills),
    ];
  }

  return [];
}

async function saveSetting(plugin, setting, value) {
  try {
    plugins = await invoke("set_plugin_setting", { id: plugin.id, setting: setting.id, value });
  } catch (error) {
    toast(String(error), { error: true });
  }
  render();
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
