import { $, el, invoke, formatDuration, shake, copyText } from "./util.js";

const STATUS = {
  idle: {
    label: "Ready",
    hint: "Tap <kbd>⌘</kbd> <kbd>⌥</kbd> or click the button to start.",
    action: "Start dictation",
  },
  connecting: {
    label: "Connecting",
    hint: "Start talking. Audio is buffered while connecting.",
    action: "Stop dictation",
  },
  listening: {
    label: "Listening",
    hint: "Tap <kbd>⌘</kbd> <kbd>⌥</kbd> again to finish.",
    action: "Stop dictation",
  },
  finalizing: {
    label: "Finalizing",
    hint: "Waiting for the last words from ElevenLabs…",
    action: "Finalizing",
  },
};

let state = null;
let stoppedAt = null;
let timerHandle = null;

export function initDictate() {
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
  $("status-hint").innerHTML = info.hint;
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
    text: "The ⌘⌥ shortcut needs Input Monitoring permission. Grant it, then restart Touchless.",
    actions: [
      ["Open Settings", () => invoke("open_privacy_settings", { pane: "input_monitoring" })],
      ["Restart", () => invoke("restart_app")],
    ],
  },
  unavailable: {
    text: "The ⌘⌥ shortcut could not be started. You can still use the button.",
    actions: [],
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
  if (hotkey) callouts.push(callout("warning", hotkey.text, hotkey.actions));

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
