import { $, el, listen, cardBlocks } from "./util.js";

const MAX_CARDS = 3;
/** Keep in sync with `.tip.leaving` in tips.css. */
const FADE_MS = 250;

function show({ card, duration_ms }) {
  const node = el("section", { className: "tip" }, ...cardBlocks(card));
  $("tips").prepend(node);
  for (const extra of [...$("tips").children].slice(MAX_CARDS)) extra.remove();
  setTimeout(() => {
    node.classList.add("leaving");
    setTimeout(() => node.remove(), FADE_MS);
  }, duration_ms);
}

await listen("tip", ({ payload }) => show(payload));
