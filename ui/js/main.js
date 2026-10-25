import { invoke, listen, toast } from "./util.js";
import {
  initDictate,
  renderRecorder,
  renderCallouts,
  renderShortcut,
  onToggleIgnored,
} from "./dictate.js";
import { initHistory, loadHistory, focusTranscript } from "./history.js";
import { loadPlugins } from "./plugins.js";
import { initMcp, loadMcp } from "./mcp.js";
import { initSettings, renderSettings, renderPermissions } from "./settings.js";
import { initModels, renderModels, refreshExternalModels } from "./models.js";

const PAGES = ["dictate", "history", "plugins", "mcp", "settings"];
const DELIVERY_MESSAGES = {
  pasted: "Pasted into the active app",
  copied: "Copied to clipboard",
  saved: "Saved to history",
};

const app = {
  recorder: null,
  settings: null,
  permissions: null,
  models: null,
  whisper: null,
  external: null,
};
let deliveryTimer = null;

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
  if (page === "plugins") loadPlugins();
  if (page === "mcp") loadMcp();
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
initMcp();
initSettings((settings) => {
  app.settings = settings;
  renderModels(settings);
  renderShortcut();
  renderCallouts(app);
});
await initModels({
  onSettings: (settings) => {
    app.settings = settings;
    renderSettings(settings);
    renderModels(settings);
    renderCallouts(app);
  },
  onState: ({ models, whisper, external }) => {
    app.models = models;
    app.whisper = whisper;
    app.external = external;
    renderCallouts(app);
  },
});

window.addEventListener("hashchange", () => showPage(currentPage()));
// Permissions are granted in System Settings, so re-check when the user comes back.
window.addEventListener("focus", refreshPermissions);
// Apps may have been installed or reconfigured while Touchless was in the background,
// and may have moved or deleted model files Touchless uses.
window.addEventListener("focus", () => {
  if (currentPage() === "mcp") loadMcp();
  refreshExternalModels();
});

await listen("recorder-state", (event) => {
  app.recorder = event.payload;
  if (app.recorder.status === "connecting" || app.recorder.status === "listening") {
    clearDeliveryStatus();
  }
  renderRecorder(app.recorder);
  renderCallouts(app);
});
await listen("toggle-ignored", onToggleIgnored);
await listen("navigate", ({ payload }) => {
  if (PAGES.includes(payload)) location.hash = `#${payload}`;
});
await listen("open-transcript", ({ payload }) => focusTranscript(payload));
await listen("transcripts-changed", () => {
  if (currentPage() === "history") loadHistory();
});
// Also sent when the OpenAI connection changes, including a ChatGPT session ending.
await listen("plugins-changed", async () => {
  if (currentPage() === "plugins") loadPlugins();
  app.settings = await invoke("get_settings");
  renderSettings(app.settings);
  renderModels(app.settings);
  renderCallouts(app);
});
await listen("dictation-finished", ({ payload }) => {
  if (!payload.delivery) return;
  const message = DELIVERY_MESSAGES[payload.delivery];
  showDeliveryStatus(message);
  if (document.hasFocus()) toast(message);
});

function showDeliveryStatus(message) {
  const status = document.getElementById("delivery-status");
  if (!status) return;
  clearTimeout(deliveryTimer);
  status.textContent = message;
  status.hidden = false;
  deliveryTimer = setTimeout(clearDeliveryStatus, 7000);
}

function clearDeliveryStatus() {
  const status = document.getElementById("delivery-status");
  if (status) status.hidden = true;
  clearTimeout(deliveryTimer);
  deliveryTimer = null;
}

app.recorder = await invoke("get_state");
app.settings = await invoke("get_settings");
renderRecorder(app.recorder);
renderSettings(app.settings);
renderModels(app.settings);
await refreshPermissions();
showPage(currentPage());
