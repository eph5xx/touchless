import { $, el, icon, invoke, listen, openSettingsTab, toast } from "./util.js";

const ELEVENLABS = "elevenlabs";
const DESCRIPTIONS = {
  whisper_large_v3_turbo: "Best accuracy.",
  whisper_medium: "Accurate, but slower than Turbo.",
  whisper_small: "Fastest.",
};

let models = [];
/** Model files from other apps, which the user picked. */
let external = [];
/** Whether the user opened or closed the custom model section; `null` until they do. */
let customOpen = null;
let whisper = { state: "unloaded" };
let settings = null;
let onSettings = () => {};
let onState = () => {};
/** The model whose Delete button waits for a second click. */
let confirmingDelete = null;
let confirmTimer = null;

/** The id of the selected engine: a built-in id, or the path of an external file. */
export function engineId(engine) {
  return typeof engine === "string" ? engine : (engine?.external ?? null);
}

export async function initModels(handlers) {
  ({ onSettings, onState } = handlers);
  await listen("models-changed", ({ payload }) => {
    models = payload;
    changed();
  });
  await listen("external-models-changed", ({ payload }) => {
    external = payload;
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
  [models, external, whisper] = await Promise.all([
    invoke("list_models"),
    invoke("list_external_models"),
    invoke("get_whisper_status"),
  ]);
  changed();
}

/** Re-checks the external files, which other apps may have moved or deleted. */
export async function refreshExternalModels() {
  external = await invoke("list_external_models");
  changed();
}

export function renderModels(next) {
  settings = next;
  render();
}

export function modelsState() {
  return { models, whisper, external };
}

function changed() {
  render();
  onState({ models, whisper, external });
}

function render() {
  if (!settings) return;
  $("models-list").replaceChildren(elevenLabsCard(), ...models.map(whisperCard), customModels());
}

/** Picked whisper.cpp files, folded away until the user opens it or has picked one. */
function customModels() {
  return el(
    "details",
    {
      className: "models-custom",
      open: customOpen ?? external.length > 0,
      ontoggle: (event) => (customOpen = event.currentTarget.open),
    },
    el("summary", {}, icon("chevron-right", "models-custom-icon"), "Use your own whisper.cpp model"),
    el(
      "div",
      { className: "models-custom-body" },
      el(
        "p",
        { className: "model-description" },
        "Pick a ggml .bin file, such as one Handy downloaded. Core ML (WhisperKit) and ONNX models " +
          "(Moonshine, Parakeet) won't work. Touchless reads the file where it is and never changes or deletes it.",
      ),
      ...external.map(externalCard),
      button("secondary small model-choose", "Choose .bin file…", (event) => chooseFile(event.currentTarget)),
    ),
  );
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
  const selected = engineId(settings.engine) === model.id;
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

function externalCard(model) {
  const selected = engineId(settings.engine) === model.id;
  const description = [model.label, model.size != null && formatBytes(model.size), model.path]
    .filter(Boolean)
    .join(" · ");
  const status = [
    model.english_only && el("span", { className: "badge" }, "English only"),
    model.error
      ? el("span", { className: "badge warning", title: model.error }, model.missing ? "Missing" : "Can't use")
      : selected && loadState(model),
  ];
  return card({
    name: model.name,
    description,
    selected,
    status,
    actions: [
      !selected &&
        !model.error &&
        button("secondary small", "Use", (event) => useEngine(event.currentTarget, { external: model.id })),
      button("ghost small", "Remove", (event) => removeExternal(event.currentTarget, model)),
    ],
  });
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
        ...[status].flat(),
        selected && el("span", { className: "badge success" }, "In use"),
        ...actions.filter(Boolean),
      ),
    ),
    selected && whisper.state === "failed" && whisper.model === engineId(settings.engine)
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

async function chooseFile(node) {
  node.disabled = true;
  try {
    const next = await invoke("add_external_model");
    if (next) onSettings(next);
  } catch (error) {
    toast(String(error), { error: true, duration: 5000 });
  } finally {
    node.disabled = false;
  }
}

async function removeExternal(node, model) {
  node.disabled = true;
  try {
    const inUse = engineId(settings.engine) === model.id;
    onSettings(await invoke("remove_external_model", { path: model.id }));
    toast(inUse ? `${model.name} removed. Switched to ElevenLabs.` : `${model.name} removed`);
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
    const inUse = engineId(settings.engine) === model.id;
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
