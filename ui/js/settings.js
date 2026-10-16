import { $, el, invoke, toast, shortcutKeys, shortcutText, shortcutVerb } from "./util.js";
import { LANGUAGES, languageName } from "./languages.js";

const MAX_KEYTERMS = 50;
const REWRITE_HINTS = {
  off: "Pastes your words as transcribed.",
  enhance: "Fixes grammar and smooths wording, in your voice. Costs 30% more.",
  structure: "Adds paragraphs and lists when your text needs them. Costs 30% more.",
  custom: "Tell it how to rewrite your text. Costs 30% more.",
};
const PROVIDERS = {
  elevenlabs: {
    name: "ElevenLabs",
    description: "Turns your speech into text. Your key stays in your Mac's Keychain.",
  },
  openai: {
    name: "OpenAI",
    description: "Powers plugins like English Teacher. Your key stays in your Mac's Keychain.",
  },
};

let onChange = () => {};
let current = null;
let recordingShortcut = false;

export function initSettings(handleChange) {
  onChange = handleChange;
  for (const input of document.querySelectorAll("[data-preference]")) {
    input.addEventListener("change", savePreferences);
  }

  $("shortcut-change").addEventListener("click", recordShortcut);
  $("shortcut-reset").addEventListener("click", (event) =>
    run(event.currentTarget, async () => {
      update(await invoke("reset_shortcut"));
      toast("Shortcut reset");
    }),
  );
  // Keys pressed while recording belong to the new shortcut, not to the page.
  for (const type of ["keydown", "keyup"]) {
    document.addEventListener(
      type,
      (event) => {
        if (recordingShortcut) event.preventDefault();
      },
      true,
    );
  }
  window.addEventListener("blur", () => {
    if (recordingShortcut) invoke("cancel_shortcut_capture");
  });

  $("language-code").replaceChildren(
    el("option", { value: "" }, "Auto-detect"),
    ...LANGUAGES.map(([code, name]) => el("option", { value: code }, name)),
  );
  $("language-code").addEventListener("change", (event) =>
    saveTranscription({ language_code: event.target.value || null }),
  );
  $("secondary-add").addEventListener("change", (event) => {
    const code = event.target.value;
    if (!code) return;
    saveTranscription({
      secondary_languages: [...current.transcription.secondary_languages, code],
    });
  });
  $("no-verbatim").addEventListener("change", (event) =>
    saveTranscription({ no_verbatim: event.target.checked }),
  );
  $("keyterm-input").addEventListener("keydown", async (event) => {
    const input = event.target;
    if (event.key !== "Enter" || !input.value.trim()) return;
    event.preventDefault();
    if (await saveTranscription({ keyterms: [...current.transcription.keyterms, input.value] })) {
      input.value = "";
    }
  });
  $("rewrite").addEventListener("change", async (event) => {
    const rewrite = event.target.value;
    if ((await saveTranscription({ rewrite })) && rewrite === "custom") $("transcript-edit").focus();
  });
  $("transcript-edit").addEventListener("change", (event) =>
    saveTranscription({ transcript_edit: event.target.value }),
  );
}

export function renderSettings(settings) {
  current = settings;
  for (const input of document.querySelectorAll("[data-preference]")) {
    const value = settings.preferences[input.dataset.preference];
    if (input.type === "checkbox") input.checked = Boolean(value);
    else input.value = value;
  }
  const restore = document.querySelector('[data-preference="restore_clipboard"]');
  restore.disabled = !settings.preferences.auto_paste;
  restore.closest(".row").style.opacity = settings.preferences.auto_paste ? "" : "0.5";

  for (const provider of Object.keys(PROVIDERS)) {
    const box = $(`api-key-${provider}`);
    // Keep a typed-but-unsaved key when re-rendering.
    const draft = box.querySelector("input")?.value ?? "";
    box.replaceChildren(...renderApiKey(provider, settings.api_keys[provider], draft));
  }
  renderShortcut(settings);
  renderTranscription(settings.transcription);
}

function renderShortcut(settings) {
  const keys = $("shortcut-keys");
  keys.classList.toggle("recording", recordingShortcut);
  if (recordingShortcut) {
    keys.replaceChildren("Press your shortcut…");
    $("shortcut-hint").textContent =
      "Tap modifier keys like ⌥⌘ or Right ⌥, or press a combination like ⌥Space. Esc cancels.";
  } else {
    keys.replaceChildren(...shortcutKeys(settings.shortcut));
    $("shortcut-hint").textContent =
      `${shortcutVerb(settings.shortcut)} it in any app to start, and again to stop.`;
  }
  $("shortcut-change").textContent = recordingShortcut ? "Cancel" : "Change";
  $("shortcut-reset").hidden = recordingShortcut || settings.shortcut_is_default;
}

async function recordShortcut() {
  if (recordingShortcut) {
    invoke("cancel_shortcut_capture");
    return;
  }
  recordingShortcut = true;
  renderShortcut(current);
  let settings = null;
  try {
    settings = await invoke("capture_shortcut");
  } catch (error) {
    toast(String(error), { error: true, duration: 4000 });
  }
  recordingShortcut = false;
  if (settings) {
    update(settings);
    toast(`Shortcut set to ${shortcutText(settings.shortcut)}`);
  } else {
    renderShortcut(current);
  }
}

function renderApiKey(provider, status, draft) {
  const { name, description } = PROVIDERS[provider];
  const input = el("input", {
    type: "password",
    placeholder: status.hint ? `Current key ${status.hint}` : "Paste API key",
    value: draft,
    autocomplete: "off",
    spellcheck: false,
  });
  const save = el("button", { type: "button", disabled: !draft.trim() }, "Save");
  input.addEventListener("input", () => (save.disabled = !input.value.trim()));
  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && input.value.trim()) save.click();
  });

  save.addEventListener("click", () =>
    run(save, async () => {
      const settings = await invoke("set_api_key", { provider, key: input.value });
      input.value = "";
      toast(`${name} key saved`);
      update(settings);
    }),
  );

  const test = el("button", { className: "secondary", type: "button", disabled: !status.saved }, "Test");
  test.addEventListener("click", () =>
    run(test, async () => toast(await invoke("test_api_key", { provider }), { duration: 4000 }), "Testing…"),
  );

  const remove = status.saved
    ? el("button", { className: "ghost danger", type: "button" }, "Remove")
    : null;
  remove?.addEventListener("click", () =>
    run(remove, async () => {
      update(await invoke("remove_api_key", { provider }));
      toast(`${name} key removed`);
    }),
  );

  const [badgeKind, badgeText] = status.saved ? ["success", "Saved"] : ["", "Not set"];
  return [
    el(
      "div",
      { className: "api-key-head" },
      el("span", { className: "api-key-name" }, name),
      el("span", { className: `badge ${badgeKind}` }, badgeText),
    ),
    el("p", { className: "api-key-description" }, description),
    el("div", { className: "api-key-form" }, input, save, test, remove),
  ];
}

function renderTranscription(options) {
  $("language-code").value = options.language_code ?? "";

  const selected = new Set(options.secondary_languages);
  $("secondary-add").replaceChildren(
    el("option", { value: "" }, "Add language…"),
    ...LANGUAGES.filter(([code]) => code !== options.language_code && !selected.has(code)).map(
      ([code, name]) => el("option", { value: code }, name),
    ),
  );
  $("secondary-add").value = "";
  $("secondary-languages").replaceChildren(
    ...options.secondary_languages.map((code) =>
      chip(languageName(code), () =>
        saveTranscription({
          secondary_languages: options.secondary_languages.filter((item) => item !== code),
        }),
      ),
    ),
  );

  $("no-verbatim").checked = options.no_verbatim;

  $("keyterms").replaceChildren(
    ...options.keyterms.map((term) =>
      chip(term, () =>
        saveTranscription({ keyterms: options.keyterms.filter((item) => item !== term) }),
      ),
    ),
  );
  $("keyterms-count").textContent = `${options.keyterms.length}/${MAX_KEYTERMS}`;
  $("keyterm-input").disabled = options.keyterms.length >= MAX_KEYTERMS;

  $("rewrite").value = options.rewrite;
  $("rewrite-hint").textContent = REWRITE_HINTS[options.rewrite];
  $("transcript-edit").hidden = options.rewrite !== "custom";
  // Don't overwrite the instruction while the user is typing it.
  if (document.activeElement !== $("transcript-edit")) {
    $("transcript-edit").value = options.transcript_edit;
  }
}

function chip(label, onRemove) {
  return el(
    "span",
    { className: "chip" },
    label,
    el("button", { type: "button", className: "chip-remove", title: "Remove", onclick: onRemove }, "×"),
  );
}

/** Saves a partial change to the transcription options; returns whether it worked. */
async function saveTranscription(change) {
  const options = { ...current.transcription, ...change };
  try {
    update(await invoke("update_transcription", { options }));
    return true;
  } catch (error) {
    toast(String(error), { error: true, duration: 4000 });
    renderSettings(current);
    return false;
  }
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
    preferences[input.dataset.preference] = input.type === "checkbox" ? input.checked : input.value;
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
      { className: "row wrap" },
      el(
        "div",
        { className: "row-text" },
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
      "Needed for modifier-key shortcuts and to record a new shortcut. Restart after allowing.",
      permissions.input_monitoring,
      [
        ["Open Settings", () => invoke("open_privacy_settings", { pane: "input_monitoring" })],
        ["Restart", () => invoke("restart_app")],
      ],
    ),
    row(
      "Accessibility",
      "Lets Touchless paste into other apps.",
      permissions.accessibility,
      [
        ["Grant access", () => invoke("request_accessibility")],
        ["Open Settings", () => invoke("open_privacy_settings", { pane: "accessibility" })],
      ],
    ),
  );
}
