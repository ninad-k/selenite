// Small DOM helpers shared by the Workspace modules. All text goes in via textContent.

export function $<T extends HTMLElement>(id: string): T {
  const el = document.getElementById(id);
  if (!el) throw new Error(`missing element #${id}`);
  return el as T;
}

export interface HProps {
  class?: string;
  text?: string;
  type?: string;
  title?: string;
  label?: string;
  role?: string;
  attrs?: Record<string, string>;
}

export function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  props: HProps = {},
  ...children: (Node | string)[]
): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  if (props.class) el.className = props.class;
  if (props.text !== undefined) el.textContent = props.text;
  if (props.type) el.setAttribute("type", props.type);
  if (props.title) el.title = props.title;
  if (props.label) el.setAttribute("aria-label", props.label);
  if (props.role) el.setAttribute("role", props.role);
  if (props.attrs) for (const [k, v] of Object.entries(props.attrs)) el.setAttribute(k, v);
  for (const c of children) el.append(c);
  return el;
}

export function button(text: string, onClick: () => void, cls = "", label?: string): HTMLButtonElement {
  const b = h("button", { type: "button", text, class: cls });
  if (label) b.setAttribute("aria-label", label);
  b.addEventListener("click", onClick);
  return b;
}

export interface ModalHandle {
  body: HTMLElement;
  close: () => void;
}

const FOCUSABLE =
  'button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

/** A modal dialog with a focus trap. Escape closes it; focus returns to the opener. */
export function modal(title: string, onClose?: () => void): ModalHandle {
  const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  const titleId = `modal-title-${Math.random().toString(36).slice(2, 8)}`;
  const body = h("div", { class: "modal-body" });
  const heading = h("h2", { class: "dialog-title", text: title });
  heading.id = titleId;
  const dialog = h("div", { class: "dialog modal", role: "dialog" }, heading, body);
  dialog.setAttribute("aria-modal", "true");
  dialog.setAttribute("aria-labelledby", titleId);
  const overlay = h("div", { class: "overlay modal-overlay" }, dialog);
  let closed = false;
  const close = (): void => {
    if (closed) return;
    closed = true;
    overlay.remove();
    opener?.focus();
    onClose?.();
  };
  overlay.addEventListener("mousedown", (e) => {
    if (e.target === overlay) close();
  });
  overlay.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      close();
    } else if (e.key === "Tab") {
      const items = [...dialog.querySelectorAll<HTMLElement>(FOCUSABLE)].filter((x) => !x.hidden);
      const first = items[0];
      const last = items[items.length - 1];
      if (!first || !last) return;
      if (e.shiftKey && document.activeElement === first) {
        e.preventDefault();
        last.focus();
      } else if (!e.shiftKey && document.activeElement === last) {
        e.preventDefault();
        first.focus();
      }
    }
  });
  document.body.append(overlay);
  return { body, close };
}

/** Ask for one line of text. Resolves to null when cancelled. */
export function promptText(
  title: string,
  label: string,
  initial = "",
  hint = "",
): Promise<string | null> {
  return new Promise((resolve) => {
    let result: string | null = null;
    const m = modal(title, () => resolve(result));
    const input = h("input", { type: "text", label });
    input.value = initial;
    input.autocomplete = "off";
    input.spellcheck = false;
    const form = h(
      "form",
      { class: "modal-form" },
      h("label", { class: "field-label", text: label }),
      input,
    );
    if (hint) form.append(h("p", { class: "small muted", text: hint }));
    const ok = h("button", { type: "submit", class: "primary", text: "OK" });
    form.append(h("div", { class: "row" }, ok, button("Cancel", () => m.close())));
    form.addEventListener("submit", (e) => {
      e.preventDefault();
      result = input.value;
      m.close();
    });
    m.body.append(form);
    input.focus();
    input.select();
  });
}

export interface MenuItem {
  label: string;
  hint?: string;
  run: () => void;
}

/** Keyboard-navigable popup menu anchored below an element. Returns a close function. */
export function popupMenu(anchor: HTMLElement, items: MenuItem[], label: string): () => void {
  const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  const menu = h("div", { class: "popup-menu", role: "menu", label });
  const buttons: HTMLButtonElement[] = [];
  let closed = false;
  const close = (restore = true): void => {
    if (closed) return;
    closed = true;
    menu.remove();
    document.removeEventListener("mousedown", away, true);
    if (restore) opener?.focus();
  };
  const away = (e: MouseEvent): void => {
    if (!menu.contains(e.target as Node)) close(false);
  };
  for (const it of items) {
    const b = h("button", { type: "button", role: "menuitem", class: "menu-item" });
    b.append(h("span", { text: it.label }));
    if (it.hint) b.append(h("span", { class: "muted small", text: it.hint }));
    b.addEventListener("click", () => {
      close();
      it.run();
    });
    buttons.push(b);
    menu.append(b);
  }
  menu.addEventListener("keydown", (e) => {
    const i = buttons.indexOf(document.activeElement as HTMLButtonElement);
    if (e.key === "ArrowDown") {
      e.preventDefault();
      buttons[(i + 1) % buttons.length]?.focus();
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      buttons[(i - 1 + buttons.length) % buttons.length]?.focus();
    } else if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      close();
    } else if (e.key === "Tab") {
      e.preventDefault();
    }
  });
  document.body.append(menu);
  const r = anchor.getBoundingClientRect();
  menu.style.left = `${Math.max(4, Math.min(r.left, window.innerWidth - 220))}px`;
  menu.style.top = `${Math.min(r.bottom + 2, window.innerHeight - 40)}px`;
  document.addEventListener("mousedown", away, true);
  buttons[0]?.focus();
  return close;
}
