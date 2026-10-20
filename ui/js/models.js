import { $, el, invoke, listen, openSettingsTab, toast } from "./util.js";

const ELEVENLABS = "elevenlabs";
const DESCRIPTIONS = {
  whisper_large_v3_turbo: "Best accuracy.",
  whisper_medium: "Accurate, but slower than Turbo.",
  whisper_small: "Fastest.",
};

let models = [];
let whisper = { state: "unloaded" };
let settings = null;
let onSettings = () => {};
let onState = () => {};
/** The model whose Delete button waits for a second click. */
let confirmingDelete = null;
let confirmTimer = null;

export async function initModels(handlers) {
  ({ onSettings, onState } = handlers);
  await listen("models-changed", ({ payload }) => {
    models = payload;
    changed();
  });
  await listen("model-download-progress", ({ payload }) => {
    const model = models.find((item) => item.id === payload.id);
    if (!model || model.state !== "downloading") return;
    model.received = payload.received;
    model.total = payload.total;
    updateProgress(model);
  });
  await listen("model-download-failed", ({ payload }) => {
    toast(`Download failed: ${payload.error}`, { error: true, duration: 5000 });
  });
  await listen("whisper-status", ({ payload }) => {
    whisper = payload;
    changed();
  });
  [models, whisper] = await Promise.all([invoke("list_models"), invoke("get_whisper_status")]);
  changed();
}

export function renderModels(next) {
  settings = next;
  render();
}

export function modelsState() {
  return { models, whisper };
}

function changed() {
  render();
  onState({ models, whisper });
}

function render() {
  if (!settings) return;
  $("models-list").replaceChildren(elevenLabsCard(), ...models.map(whisperCard));
}

function elevenLabsCard() {
  const selected = settings.engine === ELEVENLABS;
  const keySaved = settings.api_keys.elevenlabs.saved;
  return card({
    name: "ElevenLabs Scribe",
    description: "Runs in the cloud and shows words as you speak. Needs an API key.",
    selected,
    status: keySaved ? null : el("span", { className: "badge warning" }, "No API key"),
    actions: [
      !keySaved &&
        button("secondary small", "Add key", () => openSettingsTab("services")),
      !selected && button("secondary small", "Use", (event) => useEngine(event.currentTarget, ELEVENLABS)),
    ],
  });
}

function whisperCard(model) {
  const selected = settings.engine === model.id;
  const description = `${formatBytes(model.size)} · ${DESCRIPTIONS[model.id] ?? ""}`;
  const actions = [];
  let status = null;
  let extra = null;

  if (model.state === "not_downloaded") {
    actions.push(button("secondary small", "Download", (event) => download(event.currentTarget, model)));
  } else if (model.state === "downloading") {
    actions.push(button("ghost small", "Cancel", () => invoke("cancel_model_download", { model: model.id })));
    extra = el(
      "div",
      { className: "model-progress" },
      el("div", { className: "progress-track" }, el("div", { className: "progress-fill", id: `progress-${model.id}` })),
      el("span", { className: "progress-text", id: `progress-text-${model.id}` }),
    );
  } else {
    if (selected) status = loadState(model);
    else actions.push(button("secondary small", "Use", (event) => useEngine(event.currentTarget, model.id)));
    const confirming = confirmingDelete === model.id;
    actions.push(
      button("ghost small danger", confirming ? "Confirm delete" : "Delete", (event) =>
        confirming ? remove(event.currentTarget, model) : askToDelete(model),
      ),
    );
  }

  const node = card({ name: model.name, description, selected, status, actions, extra });
  if (model.state === "downloading") updateProgress(model, node);
  return node;
}

/** The badge for the selected local model: loading, failed or ready. */
function loadState(model) {
  if (whisper.model === model.id && whisper.state === "loading") {
    return el("span", { className: "badge" }, "Loading…");
  }
  if (whisper.model === model.id && whisper.state === "failed") {
    return el("span", { className: "badge warning", title: whisper.error }, "Could not load");
  }
  return null;
}

function card({ name, description, selected, status, actions, extra }) {
  return el(
    "article",
    { className: selected ? "model-card selected" : "model-card", role: "radio", ariaChecked: String(selected) },
    el(
      "div",
      { className: "model-main" },
      el(
        "div",
        { className: "model-text" },
        el("div", { className: "model-name" }, name),
        el("p", { className: "model-description" }, description),
      ),
      el(
        "div",
        { className: "model-actions" },
        status,
        selected && el("span", { className: "badge success" }, "In use"),
        ...actions.filter(Boolean),
      ),
    ),
    selected && whisper.state === "failed" && whisper.model === settings.engine
      ? el("p", { className: "model-error" }, whisper.error)
      : null,
    extra,
  );
}

function button(className, label, onclick) {
  return el("button", { className, type: "button", onclick }, label);
}

function updateProgress(model, root = document) {
  const fill = root.querySelector(`#progress-${model.id}`);
  const label = root.querySelector(`#progress-text-${model.id}`);
  if (!fill || !label) return;
  const share = model.total ? model.received / model.total : 0;
  fill.style.width = `${(share * 100).toFixed(1)}%`;
  label.textContent = `${formatBytes(model.received)} of ${formatBytes(model.total)}`;
}

async function download(node, model) {
  node.disabled = true;
  try {
    models = await invoke("download_model", { model: model.id });
    changed();
  } catch (error) {
    toast(String(error), { error: true, duration: 4000 });
    node.disabled = false;
  }
}

async function useEngine(node, engine) {
  node.disabled = true;
  try {
    onSettings(await invoke("set_engine", { engine }));
  } catch (error) {
    toast(String(error), { error: true, duration: 4000 });
    node.disabled = false;
  }
}

function askToDelete(model) {
  confirmingDelete = model.id;
  clearTimeout(confirmTimer);
  confirmTimer = setTimeout(() => {
    confirmingDelete = null;
    render();
  }, 4000);
  render();
}

async function remove(node, model) {
  confirmingDelete = null;
  clearTimeout(confirmTimer);
  node.disabled = true;
  try {
    const inUse = settings.engine === model.id;
    onSettings(await invoke("delete_model", { model: model.id }));
    toast(inUse ? `${model.name} deleted. Switched to ElevenLabs.` : `${model.name} deleted`);
  } catch (error) {
    toast(String(error), { error: true, duration: 4000 });
    render();
  }
}

function formatBytes(bytes) {
  if (bytes >= 1e9) return `${(bytes / 1e9).toFixed(1)} GB`;
  return `${Math.round(bytes / 1e6)} MB`;
}
