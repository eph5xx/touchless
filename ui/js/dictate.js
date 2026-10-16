import {
  $,
  el,
  invoke,
  formatDuration,
  shake,
  copyText,
  shortcutKeys,
  shortcutText,
  shortcutVerb,
} from "./util.js";

// `press` is e.g. ["Tap ", <kbd>⌥</kbd>, " ", <kbd>⌘</kbd>], or null before settings load.
const STATUS = {
  idle: {
    label: "Ready",
    hint: (press) =>
      press ? [...press, " or click the button to start."] : ["Click the button to start."],
    action: "Start dictation",
  },
  connecting: {
    label: "Connecting",
    hint: () => ["Go ahead and talk. Connecting…"],
    action: "Stop dictation",
  },
  listening: {
    label: "Listening",
    hint: (press) => (press ? [...press, " again to finish."] : ["Click the button to finish."]),
    action: "Stop dictation",
  },
  finalizing: {
    label: "Finalizing",
    hint: () => ["Finishing up…"],
    action: "Finalizing",
  },
};

let state = null;
let stoppedAt = null;
let timerHandle = null;
let getShortcut = () => null;

export function initDictate(shortcut) {
  getShortcut = shortcut;
  $("record").addEventListener("click", () => {
    if (state?.status === "finalizing") shake($("record"));
    invoke("toggle_recording");
  });
  $("copy-live").addEventListener("click", () => copyText(state?.committed ?? ""));
}

export function renderRecorder(next) {
  if (next.status === "finalizing" && state?.status !== "finalizing") stoppedAt = Date.now();
  if (next.status !== "finalizing") stoppedAt = null;
  state = next;

  const { status } = state;
  const info = STATUS[status];
  const button = $("record");
  button.className = `record-button ${status}`;
  button.setAttribute("aria-label", info.action);
  button.title = info.action;

  $("status-label").textContent = info.label;
  $("status-label").className = `status-label ${status}`;
  renderShortcut();
  $("nav-dot").className = `dot ${status}`;

  $("committed").textContent = state.committed;
  $("partial").textContent = state.committed && state.partial ? ` ${state.partial}` : state.partial;
  $("transcript-empty").hidden = Boolean(state.committed || state.partial);
  $("copy-live").hidden = !state.committed;

  renderTimer();
  if (status === "idle" || status === "finalizing") {
    clearInterval(timerHandle);
    timerHandle = null;
  } else if (!timerHandle) {
    timerHandle = setInterval(renderTimer, 250);
  }
}

/** Renders the text that mentions the shortcut. */
export function renderShortcut() {
  const shortcut = getShortcut();
  const verb = shortcut && shortcutVerb(shortcut);
  $("dictate-subtitle").textContent = shortcut
    ? `${verb} ${shortcutText(shortcut)}, talk, and your words appear where you type.`
    : "Talk, and your words appear where you type.";
  const press = shortcut ? [`${verb} `, ...shortcutKeys(shortcut)] : null;
  $("status-hint").replaceChildren(...STATUS[state?.status ?? "idle"].hint(press));
}

function renderTimer() {
  const end = stoppedAt ?? Date.now();
  const active = state && state.status !== "idle" && state.started_at;
  $("timer").textContent = active ? formatDuration(end - state.started_at) : "00:00";
}

export function onToggleIgnored() {
  shake($("record"));
}

const HOTKEY_MESSAGES = {
  needs_permission: {
    text: (name) => `${name} needs Input Monitoring permission. Grant it, then restart Touchless.`,
    actions: [
      ["Open Settings", () => invoke("open_privacy_settings", { pane: "input_monitoring" })],
      ["Restart", () => invoke("restart_app")],
    ],
  },
  unavailable: {
    text: (name) => `${name} could not be started. Choose another one, or use the button.`,
    actions: [["Change shortcut", () => (location.hash = "#settings")]],
  },
};

/** Renders the warnings above the recorder from the shared app state. */
export function renderCallouts({ recorder, settings, permissions }) {
  const callouts = [];
  const callout = (kind, text, actions = []) =>
    el(
      "div",
      { className: `callout ${kind}` },
      el("p", {}, text),
      ...actions.map(([label, onclick]) =>
        el("button", { className: "secondary small", type: "button", onclick }, label),
      ),
    );

  if (recorder?.error) callouts.push(callout("error", recorder.error));

  if (settings && !settings.api_key.saved) {
    callouts.push(
      callout("info", "Add your ElevenLabs API key to start dictating.", [
        ["Open Settings", () => (location.hash = "#settings")],
      ]),
    );
  }

  const hotkey = HOTKEY_MESSAGES[recorder?.hotkey];
  if (hotkey) {
    const name = settings ? `The ${shortcutText(settings.shortcut)} shortcut` : "The shortcut";
    callouts.push(callout("warning", hotkey.text(name), hotkey.actions));
  }

  if (settings?.preferences.auto_paste && permissions && !permissions.accessibility) {
    callouts.push(
      callout(
        "warning",
        "To paste into other apps, allow Touchless in Accessibility. Until then, transcripts are copied to the clipboard.",
        [["Grant access", () => invoke("request_accessibility")]],
      ),
    );
  }

  $("callouts").replaceChildren(...callouts);
}
