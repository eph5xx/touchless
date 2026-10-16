export const { invoke } = window.__TAURI__.core;
export const { listen } = window.__TAURI__.event;

export const $ = (id) => document.getElementById(id);

/** Creates an element: `el("button", { className: "x", onclick }, "Label", child)`. */
export function el(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (key === "dataset") Object.assign(node.dataset, value);
    else if (key in node) node[key] = value;
    else node.setAttribute(key, value);
  }
  node.append(...children.filter((child) => child != null && child !== false));
  return node;
}

/** Builds an inline SVG icon from path data strings. */
export function icon(...paths) {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  for (const d of paths) {
    const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
    path.setAttribute("d", d);
    svg.append(path);
  }
  return svg;
}

export const ICONS = {
  copy: ["M9 9h10v10H9z", "M5 15V5h10"],
  trash: ["M4 7h16", "M10 11v6M14 11v6", "M6 7l1 13h10l1-13", "M9 7V4h6v3"],
};

const KEY_LABELS = {
  fn: "fn",
  control: "⌃",
  option: "⌥",
  shift: "⇧",
  command: "⌘",
  left_control: "Left ⌃",
  right_control: "Right ⌃",
  left_option: "Left ⌥",
  right_option: "Right ⌥",
  left_shift: "Left ⇧",
  right_shift: "Right ⇧",
  left_command: "Left ⌘",
  right_command: "Right ⌘",
};

const CODE_LABELS = {
  Enter: "Return",
  Backspace: "Delete",
  Delete: "Forward Delete",
  PageUp: "Page Up",
  PageDown: "Page Down",
  ArrowUp: "↑",
  ArrowDown: "↓",
  ArrowLeft: "←",
  ArrowRight: "→",
  Minus: "-",
  Equal: "=",
  BracketLeft: "[",
  BracketRight: "]",
  Backslash: "\\",
  Semicolon: ";",
  Quote: "'",
  Comma: ",",
  Period: ".",
  Slash: "/",
  Backquote: "`",
};

function codeLabel(code) {
  if (/^Key[A-Z]$/.test(code)) return code.slice(3);
  if (/^Digit\d$/.test(code)) return code.slice(5);
  return CODE_LABELS[code] ?? code;
}

function shortcutLabels(shortcut) {
  if (shortcut.kind === "combo") {
    return [...shortcut.modifiers.map((key) => KEY_LABELS[key]), codeLabel(shortcut.key)];
  }
  return shortcut.keys.map((key) => KEY_LABELS[key]);
}

/** The shortcut as plain text, e.g. "⌥⌘", "Right ⌥" or "⇧⌘D". */
export function shortcutText(shortcut) {
  const labels = shortcutLabels(shortcut);
  const compact = shortcut.kind === "combo" || labels.every((label) => label.length === 1);
  return labels.join(compact ? "" : " ");
}

/** The shortcut as `<kbd>` elements separated by spaces. */
export function shortcutKeys(shortcut) {
  return shortcutLabels(shortcut).flatMap((label, index) => [
    ...(index ? [" "] : []),
    el("kbd", {}, label),
  ]);
}

/** "Tap" for modifier keys on their own, "Press" for a combination. */
export function shortcutVerb(shortcut) {
  return shortcut.kind === "tap" ? "Tap" : "Press";
}

/** Renders the blocks of a plugin card (see `plugin_api::Block`). */
export function cardBlocks(card) {
  return (card?.blocks ?? []).map((block) =>
    block.kind === "change"
      ? el(
          "div",
          { className: "block-change" },
          el("del", {}, block.before),
          el("span", { className: "block-arrow" }, "→"),
          el("ins", {}, block.after),
        )
      : el("div", { className: "block-text" }, block.text),
  );
}

/** All text in a plugin card, for search. */
export function cardText(card) {
  return (card?.blocks ?? [])
    .flatMap((block) => (block.kind === "change" ? [block.before, block.after] : [block.text]))
    .join(" ");
}

export function formatDuration(ms) {
  const total = Math.max(0, Math.floor(ms / 1000));
  const minutes = String(Math.floor(total / 60)).padStart(2, "0");
  const seconds = String(total % 60).padStart(2, "0");
  return `${minutes}:${seconds}`;
}

export function toast(message, { error = false, duration = 2200 } = {}) {
  const node = el("div", { className: error ? "toast error" : "toast" }, message);
  $("toasts").append(node);
  setTimeout(() => {
    node.classList.add("leaving");
    setTimeout(() => node.remove(), 200);
  }, duration);
}

export function shake(node) {
  node.classList.remove("shake");
  void node.offsetWidth;
  node.classList.add("shake");
}

export async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    toast("Copied to clipboard");
  } catch {
    toast("Could not copy", { error: true });
  }
}
