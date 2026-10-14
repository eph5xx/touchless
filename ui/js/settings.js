import { $, el, invoke, toast } from "./util.js";

const SOURCE_BADGES = {
  keychain: ["success", "Saved in Keychain"],
  environment: ["warning", "Using .env"],
};

let onChange = () => {};
let current = null;

export function initSettings(handleChange) {
  onChange = handleChange;
  for (const input of document.querySelectorAll("[data-preference]")) {
    input.addEventListener("change", savePreferences);
  }
}

export function renderSettings(settings) {
  current = settings;
  for (const input of document.querySelectorAll("[data-preference]")) {
    input.checked = Boolean(settings.preferences[input.dataset.preference]);
  }
  const restore = document.querySelector('[data-preference="restore_clipboard"]');
  restore.disabled = !settings.preferences.auto_paste;
  restore.closest(".row").style.opacity = settings.preferences.auto_paste ? "" : "0.5";

  // Keep typed-but-unsaved keys when re-rendering.
  const drafts = {};
  for (const input of document.querySelectorAll("#providers input")) {
    drafts[input.dataset.provider] = input.value;
  }
  $("providers").replaceChildren(
    ...settings.providers.map((provider) => renderProvider(provider, drafts[provider.id] ?? "")),
  );
}

function renderProvider(provider, draft) {
  const [badgeKind, badgeText] = SOURCE_BADGES[provider.source] ?? ["", "Not set"];
  const input = el("input", {
    type: "password",
    placeholder: provider.hint ? `Current key ${provider.hint}` : "Paste API key",
    value: draft,
    autocomplete: "off",
    spellcheck: false,
    dataset: { provider: provider.id },
  });
  const save = el("button", { type: "button", disabled: !draft.trim() }, "Save");
  input.addEventListener("input", () => (save.disabled = !input.value.trim()));
  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && input.value.trim()) save.click();
  });

  save.addEventListener("click", () =>
    run(save, async () => {
      const settings = await invoke("set_api_key", { provider: provider.id, key: input.value });
      input.value = "";
      toast(`${provider.name} key saved`);
      update(settings);
    }),
  );

  const test = provider.testable
    ? el("button", { className: "secondary", type: "button", disabled: !provider.source }, "Test")
    : null;
  test?.addEventListener("click", () =>
    run(test, async () => toast(await invoke("test_api_key", { provider: provider.id })), "Testing…"),
  );

  const remove =
    provider.source === "keychain"
      ? el("button", { className: "ghost danger", type: "button" }, "Remove")
      : null;
  remove?.addEventListener("click", () =>
    run(remove, async () => {
      update(await invoke("remove_api_key", { provider: provider.id }));
      toast(`${provider.name} key removed`);
    }),
  );

  return el(
    "div",
    { className: "card provider" },
    el(
      "div",
      { className: "provider-head" },
      el("span", { className: "provider-name" }, provider.name),
      el("span", { className: `badge ${badgeKind}` }, badgeText),
    ),
    el("p", { className: "provider-description" }, provider.description),
    el("div", { className: "provider-form" }, input, save, test, remove),
  );
}

async function run(button, action, busyLabel) {
  const label = button.textContent;
  button.disabled = true;
  if (busyLabel) button.textContent = busyLabel;
  try {
    await action();
  } catch (error) {
    toast(String(error), { error: true, duration: 4000 });
  } finally {
    if (button.isConnected) {
      button.disabled = false;
      button.textContent = label;
    }
  }
}

function update(settings) {
  renderSettings(settings);
  onChange(settings);
}

async function savePreferences() {
  const preferences = { ...current.preferences };
  for (const input of document.querySelectorAll("[data-preference]")) {
    preferences[input.dataset.preference] = input.checked;
  }
  try {
    update(await invoke("update_preferences", { preferences }));
  } catch (error) {
    toast(String(error), { error: true });
    renderSettings(current);
  }
}

export function renderPermissions(permissions) {
  const row = (title, subtitle, granted, actions) =>
    el(
      "div",
      { className: "row" },
      el(
        "div",
        {},
        el("div", { className: "row-title" }, title),
        el("div", { className: "row-subtitle" }, subtitle),
      ),
      el(
        "div",
        { className: "row-actions" },
        ...(granted
          ? [el("span", { className: "badge success" }, "Allowed")]
          : [
              el("span", { className: "badge warning" }, "Not allowed"),
              ...actions.map(([label, onclick]) =>
                el("button", { className: "secondary small", type: "button", onclick }, label),
              ),
            ]),
      ),
    );

  $("permissions").replaceChildren(
    row(
      "Input Monitoring",
      "Needed for the ⌘⌥ shortcut. Restart Touchless after allowing it.",
      permissions.input_monitoring,
      [
        ["Open Settings", () => invoke("open_privacy_settings", { pane: "input_monitoring" })],
        ["Restart", () => invoke("restart_app")],
      ],
    ),
    row(
      "Accessibility",
      "Needed to paste transcripts into other apps.",
      permissions.accessibility,
      [
        ["Grant access", () => invoke("request_accessibility")],
        ["Open Settings", () => invoke("open_privacy_settings", { pane: "accessibility" })],
      ],
    ),
  );
}
