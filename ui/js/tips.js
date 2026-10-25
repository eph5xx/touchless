import { $, el, invoke, listen, cardBlocks } from "./util.js";

const MAX_CARDS = 3;
/** Keep in sync with `.tip.leaving` in tips.css. */
const FADE_MS = 250;
/** Time a card still gets once the pointer leaves, so it can be read. */
const MIN_RESUME_MS = 1500;

/** Timer state per card on screen: `{ remaining, started, timer }`. */
const timers = new Map();
let hovering = false;

/** Sizes the window to the cards, or hides it once none are left. */
function fit() {
  const tips = $("tips");
  // Layout ignores the slide-in transform, and offset sizes are measured even
  // while the window is hidden.
  const empty = tips.children.length === 0;
  if (empty) hovering = false;
  invoke("fit_tips", {
    width: empty ? 0 : tips.offsetWidth,
    height: empty ? 0 : tips.offsetHeight,
  }).catch((error) => console.error("could not fit the tips window", error));
}

function start(node) {
  const state = timers.get(node);
  if (!state || state.timer) return;
  state.started = Date.now();
  state.timer = setTimeout(() => dismiss(node), state.remaining);
}

function pause(node) {
  const state = timers.get(node);
  if (!state?.timer) return;
  clearTimeout(state.timer);
  state.timer = null;
  state.remaining -= Date.now() - state.started;
}

function dismiss(node) {
  const state = timers.get(node);
  if (!state) return;
  clearTimeout(state.timer);
  timers.delete(node);
  node.classList.add("leaving");
  setTimeout(() => {
    node.remove();
    fit();
  }, FADE_MS);
}

function show({ card, duration_ms, transcript_id, hint }) {
  const node = el(
    "section",
    { className: "tip" },
    hint ? el("div", { className: "tip-hint" }, hint) : null,
    ...cardBlocks(card),
  );
  node.addEventListener("click", () => {
    dismiss(node);
    if (transcript_id != null) {
      invoke("open_transcript", { id: transcript_id }).catch((error) =>
        console.error("could not open the transcript", error),
      );
    }
  });
  $("tips").prepend(node);
  for (const extra of [...$("tips").children].slice(MAX_CARDS)) {
    clearTimeout(timers.get(extra)?.timer);
    timers.delete(extra);
    extra.remove();
  }
  timers.set(node, { remaining: duration_ms, started: 0, timer: null });
  if (!hovering) start(node);
  fit();
}

function setHovering(next) {
  if (next === hovering) return;
  hovering = next;
  for (const [node, state] of timers) {
    if (hovering) {
      pause(node);
    } else {
      state.remaining = Math.max(state.remaining, MIN_RESUME_MS);
      start(node);
    }
  }
}

await listen("tip", ({ payload }) => show(payload));
await listen("tips-hover", ({ payload }) => setHovering(payload));
