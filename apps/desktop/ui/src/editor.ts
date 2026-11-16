import { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands";
import { markdown } from "@codemirror/lang-markdown";
import { syntaxTree } from "@codemirror/language";
import { highlightSelectionMatches, searchKeymap } from "@codemirror/search";
import { closeBrackets, closeBracketsKeymap } from "@codemirror/autocomplete";
import { EditorState, Prec, Range } from "@codemirror/state";
import {
  Decoration,
  type DecorationSet,
  EditorView,
  MatchDecorator,
  ViewPlugin,
  type ViewUpdate,
  keymap,
} from "@codemirror/view";

export interface EditorCallbacks {
  /** Document text changed because of the user. */
  onChange: () => void;
  /** Ctrl/Cmd+click on a wikilink; receives the link target without alias or heading. */
  onLinkClick: (target: string) => void;
}

export interface EditorHandle {
  setText: (text: string) => void;
  getText: () => string;
  focus: () => void;
}

const WIKILINK = /!?\[\[([^\]\n]+)\]\]/g;

/** Target part of `Target`, `Target|alias` or `Target#Heading`. */
export function wikilinkTarget(inner: string): string {
  const beforeAlias = inner.split("|")[0] ?? "";
  const beforeHeading = beforeAlias.split("#")[0] ?? "";
  return beforeHeading.trim();
}

const linkMark = Decoration.mark({ class: "cm-wikilink" });
const tagMark = Decoration.mark({ class: "cm-hashtag" });

const wikilinkMatcher = new MatchDecorator({ regexp: WIKILINK, decoration: linkMark });
const tagMatcher = new MatchDecorator({
  regexp: /(?<![\p{L}\p{N}_&#/])#[\p{L}_][\p{L}\p{N}_/-]*/gu,
  decoration: tagMark,
});

function matchPlugin(matcher: MatchDecorator) {
  return ViewPlugin.fromClass(
    class {
      decorations: DecorationSet;
      constructor(view: EditorView) {
        this.decorations = matcher.createDeco(view);
      }
      update(u: ViewUpdate) {
        this.decorations = matcher.updateDeco(u, this.decorations);
      }
    },
    { decorations: (v) => v.decorations },
  );
}

const nodeClasses: Record<string, string> = {
  ATXHeading1: "cm-h1",
  ATXHeading2: "cm-h2",
  ATXHeading3: "cm-h3",
  ATXHeading4: "cm-h4",
  ATXHeading5: "cm-h4",
  ATXHeading6: "cm-h4",
  SetextHeading1: "cm-h1",
  SetextHeading2: "cm-h2",
  StrongEmphasis: "cm-strong",
  Emphasis: "cm-em",
  InlineCode: "cm-code",
  FencedCode: "cm-codeblock",
  Blockquote: "cm-quote",
  HeaderMark: "cm-mark-dim",
  ListMark: "cm-mark-dim",
  QuoteMark: "cm-mark-dim",
};

const markdownStyle = ViewPlugin.fromClass(
  class {
    decorations: DecorationSet;
    constructor(view: EditorView) {
      this.decorations = this.build(view);
    }
    update(u: ViewUpdate) {
      if (u.docChanged || u.viewportChanged) this.decorations = this.build(u.view);
    }
    build(view: EditorView): DecorationSet {
      const out: Range<Decoration>[] = [];
      for (const { from, to } of view.visibleRanges) {
        syntaxTree(view.state).iterate({
          from,
          to,
          enter: (node) => {
            const cls = nodeClasses[node.name];
            if (cls && node.to > node.from) {
              out.push(Decoration.mark({ class: cls }).range(node.from, node.to));
            }
          },
        });
      }
      return Decoration.set(out, true);
    }
  },
  { decorations: (v) => v.decorations },
);

const theme = EditorView.theme({
  "&": {
    height: "100%",
    color: "var(--text)",
    backgroundColor: "var(--editor-bg)",
    fontSize: "16px",
  },
  "&.cm-focused": { outline: "none" },
  ".cm-scroller": {
    fontFamily: "var(--font-editor)",
    lineHeight: "1.65",
    overflow: "auto",
  },
  ".cm-content": {
    caretColor: "var(--accent)",
    maxWidth: "52rem",
    margin: "0 auto",
    padding: "28px 28px 40vh",
  },
  ".cm-cursor": { borderLeftColor: "var(--accent)" },
  "&.cm-focused .cm-selectionBackground, .cm-selectionBackground, ::selection": {
    backgroundColor: "var(--selection)",
  },
  ".cm-activeLine": { backgroundColor: "transparent" },
  ".cm-searchMatch": { backgroundColor: "var(--mark-bg)" },
  ".cm-panels": {
    backgroundColor: "var(--surface)",
    color: "var(--text)",
    borderColor: "var(--border)",
  },
});

export function createEditor(parent: HTMLElement, cb: EditorCallbacks): EditorHandle {
  const clickHandler = EditorView.domEventHandlers({
    mousedown(event, view) {
      if (!(event.ctrlKey || event.metaKey) || event.button !== 0) return false;
      const pos = view.posAtCoords({ x: event.clientX, y: event.clientY });
      if (pos === null) return false;
      const line = view.state.doc.lineAt(pos);
      for (const m of line.text.matchAll(WIKILINK)) {
        const from = line.from + (m.index ?? 0);
        const to = from + m[0].length;
        if (pos >= from && pos <= to) {
          const target = wikilinkTarget(m[1] ?? "");
          if (target) {
            event.preventDefault();
            cb.onLinkClick(target);
            return true;
          }
        }
      }
      return false;
    },
  });

  const makeState = (doc: string): EditorState =>
    EditorState.create({
      doc,
      extensions: [
        history(),
        closeBrackets(),
        highlightSelectionMatches(),
        markdown(),
        EditorView.lineWrapping,
        EditorView.contentAttributes.of({ "aria-label": "Page text", spellcheck: "true" }),
        keymap.of([
          ...closeBracketsKeymap,
          ...defaultKeymap,
          ...historyKeymap,
          ...searchKeymap,
          indentWithTab,
        ]),
        matchPlugin(wikilinkMatcher),
        matchPlugin(tagMatcher),
        markdownStyle,
        clickHandler,
        theme,
        EditorView.updateListener.of((u) => {
          if (u.docChanged) cb.onChange();
        }),
      ],
    });

  const view = new EditorView({ state: makeState(""), parent });
  return {
    // A fresh state per page also resets undo history.
    setText: (text) => view.setState(makeState(text)),
    getText: () => view.state.doc.toString(),
    focus: () => view.focus(),
  };
}

// ---------------------------------------------------------------- block editor (Workspace)

export type SlashKey = "ArrowDown" | "ArrowUp" | "Enter" | "Escape";

export interface BlockEditorOptions {
  doc: string;
  onChange: (text: string) => void;
  onBlur: () => void;
  onEscape: () => void;
  onCtrlEnter: () => void;
  onBackspaceEmpty: () => void;
  /** Offer a key to the slash menu; return true when it consumed the key. */
  slashKey: (key: SlashKey) => boolean;
}

export interface BlockEditorHandle {
  getText: () => string;
  setText: (text: string) => void;
  focus: (pos?: number) => void;
  destroy: () => void;
}

const blockTheme = EditorView.theme({
  "&": { color: "var(--text)", backgroundColor: "transparent", fontSize: "inherit" },
  "&.cm-focused": { outline: "none" },
  ".cm-scroller": { fontFamily: "var(--font-editor)", lineHeight: "1.6", overflow: "visible" },
  ".cm-content": { caretColor: "var(--accent)", padding: "2px 0" },
  ".cm-cursor": { borderLeftColor: "var(--accent)" },
  "&.cm-focused .cm-selectionBackground, .cm-selectionBackground, ::selection": {
    backgroundColor: "var(--selection)",
  },
  ".cm-activeLine": { backgroundColor: "transparent" },
});

/** A compact Markdown editor for a single block. */
export function createBlockEditor(parent: HTMLElement, o: BlockEditorOptions): BlockEditorHandle {
  const slash = (key: SlashKey) => () => {
    if (o.slashKey(key)) return true;
    if (key === "Escape") {
      o.onEscape();
      return true;
    }
    return false;
  };
  const state = EditorState.create({
    doc: o.doc,
    extensions: [
      history(),
      closeBrackets(),
      markdown(),
      EditorView.lineWrapping,
      EditorView.contentAttributes.of({ "aria-label": "Block text", spellcheck: "true" }),
      Prec.highest(
        keymap.of([
          { key: "ArrowDown", run: slash("ArrowDown") },
          { key: "ArrowUp", run: slash("ArrowUp") },
          { key: "Enter", run: slash("Enter") },
          { key: "Escape", run: slash("Escape") },
          {
            key: "Mod-Enter",
            run: () => {
              o.onCtrlEnter();
              return true;
            },
          },
          {
            key: "Backspace",
            run: (v) => {
              if (v.state.doc.length === 0) {
                o.onBackspaceEmpty();
                return true;
              }
              return false;
            },
          },
        ]),
      ),
      keymap.of([...closeBracketsKeymap, ...defaultKeymap, ...historyKeymap, indentWithTab]),
      matchPlugin(wikilinkMatcher),
      matchPlugin(tagMatcher),
      markdownStyle,
      blockTheme,
      EditorView.updateListener.of((u) => {
        if (u.docChanged) o.onChange(u.state.doc.toString());
      }),
      EditorView.domEventHandlers({ blur: () => (o.onBlur(), false) }),
    ],
  });
  const view = new EditorView({ state, parent });
  return {
    getText: () => view.state.doc.toString(),
    setText: (text) =>
      view.dispatch({
        changes: { from: 0, to: view.state.doc.length, insert: text },
        selection: { anchor: text.length },
      }),
    focus: (pos) => {
      view.focus();
      const at = Math.min(pos ?? view.state.doc.length, view.state.doc.length);
      view.dispatch({ selection: { anchor: at } });
    },
    destroy: () => view.destroy(),
  };
}
