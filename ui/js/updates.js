import { $, invoke, toast } from "./util.js";

let view = null;

export function initUpdates() {
  $("update-action").addEventListener("click", async (event) => {
    const button = event.currentTarget;
    button.disabled = true;
    try {
      renderUpdates(
        await invoke(view?.status === "available" ? "install_update" : "check_for_updates"),
      );
    } catch (error) {
      toast(String(error), { error: true, duration: 4000 });
      renderUpdates(view);
    }
  });
  $("update-notes-link").addEventListener("click", () =>
    invoke("open_release_page").catch((error) => toast(String(error), { error: true })),
  );
  $("app-version").addEventListener("click", () => {
    if (!view?.version) return;
    // Wait for the hash change to show the Settings page first.
    requestAnimationFrame(() => $("updates-title").scrollIntoView({ behavior: "smooth" }));
  });
}

export function renderUpdates(next) {
  view = next;
  if (!view) return;
  renderCard();
  renderSidebar();
}

function statusText() {
  switch (view.status) {
    case "checking":
      return "Checking…";
    case "up_to_date":
      return "Up to date";
    case "available":
      return `Version ${view.version} is available`;
    case "downloading":
      return view.percent == null ? "Downloading…" : `Downloading ${view.percent}%`;
    case "ready":
      return `Installing version ${view.version}. Touchless restarts when you finish dictating.`;
    case "error":
      return view.message;
    default:
      return "Touchless looks for new versions on GitHub.";
  }
}

function renderCard() {
  $("update-current").textContent = view.current_version;
  $("update-status").textContent = statusText();
  $("update-status").classList.toggle("error-text", view.status === "error");

  const action = $("update-action");
  const busy = ["checking", "downloading", "ready"].includes(view.status);
  action.disabled = busy;
  action.className = view.status === "available" ? "small" : "secondary small";
  action.textContent = {
    available: "Install and restart",
    checking: "Checking…",
    downloading: "Installing…",
    ready: "Installing…",
  }[view.status] ?? "Check now";

  $("update-notes-link").hidden = !view.version;
  $("update-notes").hidden = view.status !== "available" || !view.notes;
  $("update-notes").textContent = view.notes ?? "";
}

function renderSidebar() {
  const link = $("app-version");
  const offered = Boolean(view.version);
  link.classList.toggle("update", offered);
  link.textContent = offered ? "Update available" : `v${view.current_version}`;
  link.title = offered ? `Version ${view.version} is ready to install` : "";
}
