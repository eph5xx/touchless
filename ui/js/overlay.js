import { $, el, invoke, listen, formatDuration, shake, cardBlocks } from "./util.js";

const DELIVERY_LABELS = {
  pasted: "Pasted",
  copied: "Copied",
  saved: "Saved",
};

let state = null;
let finished = null;
let stoppedAt = null;
let timerHandle = null;
/** Plugin cards for the finished dictation, by plugin id. */
const cards = new Map();

const minimal = () => document.body.dataset.style === "minimal";

function setStyle(preferences) {
  document.body.dataset.style = preferences.overlay;
  render();
}

function render() {
  renderContent();
  if (minimal()) return;
  // Right-align overflowing text so the newest words stay visible.
  const textBox = $("pill").querySelector(".text");
  textBox.classList.add("short");
  textBox.classList.toggle("short", $("text").offsetWidth <= textBox.clientWidth);
}

function renderContent() {
  const pill = $("pill");

  if (finished) {
    const kind = finished.error ? "failed" : finished.text ? "done" : "empty";
    pill.className = `pill ${kind}`;
    $("committed").textContent = finished.error ?? (finished.text || "Nothing heard");
    $("committed").className = finished.text || finished.error ? "" : "placeholder";
    $("partial").textContent = "";
    const delivery = DELIVERY_LABELS[finished.delivery] ?? "";
    if (!minimal()) $("meta").textContent = finished.error ? "" : delivery;
    else if (kind === "failed") $("meta").textContent = "Failed";
    else if (kind === "empty") $("meta").textContent = "Nothing heard";
    else $("meta").textContent = delivery || "Done";
    return;
  }

  const status = state?.status ?? "idle";
  pill.className = `pill ${status}`;
  const committed = state?.committed ?? "";
  const partial = state?.partial ?? "";
  const hasText = Boolean(committed || partial);

  $("committed").className = hasText ? "" : "placeholder";
  $("committed").textContent = hasText
    ? committed
    : status === "connecting"
      ? "Connecting…"
      : "Listening…";
  $("partial").textContent = committed && partial ? ` ${partial}` : partial;

  const end = stoppedAt ?? Date.now();
  $("meta").textContent =
    status === "finalizing"
      ? "Finalizing"
      : state?.started_at
        ? formatDuration(end - state.started_at)
        : "";
}

function renderCards() {
  $("cards").replaceChildren(...[...cards.values()].map(renderCard));
}

function renderCard(entry) {
  const [tone, body] = entry.pending
    ? ["pending", [el("div", { className: "block-text" }, "Checking…")]]
    : entry.error
      ? ["failed", [el("div", { className: "block-text" }, `Couldn't check this one: ${entry.error}`)]]
      : [entry.card.tone, cardBlocks(entry.card)];
  return el(
    "section",
    { className: `card ${tone}` },
    el("header", {}, el("span", { className: "spinner" }), entry.name),
    ...body,
  );
}

/** Applies a plugin event if it belongs to the dictation on screen. */
function onPlugin(payload, entry) {
  if (finished?.transcript_id == null || finished.transcript_id !== payload.transcript_id) return;
  if (entry) cards.set(payload.plugin_id, { name: payload.plugin_name, ...entry });
  else cards.delete(payload.plugin_id);
  renderCards();
}

function onState(next) {
  if (next.status === "connecting" && state?.status !== "connecting") {
    finished = null;
    cards.clear();
    renderCards();
  }
  if (next.status === "finalizing" && state?.status !== "finalizing") stoppedAt = Date.now();
  if (next.status === "connecting") stoppedAt = null;
  state = next;

  const running = state.status === "connecting" || state.status === "listening";
  if (running && !timerHandle) timerHandle = setInterval(render, 250);
  if (!running) {
    clearInterval(timerHandle);
    timerHandle = null;
  }
  render();
}

await listen("recorder-state", ({ payload }) => onState(payload));
await listen("dictation-finished", ({ payload }) => {
  finished = payload;
  render();
});
await listen("plugin-started", ({ payload }) => onPlugin(payload, { pending: true }));
await listen("plugin-finished", ({ payload }) => {
  const { card, error } = payload;
  // A plugin with nothing to say (e.g. not English) just disappears.
  onPlugin(payload, card || error ? { card, error } : null);
});
await listen("toggle-ignored", () => shake($("pill")));
await listen("preferences-changed", ({ payload }) => setStyle(payload));

setStyle((await invoke("get_settings")).preferences);
onState(await invoke("get_state"));
