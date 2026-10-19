import {
  $,
  el,
  icon,
  invoke,
  formatDuration,
  copyText,
  cardBlocks,
  toast,
  shortcutKeys,
  shortcutVerb,
} from "./util.js";

const PAGE_SIZE = 50;
const SEARCH_DELAY_MS = 200;
const DAY_MS = 24 * 60 * 60 * 1000;

let entries = [];
let nextCursor = null;
let total = 0;
let loadingMore = false;
// Bumped by every new first-page load so older responses are dropped.
let generation = 0;
let getShortcut = () => null;
let activeFilter = "all";
let searchTimer = null;
let loadMoreObserver = null;
let measureFrame = 0;
const expandedEntries = new Set();

export function initHistory(shortcut) {
  getShortcut = shortcut;
  $("history-search").addEventListener("input", () => {
    clearTimeout(searchTimer);
    searchTimer = setTimeout(loadHistory, SEARCH_DELAY_MS);
  });

  const filters = $("history-filters");
  filters?.addEventListener("click", (event) => {
    const button = event.target.closest("[data-history-filter]");
    if (!button || !filters.contains(button)) return;

    activeFilter = button.dataset.historyFilter;
    renderFilters();
    loadHistory();
  });

  loadMoreObserver = new IntersectionObserver(
    (observed) => {
      if (observed.some((entry) => entry.isIntersecting)) loadMore();
    },
    { root: document.querySelector("main"), rootMargin: "0px 0px 600px 0px" },
  );
  loadMoreObserver.observe($("history-more"));

  // Whether a preview is cut off depends on the width it gets.
  new ResizeObserver(scheduleMeasure).observe($("history-list"));
}

function currentQuery() {
  const search = $("history-search").value.trim();
  let from = null;
  if (activeFilter === "today") {
    const midnight = new Date();
    midnight.setHours(0, 0, 0, 0);
    from = midnight.getTime();
  } else if (activeFilter === "week") {
    from = Date.now() - 7 * DAY_MS;
  }
  return {
    search: search || null,
    from,
    with_insights: activeFilter === "insights",
    limit: PAGE_SIZE,
  };
}

export async function loadHistory() {
  clearTimeout(searchTimer);
  const request = ++generation;
  const query = currentQuery();
  let page;
  try {
    page = await invoke("list_transcripts", { query: { ...query, cursor: null } });
  } catch (error) {
    if (request !== generation) return;
    toast(`Could not load history: ${error}`, { error: true });
    page = { transcripts: [], next_cursor: null, total: 0 };
  }
  if (request !== generation) return;
  entries = page.transcripts;
  nextCursor = page.next_cursor;
  total = page.total ?? entries.length;
  loadingMore = false;
  render(query);
}

async function loadMore() {
  if (!nextCursor || loadingMore) return;
  loadingMore = true;
  const request = generation;
  const query = currentQuery();
  try {
    const page = await invoke("list_transcripts", { query: { ...query, cursor: nextCursor } });
    if (request !== generation) return;
    const known = new Set(entries.map((entry) => entry.id));
    entries = entries.concat(page.transcripts.filter((entry) => !known.has(entry.id)));
    nextCursor = page.next_cursor;
    render(query);
  } catch (error) {
    if (request === generation) toast(`Could not load more history: ${error}`, { error: true });
  } finally {
    if (request === generation) loadingMore = false;
  }
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

function render(query = currentQuery()) {
  const nodes = [];
  let currentDay = null;
  for (const entry of entries) {
    const day = dayLabel(entry.created_at);
    if (day !== currentDay) {
      currentDay = day;
      nodes.push(el("div", { className: "day" }, day));
    }
    nodes.push(renderEntry(entry));
  }

  $("history-list").replaceChildren(...nodes);
  $("history-empty").hidden = entries.length > 0;
  $("history-empty-text").replaceChildren(...emptyText(query));
  renderCount();
  renderFilters();
  scheduleMeasure();

  // Re-observing reports the marker's current position, so another page loads
  // if this one did not fill the window.
  loadMoreObserver.unobserve($("history-more"));
  loadMoreObserver.observe($("history-more"));
}

function renderCount() {
  const count = $("history-count");
  if (count) count.textContent = `${total} ${total === 1 ? "transcript" : "transcripts"}`;
}

function renderFilters() {
  document.querySelectorAll("[data-history-filter]").forEach((button) => {
    const selected = button.dataset.historyFilter === activeFilter;
    button.classList.toggle("active", selected);
    button.setAttribute("aria-pressed", String(selected));
  });
}

function emptyText(query) {
  if (query.search) return ["No transcripts match your search."];
  if (activeFilter !== "all") return ["No transcripts match this filter."];
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
  const results = entry.plugin_results ?? [];
  const expanded = expandedEntries.has(entry.id);
  const detailsId = `history-entry-${entry.id}-details`;

  const row = el(
    "article",
    { className: `history-row${expanded ? " expanded" : ""}`, dataset: { id: entry.id } },
    el(
      "div",
      { className: "history-row-header" },
      el(
        "div",
        { className: "history-row-meta" },
        el("span", {}, `${time} · ${formatDuration(entry.duration_ms)}`),
        results.length
          ? el(
              "span",
              { className: "history-insight-count" },
              `${results.length} ${results.length === 1 ? "insight" : "insights"}`,
            )
          : null,
      ),
      el(
        "div",
        { className: "history-row-actions" },
        el(
          "button",
          {
            className: "ghost small history-disclosure",
            type: "button",
            title: expanded ? "Collapse transcript" : "Expand transcript",
            ariaLabel: expanded ? "Collapse transcript" : "Expand transcript",
            "aria-expanded": String(expanded),
            "aria-controls": detailsId,
            // Shown once measuring finds hidden text, or right away for insights.
            hidden: !expanded && results.length === 0,
            onclick: (event) => {
              event.stopPropagation();
              toggleExpanded(entry, row);
            },
          },
          icon("chevron-down", "history-disclosure-icon"),
          expanded ? "Less" : "Details",
        ),
        el(
          "button",
          {
            className: "ghost small",
            type: "button",
            title: "Copy transcript",
            ariaLabel: "Copy transcript",
            onclick: (event) => {
              event.stopPropagation();
              copyText(entry.text);
            },
          },
          icon("copy"),
          "Copy",
        ),
        el(
          "button",
          {
            className: "ghost small danger",
            type: "button",
            title: "Delete transcript",
            ariaLabel: "Delete transcript",
            onclick: (event) => {
              event.stopPropagation();
              remove(entry);
            },
          },
          icon("trash-2"),
        ),
      ),
    ),
    el("div", { className: "history-row-preview" }, entry.text),
    el(
      "div",
      { className: "history-row-details", id: detailsId, hidden: !expanded },
      ...results.map(renderResult),
    ),
  );
  row.dataset.insights = String(results.length > 0);
  return row;
}

function scheduleMeasure() {
  cancelAnimationFrame(measureFrame);
  measureFrame = requestAnimationFrame(() => {
    for (const row of $("history-list").querySelectorAll(".history-row")) measure(row);
  });
}

/** Shows Details only when the row hides something: clipped text or insights. */
function measure(row) {
  const button = row.querySelector(".history-disclosure");
  if (row.classList.contains("expanded") || row.dataset.insights === "true") {
    button.hidden = false;
    return;
  }
  const preview = row.querySelector(".history-row-preview");
  button.hidden = preview.scrollHeight <= preview.clientHeight;
}

function toggleExpanded(entry, row) {
  if (expandedEntries.has(entry.id)) expandedEntries.delete(entry.id);
  else expandedEntries.add(entry.id);
  const next = renderEntry(entry);
  row.replaceWith(next);
  measure(next);
  next.querySelector(".history-disclosure").focus();
}

function renderResult(result) {
  return el(
    "div",
    { className: `entry-plugin ${result.card?.tone ?? ""}` },
    el("div", { className: "entry-plugin-name" }, result.plugin_name),
    ...cardBlocks(result.card),
  );
}

async function remove(entry) {
  try {
    await invoke("delete_transcript", { id: entry.id });
    if (entries.some((other) => other.id === entry.id)) {
      entries = entries.filter((other) => other.id !== entry.id);
      total = Math.max(0, total - 1);
    }
    expandedEntries.delete(entry.id);
    render();
    toast("Transcript deleted");
  } catch (error) {
    toast(`Could not delete: ${error}`, { error: true });
  }
}
