import { el } from "./util.js";
import { searchLanguages } from "./languages.js";

let input = null;
let list = null;
let onPick = () => {};
let picked = [];
let results = [];
let active = 0;

export function initLanguagePicker(searchInput, resultsList, options) {
  input = searchInput;
  list = resultsList;
  onPick = options.onPick;

  input.addEventListener("focus", open);
  input.addEventListener("input", () => {
    active = 0;
    open();
  });
  input.addEventListener("blur", close);
  input.addEventListener("keydown", (event) => {
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      if (list.hidden) return open();
      if (!results.length) return;
      const step = event.key === "ArrowDown" ? 1 : -1;
      highlight((active + step + results.length) % results.length, true);
    } else if (event.key === "Enter") {
      event.preventDefault();
      if (!list.hidden && results[active]) pick(results[active][0]);
    } else if (event.key === "Escape" && !list.hidden) {
      event.preventDefault();
      close();
    }
  });
  // Keeps focus in the search box when a result is clicked.
  list.addEventListener("mousedown", (event) => event.preventDefault());
}

/** Updates which languages are already picked, re-filtering an open list. */
export function refreshLanguagePicker(codes) {
  picked = codes;
  if (!list.hidden) render();
}

function open() {
  list.hidden = false;
  input.setAttribute("aria-expanded", "true");
  render();
}

function close() {
  list.hidden = true;
  input.setAttribute("aria-expanded", "false");
  input.removeAttribute("aria-activedescendant");
}

function pick(code) {
  input.value = "";
  active = 0;
  onPick(code);
  render();
}

function render() {
  results = searchLanguages(input.value, picked);
  active = Math.min(active, Math.max(results.length - 1, 0));

  if (!results.length) {
    input.removeAttribute("aria-activedescendant");
    const message = input.value.trim()
      ? `No languages match “${input.value.trim()}”`
      : "You've added every language";
    list.replaceChildren(el("li", { className: "language-empty" }, message));
    return;
  }

  list.replaceChildren(
    ...results.map(([code, name], index) =>
      el(
        "li",
        {
          id: `language-option-${code}`,
          role: "option",
          onclick: () => pick(code),
          onmousemove: () => highlight(index, false),
        },
        name,
      ),
    ),
  );
  highlight(active, true);
}

function highlight(index, scroll) {
  active = index;
  for (const [position, option] of [...list.children].entries()) {
    option.classList.toggle("active", position === index);
    option.setAttribute("aria-selected", String(position === index));
  }
  const current = list.children[index];
  input.setAttribute("aria-activedescendant", current.id);
  if (scroll) current.scrollIntoView({ block: "nearest" });
}
