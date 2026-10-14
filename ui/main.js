const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const STATUS_LABELS = {
  idle: "Idle",
  connecting: "Connecting…",
  listening: "Listening",
  finishing: "Saving…",
};

const HOTKEY_WARNINGS = {
  needs_permission:
    "The ⌘⌥ hotkey needs Input Monitoring permission. Grant it in System Settings → Privacy & Security → Input Monitoring, then restart the app.",
  unavailable: "The ⌘⌥ hotkey could not be started. You can still use the button.",
};

const $ = (id) => document.getElementById(id);

let state = null;
let timerHandle = null;

function showPage(page) {
  for (const section of document.querySelectorAll(".page")) {
    section.hidden = section.id !== `page-${page}`;
  }
  for (const link of document.querySelectorAll("nav a")) {
    link.classList.toggle("active", link.dataset.page === page);
  }
  if (page === "history") loadHistory();
}

function currentPage() {
  return location.hash === "#history" ? "history" : "listen";
}

function formatDuration(ms) {
  const total = Math.max(0, Math.floor(ms / 1000));
  const minutes = String(Math.floor(total / 60)).padStart(2, "0");
  const seconds = String(total % 60).padStart(2, "0");
  return `${minutes}:${seconds}`;
}

function renderTimer() {
  const active = state && state.status !== "idle" && state.started_at;
  $("timer").textContent = active ? formatDuration(Date.now() - state.started_at) : "00:00";
}

function render(next) {
  state = next;
  const { status } = state;

  $("status-badge").textContent = STATUS_LABELS[status];
  $("status-badge").className = `badge ${status}`;
  $("nav-dot").className = `dot ${status}`;

  const toggle = $("toggle");
  toggle.textContent = status === "idle" ? "Start" : "Stop";
  toggle.classList.toggle("stop", status !== "idle");
  toggle.disabled = status === "finishing";

  $("committed").textContent = state.committed;
  $("partial").textContent = state.committed && state.partial ? ` ${state.partial}` : state.partial;
  $("transcript-empty").hidden = Boolean(state.committed || state.partial);

  $("error").hidden = !state.error;
  $("error").textContent = state.error ?? "";

  const warning = HOTKEY_WARNINGS[state.hotkey];
  $("hotkey-warning").hidden = !warning;
  $("hotkey-warning").textContent = warning ?? "";

  renderTimer();
  if (status === "idle") {
    clearInterval(timerHandle);
    timerHandle = null;
  } else if (!timerHandle) {
    timerHandle = setInterval(renderTimer, 250);
  }
}

async function loadHistory() {
  const entries = await invoke("list_transcripts");
  const list = $("history-list");
  list.replaceChildren(
    ...entries.map((entry) => {
      const item = document.createElement("li");
      const meta = document.createElement("div");
      meta.className = "meta";
      meta.textContent = `${new Date(entry.created_at).toLocaleString()} · ${formatDuration(entry.duration_ms)}`;
      const text = document.createElement("div");
      text.className = "text";
      text.textContent = entry.text;
      item.append(meta, text);
      return item;
    }),
  );
  $("history-empty").hidden = entries.length > 0;
}

$("toggle").addEventListener("click", () => invoke("toggle_recording"));
window.addEventListener("hashchange", () => showPage(currentPage()));

await listen("recorder-state", (event) => render(event.payload));
await listen("transcripts-changed", () => {
  if (currentPage() === "history") loadHistory();
});

render(await invoke("get_state"));
showPage(currentPage());
