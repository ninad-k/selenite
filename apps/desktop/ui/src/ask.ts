// "Ask the vault" panel (Ctrl+Shift+A). Shows ranked passages with clickable citations.
// Retrieval only: the app never calls a model. All ranking happens in the engine; this file
// renders what the `ask` command returns. All text goes in via textContent.

import "./panels.css";
import { api, errorText, type Citation } from "./api";
import { h, modal } from "./dom";

export interface AskHooks {
  /** Open a page in Vault mode with lines `start`..`end` selected. */
  openAt: (path: string, start: number, end: number) => void;
}

const LIMIT = 8;
let hooks: AskHooks | null = null;

export function initAsk(h2: AskHooks): void {
  hooks = h2;
}

/** The citation text an AI client is asked to use: `[path#Lstart-Lend]`. */
export function citationLabel(c: Citation): string {
  return `${c.path}#L${c.start_line}-L${c.end_line}`;
}

export function openAsk(): void {
  const m = modal("Ask the vault");
  m.body.classList.add("ask-body");
  const input = h("input", { type: "text", label: "Question" });
  input.autocomplete = "off";
  input.spellcheck = true;
  input.placeholder = "e.g. When is hotel checkout in Berlin?";
  const status = h("p", { class: "small muted", text: "" });
  status.setAttribute("role", "status");
  status.setAttribute("aria-live", "polite");
  const list = h("ol", { class: "ask-results" });
  list.setAttribute("aria-label", "Passages");
  const form = h(
    "form",
    { class: "modal-form" },
    h("label", { class: "field-label", text: "Question" }),
    input,
    h("div", { class: "row" }, h("button", { type: "submit", class: "primary", text: "Find passages" })),
    h("p", {
      class: "small muted",
      text:
        "Finds the passages in your notes that best match the question. No AI model is called " +
        "here; connect an AI client over MCP (ask_vault) to get a written answer.",
    }),
  );
  m.body.append(form, status, list);
  let token = 0;

  const render = (cites: Citation[]): void => {
    list.replaceChildren();
    for (const c of cites) {
      const open = h("button", {
        type: "button",
        class: "cite-link",
        text: `[${citationLabel(c)}]`,
        title: "Open the page at these lines",
      });
      open.addEventListener("click", () => {
        m.close();
        hooks?.openAt(c.path, c.start_line, c.end_line);
      });
      const heading = c.heading ? ` › ${c.heading}` : "";
      const item = h(
        "li",
        { class: "ask-item" },
        h("div", { class: "ask-head" }, h("strong", { text: `${c.title}${heading}` }), open),
        h("p", { class: "ask-excerpt", text: c.excerpt }),
      );
      list.append(item);
    }
  };

  form.addEventListener("submit", (e) => {
    e.preventDefault();
    const q = input.value.trim();
    const mine = ++token;
    list.replaceChildren();
    if (!q) {
      status.textContent = "Type a question first.";
      return;
    }
    status.textContent = "Searching…";
    api
      .ask(q, LIMIT)
      .then((cites) => {
        if (mine !== token) return;
        status.textContent =
          cites.length === 0
            ? "No passages matched. Try other words."
            : `${cites.length} ${cites.length === 1 ? "passage" : "passages"}, best first.`;
        render(cites);
      })
      .catch((err: unknown) => {
        if (mine === token) status.textContent = `Could not search: ${errorText(err)}`;
      });
  });
  input.focus();
}
