import {
  $,
  el,
  icon,
  ICONS,
  invoke,
  formatDuration,
  copyText,
  toast,
  shortcutKeys,
  shortcutVerb,
} from "./util.js";

let entries = [];
let getShortcut = () => null;

export function initHistory(shortcut) {
  getShortcut = shortcut;
  $("history-search").addEventListener("input", render);
}

export async function loadHistory() {
  try {
    entries = await invoke("list_transcripts");
  } catch (error) {
    toast(`Could not load history: ${error}`, { error: true });
    entries = [];
  }
  render();
}

function dayLabel(timestamp) {
  const date = new Date(timestamp);
  const today = new Date();
  const yesterday = new Date();
  yesterday.setDate(today.getDate() - 1);
  if (date.toDateString() === today.toDateString()) return "Today";
  if (date.toDateString() === yesterday.toDateString()) return "Yesterday";
  return date.toLocaleDateString(undefined, {
    weekday: "long",
    month: "long",
    day: "numeric",
    year: date.getFullYear() === today.getFullYear() ? undefined : "numeric",
  });
}

function render() {
  const query = $("history-search").value.trim().toLowerCase();
  const visible = query
    ? entries.filter((entry) => entry.text.toLowerCase().includes(query))
    : entries;

  const nodes = [];
  let currentDay = null;
  for (const entry of visible) {
    const day = dayLabel(entry.created_at);
    if (day !== currentDay) {
      currentDay = day;
      nodes.push(el("div", { className: "day" }, day));
    }
    nodes.push(renderEntry(entry));
  }

  $("history-list").replaceChildren(...nodes);
  $("history-empty").hidden = visible.length > 0;
  $("history-empty-text").replaceChildren(...emptyText(query));
}

function emptyText(query) {
  if (query) return ["No transcripts match your search."];
  const shortcut = getShortcut();
  if (!shortcut) return ["No dictations yet."];
  return [
    `No dictations yet. ${shortcutVerb(shortcut)} `,
    ...shortcutKeys(shortcut),
    " to record your first one.",
  ];
}

function renderEntry(entry) {
  const time = new Date(entry.created_at).toLocaleTimeString(undefined, {
    hour: "numeric",
    minute: "2-digit",
  });
  const words = entry.text.split(/\s+/).filter(Boolean).length;

  return el(
    "article",
    { className: "card entry" },
    el(
      "div",
      { className: "entry-meta" },
      el("span", {}, `${time} · ${formatDuration(entry.duration_ms)} · ${words} ${words === 1 ? "word" : "words"}`),
      el(
        "div",
        { className: "entry-actions" },
        el(
          "button",
          { className: "ghost small", type: "button", title: "Copy", onclick: () => copyText(entry.text) },
          icon(...ICONS.copy),
          "Copy",
        ),
        el(
          "button",
          { className: "ghost small danger", type: "button", title: "Delete", onclick: () => remove(entry) },
          icon(...ICONS.trash),
        ),
      ),
    ),
    el("div", { className: "entry-text" }, entry.text),
  );
}

async function remove(entry) {
  try {
    await invoke("delete_transcript", { id: entry.id });
    entries = entries.filter((other) => other.id !== entry.id);
    render();
    toast("Transcript deleted");
  } catch (error) {
    toast(`Could not delete: ${error}`, { error: true });
  }
}
