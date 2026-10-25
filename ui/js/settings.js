import { $, el, icon, invoke, toast, shortcutKeys, shortcutText, shortcutVerb } from "./util.js";
import { languageName } from "./languages.js";
import { initLanguagePicker, refreshLanguagePicker } from "./language-picker.js";

const MAX_KEYTERMS = 50;
const REWRITE_HINTS = {
  off: "Pastes your words as transcribed.",
  enhance: "Fixes grammar and smooths wording, in your voice.",
  structure: "Adds paragraphs and lists when your text needs them.",
  custom: "Tell it how to rewrite your text.",
};
const REWRITE_COST = "Costs 30% more.";
const KEYTERMS_HINT = "Names and terms to spell right.";
const PROVIDERS = {
  elevenlabs: {
    name: "ElevenLabs",
    description: "Turns your speech into text. Your key stays in your Mac's Keychain.",
  },
  openai: {
    name: "OpenAI",
    description:
      "Powers Rewrite with local models and plugins like Vocabulary Coach. Connect it with an API key or your ChatGPT plan.",
  },
};
const OPENAI_METHODS = [
  ["api_key", "API key"],
  ["chatgpt", "ChatGPT plan"],
];
const OPENAI_METHOD_HINTS = {
  api_key: "Billed to your OpenAI account. Your key stays in your Mac's Keychain.",
  chatgpt: "Requests use the usage included in your ChatGPT plan. Your sign-in stays in your Mac's Keychain.",
};
const SIGN_IN_CANCELLED = "Sign-in cancelled";

let onChange = () => {};
let current = null;
let recordingShortcut = false;
let signingIn = false;
let welcomeSeen = false;

export function initSettings(handleChange) {
  onChange = handleChange;
  initSettingsTabs();
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

  initLanguagePicker($("language-search"), $("language-results"), {
    onPick: (code) => saveLanguages([...pickedLanguages(current.transcription), code]),
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

  const welcome = $("chatgpt-welcome");
  $("chatgpt-welcome-ok").addEventListener("click", () => welcome.close());
  $("chatgpt-welcome-usage").addEventListener("click", openUsage);
  // Escape closes the dialog too; either way it has been seen.
  welcome.addEventListener("close", async () => {
    welcomeSeen = true;
    if (!current?.openai.chatgpt.show_welcome) return;
    try {
      update(await invoke("dismiss_chatgpt_welcome"));
    } catch (error) {
      toast(String(error), { error: true });
    }
  });
}

function initSettingsTabs() {
  const tabs = [...document.querySelectorAll("#settings-tabs [data-settings-tab]")];
  const panels = [...document.querySelectorAll("[data-settings-panel]")];
  if (!tabs.length) return;

  const selectTab = (name, focus = false) => {
    const selected = tabs.find((tab) => tab.dataset.settingsTab === name) ?? tabs[0];

    for (const tab of tabs) {
      const active = tab === selected;
      tab.classList.toggle("active", active);
      tab.setAttribute("aria-selected", String(active));
      tab.tabIndex = active ? 0 : -1;
    }
    for (const panel of panels) {
      panel.hidden = panel.dataset.settingsPanel !== selected.dataset.settingsTab;
    }

    if (focus) selected.focus();
  };

  tabs.forEach((tab, index) => {
    tab.addEventListener("click", () => selectTab(tab.dataset.settingsTab));
    tab.addEventListener("keydown", (event) => {
      let nextIndex;
      if (event.key === "ArrowLeft") nextIndex = (index - 1 + tabs.length) % tabs.length;
      else if (event.key === "ArrowRight") nextIndex = (index + 1) % tabs.length;
      else if (event.key === "Home") nextIndex = 0;
      else if (event.key === "End") nextIndex = tabs.length - 1;
      else return;

      event.preventDefault();
      selectTab(tabs[nextIndex].dataset.settingsTab, true);
    });
  });

  selectTab("general");
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
    box.replaceChildren(
      ...(provider === "openai"
        ? renderOpenAi(settings, draft)
        : renderApiKey(provider, settings.api_keys[provider], draft)),
    );
  }
  renderShortcut(settings);
  renderTranscription(settings);
  renderWelcome(settings);
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
  const badge = status.saved ? ["success", "Saved"] : ["", "Not set"];
  return [
    serviceHead(name, badge),
    el("p", { className: "api-key-description" }, description),
    apiKeyForm(provider, status, draft),
  ];
}

function serviceHead(name, [badgeKind, badgeText]) {
  return el(
    "div",
    { className: "api-key-head" },
    el("span", { className: "api-key-name" }, name),
    el("span", { className: `badge ${badgeKind}` }, badgeText),
  );
}

function apiKeyForm(provider, status, draft) {
  const { name } = PROVIDERS[provider];
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

  return el("div", { className: "api-key-form" }, input, save, test, remove);
}

function renderOpenAi(settings, draft) {
  const { name, description } = PROVIDERS.openai;
  const { method, ready, chatgpt } = settings.openai;
  const badge = ready
    ? ["success", method === "chatgpt" ? "Connected" : "Saved"]
    : ["", "Not set"];
  return [
    serviceHead(name, badge),
    el("p", { className: "api-key-description" }, description),
    methodPicker(method),
    el("p", { className: "method-hint" }, OPENAI_METHOD_HINTS[method]),
    method === "chatgpt"
      ? chatgptControls(chatgpt)
      : apiKeyForm("openai", settings.api_keys.openai, draft),
  ];
}

function methodPicker(method) {
  return el(
    "div",
    { className: "segmented", role: "radiogroup", ariaLabel: "How to connect OpenAI" },
    ...OPENAI_METHODS.map(([value, label]) => {
      const selected = value === method;
      const button = el(
        "button",
        { type: "button", role: "radio", className: selected ? "active" : "", ariaChecked: String(selected) },
        label,
      );
      button.addEventListener("click", () => {
        if (selected) return;
        run(button, async () => update(await invoke("set_openai_method", { method: value })));
      });
      return button;
    }),
  );
}

function chatgptControls(chatgpt) {
  if (signingIn) {
    const cancel = el("button", { className: "secondary", type: "button" }, "Cancel");
    cancel.addEventListener("click", () => invoke("cancel_chatgpt_sign_in"));
    return el(
      "div",
      { className: "api-key-form chatgpt-status" },
      el("span", { className: "muted" }, "Waiting for your browser…"),
      cancel,
    );
  }

  if (!chatgpt.signed_in) {
    const signIn = el("button", { className: "chatgpt-button", type: "button" }, "Continue with ChatGPT");
    signIn.addEventListener("click", signInWithChatGpt);
    return el("div", { className: "api-key-form" }, signIn);
  }

  const usage = el("button", { className: "ghost", type: "button" }, "Manage usage");
  usage.addEventListener("click", openUsage);
  const test = el("button", { className: "secondary", type: "button" }, "Test");
  test.addEventListener("click", () =>
    run(test, async () => toast(await invoke("test_chatgpt"), { duration: 4000 }), "Testing…"),
  );
  const signOut = el("button", { className: "ghost danger", type: "button" }, "Sign out");
  signOut.addEventListener("click", () =>
    run(signOut, async () => {
      const { settings, revoked } = await invoke("chatgpt_sign_out");
      update(settings);
      if (revoked) toast("Signed out of ChatGPT");
      else {
        toast("Signed out on this Mac. OpenAI didn't confirm, so you can also disconnect Touchless in ChatGPT settings.", {
          duration: 7000,
        });
      }
    }),
  );
  return el(
    "div",
    { className: "chatgpt-account" },
    el(
      "div",
      { className: "chatgpt-status" },
      el("span", {}, chatgpt.email ? `Signed in as ${chatgpt.email}` : "Signed in"),
      el("span", { className: "muted" }, "· Using ChatGPT plan"),
    ),
    el("div", { className: "api-key-form" }, usage, test, signOut),
  );
}

async function signInWithChatGpt() {
  signingIn = true;
  renderSettings(current);
  try {
    const settings = await invoke("chatgpt_sign_in");
    signingIn = false;
    update(settings);
    const { email } = settings.openai.chatgpt;
    toast(email ? `Signed in as ${email}` : "Signed in with ChatGPT");
  } catch (error) {
    signingIn = false;
    renderSettings(current);
    if (String(error) !== SIGN_IN_CANCELLED) toast(String(error), { error: true, duration: 6000 });
  }
}

async function openUsage() {
  try {
    await invoke("open_chatgpt_usage");
  } catch (error) {
    toast(String(error), { error: true });
  }
}

/** Shown once, after the first sign-in with ChatGPT plan use. */
function renderWelcome(settings) {
  const dialog = $("chatgpt-welcome");
  if (settings.openai.chatgpt.show_welcome && !welcomeSeen && !dialog.open) dialog.showModal();
}

function renderTranscription(settings) {
  const options = settings.transcription;
  const local = settings.engine !== "elevenlabs";
  const languages = pickedLanguages(options);
  $("languages-hint").textContent = !languages.length
    ? "Touchless detects the language automatically. Add the languages you speak."
    : local && languages.length > 1
      ? "Whisper detects which of these languages you speak."
      : "The first one is your main language.";
  $("languages").replaceChildren(
    ...languages.map((code, index) => languageChip(languages, code, index === 0)),
  );
  refreshLanguagePicker(languages);

  $("no-verbatim").checked = options.no_verbatim;
  // Whisper has no filler-word option.
  $("no-verbatim-row").hidden = local;
  $("keyterms-hint").textContent = local ? KEYTERMS_HINT : `${KEYTERMS_HINT} Costs 20% more.`;

  $("keyterms").replaceChildren(
    ...options.keyterms.map((term) =>
      chip(term, () =>
        saveTranscription({ keyterms: options.keyterms.filter((item) => item !== term) }),
      ),
    ),
  );
  $("keyterms-count").textContent = `${options.keyterms.length}/${MAX_KEYTERMS}`;
  $("keyterm-input").disabled = options.keyterms.length >= MAX_KEYTERMS;

  // With a local model, Rewrite runs through OpenAI, so it needs OpenAI connected.
  const canRewrite = !local || settings.openai.ready;
  const rewrite = canRewrite ? options.rewrite : "off";
  const openAiCost =
    settings.openai.method === "chatgpt" ? "Uses your ChatGPT plan." : "Uses your OpenAI key.";
  $("rewrite").disabled = !canRewrite;
  $("rewrite").value = rewrite;
  $("rewrite-hint").textContent = !canRewrite
    ? "Connect OpenAI in Services."
    : rewrite === "off"
      ? REWRITE_HINTS.off
      : `${REWRITE_HINTS[rewrite]} ${local ? openAiCost : REWRITE_COST}`;
  $("transcript-edit").hidden = rewrite !== "custom";
  // Don't overwrite the instruction while the user is typing it.
  if (document.activeElement !== $("transcript-edit")) {
    $("transcript-edit").value = options.transcript_edit;
  }
}

function languageChip(languages, code, main) {
  const others = languages.filter((item) => item !== code);
  return el(
    "span",
    { className: main ? "chip main" : "chip" },
    languageName(code),
    main
      ? el("span", { className: "chip-tag" }, "Main")
      : el(
          "button",
          {
            type: "button",
            className: "chip-action",
            title: "Make main language",
            ariaLabel: "Make main language",
            onclick: () => saveLanguages([code, ...others]),
          },
          icon("star"),
        ),
    el(
      "button",
      {
        type: "button",
        className: "chip-remove",
        title: "Remove",
        ariaLabel: "Remove",
        onclick: () => saveLanguages(others),
      },
      icon("x"),
    ),
  );
}

/** The user's languages, main one first. */
function pickedLanguages(options) {
  return [options.language_code, ...options.secondary_languages].filter(Boolean);
}

function saveLanguages(codes) {
  return saveTranscription({ language_code: codes[0] ?? null, secondary_languages: codes.slice(1) });
}

function chip(label, onRemove) {
  return el(
    "span",
    { className: "chip" },
    label,
    el(
      "button",
      { type: "button", className: "chip-remove", title: "Remove", ariaLabel: "Remove", onclick: onRemove },
      icon("x"),
    ),
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
