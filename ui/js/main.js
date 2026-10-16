import { invoke, listen, toast } from "./util.js";
import {
  initDictate,
  renderRecorder,
  renderCallouts,
  renderShortcut,
  onToggleIgnored,
} from "./dictate.js";
import { initHistory, loadHistory } from "./history.js";
import { initSettings, renderSettings, renderPermissions } from "./settings.js";

const PAGES = ["dictate", "history", "settings"];
const DELIVERY_MESSAGES = {
  pasted: "Pasted into the active app",
  copied: "Copied to clipboard",
  saved: "Saved to history",
};

const app = { recorder: null, settings: null, permissions: null };

function currentPage() {
  const page = location.hash.slice(1);
  return PAGES.includes(page) ? page : "dictate";
}

function showPage(page) {
  for (const section of document.querySelectorAll(".page")) {
    section.hidden = section.id !== `page-${page}`;
  }
  for (const link of document.querySelectorAll("nav a")) {
    link.classList.toggle("active", link.dataset.page === page);
  }
  if (page === "history") loadHistory();
  if (page === "settings") refreshPermissions();
}

async function refreshPermissions() {
  app.permissions = await invoke("get_permissions");
  renderPermissions(app.permissions);
  renderCallouts(app);
}

const getShortcut = () => app.settings?.shortcut ?? null;
initDictate(getShortcut);
initHistory(getShortcut);
initSettings((settings) => {
  app.settings = settings;
  renderShortcut();
  renderCallouts(app);
});

window.addEventListener("hashchange", () => showPage(currentPage()));
// Permissions are granted in System Settings, so re-check when the user comes back.
window.addEventListener("focus", refreshPermissions);

await listen("recorder-state", (event) => {
  app.recorder = event.payload;
  renderRecorder(app.recorder);
  renderCallouts(app);
});
await listen("toggle-ignored", onToggleIgnored);
await listen("navigate", ({ payload }) => {
  if (PAGES.includes(payload)) location.hash = `#${payload}`;
});
await listen("transcripts-changed", () => {
  if (currentPage() === "history") loadHistory();
});
await listen("dictation-finished", ({ payload }) => {
  if (payload.delivery && document.hasFocus()) toast(DELIVERY_MESSAGES[payload.delivery]);
});

app.recorder = await invoke("get_state");
app.settings = await invoke("get_settings");
renderRecorder(app.recorder);
renderSettings(app.settings);
await refreshPermissions();
showPage(currentPage());
