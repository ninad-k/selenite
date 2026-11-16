//! Block-level page editing on the [`Engine`] (Workspace mode). Every edit splices one block
//! into the file through the vault block functions, so untouched bytes never change, and every
//! write is conditional on the revision the caller rendered.

use selenite_core::VaultPath;
use selenite_storage::Revision;
use selenite_vault::{
    BlockRef, Document, RenderOptions, delete_block, insert_block, move_block, parse_blocks,
    render_block, replace_block, toggle_task_item,
};
use serde::{Deserialize, Serialize};

use crate::engine::{Engine, EngineError, Result};

/// A block of a page, with its source and safe HTML.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RenderedBlock {
    /// Position in the page.
    pub index: usize,
    /// Block kind name (`paragraph`, `heading`, `list`, ...).
    pub kind: String,
    /// `^anchor` id, if the block has one.
    pub anchor: Option<String>,
    /// Raw Markdown of the block.
    pub markdown: String,
    /// Rendered, sanitised HTML.
    pub html: String,
}

/// A page split into blocks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlockPage {
    /// Page path.
    pub path: VaultPath,
    /// Page title.
    pub title: String,
    /// Revision the blocks were read from; pass it back with edits.
    pub revision: String,
    /// `icon` front matter property (an emoji or short text), if any.
    pub icon: Option<String>,
    /// `cover` front matter property (image path or URL), if any.
    pub cover: Option<String>,
    /// The blocks.
    pub blocks: Vec<RenderedBlock>,
}

/// One block edit.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum BlockEdit {
    /// Replace a block's Markdown.
    Replace {
        /// Block index or `^anchor`.
        block: String,
        /// New Markdown.
        markdown: String,
    },
    /// Insert a block after another (or at the start when `after` is absent).
    Insert {
        /// Block index or `^anchor` to insert after.
        after: Option<String>,
        /// Markdown of the new block.
        markdown: String,
    },
    /// Remove a block from the page.
    Delete {
        /// Block index or `^anchor`.
        block: String,
    },
    /// Move a block to a new position.
    Move {
        /// Block index or `^anchor`.
        block: String,
        /// Target position after removal.
        to: usize,
    },
    /// Toggle a `- [ ]` item.
    ToggleTask {
        /// Block index or `^anchor`.
        block: String,
        /// 0-based line within the block.
        line: usize,
    },
}

fn block_ref(s: &str) -> Result<BlockRef> {
    Ok(BlockRef::parse(s)?)
}

const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "avif"];

impl Engine {
    /// Split a page into rendered blocks. `asset_base` prefixes vault-relative asset paths
    /// (for example the desktop app's local asset protocol); links to pages resolve via the
    /// index.
    pub fn page_blocks(&self, path: &VaultPath, asset_base: &str) -> Result<BlockPage> {
        let (text, revision) = self.read_page(path)?;
        let doc = Document::parse(&text);
        let resolve_link = |t: &str| self.resolve(t).ok().flatten().map(|p| p.to_string());
        let resolve_asset = |t: &str| self.asset_url(path, t, asset_base);
        let opts = RenderOptions {
            resolve_link: &resolve_link,
            resolve_asset: &resolve_asset,
        };
        let blocks = parse_blocks(doc.body())
            .into_iter()
            .map(|b| RenderedBlock {
                index: b.index,
                kind: b.kind.name().to_string(),
                anchor: b.anchor.clone(),
                html: render_block(&b, &opts),
                markdown: b.text,
            })
            .collect();
        let prop = |k: &str| doc.property(k).and_then(|v| v.as_str()).map(str::to_string);
        Ok(BlockPage {
            path: path.clone(),
            title: doc.title().unwrap_or_else(|| path.stem().to_string()),
            revision: revision.0,
            icon: prop("icon"),
            cover: prop("cover").and_then(|c| self.asset_url(path, &c, asset_base)),
            blocks,
        })
    }

    /// Resolve an asset reference (relative to the page, vault-relative, or a bare file name
    /// anywhere in the vault) to `asset_base` + vault path, if the file exists. Remote
    /// `http(s)` URLs pass through unchanged.
    fn asset_url(&self, page: &VaultPath, target: &str, asset_base: &str) -> Option<String> {
        let t = target.trim();
        if t.starts_with("https://") || t.starts_with("http://") {
            return Some(t.to_string());
        }
        let decoded = t.replace("%20", " ");
        let candidates = [
            page.parent()
                .map(|p| format!("{p}/{decoded}"))
                .unwrap_or_else(|| decoded.clone()),
            decoded.clone(),
        ];
        for c in candidates {
            if let Ok(p) = VaultPath::new(&c) {
                if self.storage().stat(&p).ok().flatten().is_some() {
                    return Some(format!("{asset_base}{}", encode_path(p.as_str())));
                }
            }
        }
        // Bare file name: search the vault (first match by shortest path).
        let name = decoded
            .rsplit('/')
            .next()
            .unwrap_or(&decoded)
            .to_lowercase();
        let ext = name.rsplit('.').next().unwrap_or("");
        if !IMAGE_EXTS.contains(&ext) && !name.contains('.') {
            return None;
        }
        let mut found: Vec<VaultPath> = self
            .storage()
            .list()
            .ok()?
            .into_iter()
            .map(|e| e.path)
            .filter(|p| !p.is_hidden() && p.file_name().to_lowercase() == name)
            .collect();
        found.sort_by_key(|p| p.as_str().len());
        found
            .first()
            .map(|p| format!("{asset_base}{}", encode_path(p.as_str())))
    }

    /// Apply one block edit to a page, conditional on `expected`. Returns the new revision.
    pub fn edit_block(
        &mut self,
        path: &VaultPath,
        edit: &BlockEdit,
        expected: Revision,
    ) -> Result<Revision> {
        let (text, current) = self.read_page(path)?;
        if current != expected {
            return Err(EngineError::Storage(
                selenite_storage::StorageError::Conflict {
                    path: path.clone(),
                    expected,
                    actual: Some(current),
                },
            ));
        }
        let mut doc = Document::parse(&text);
        let body = doc.body().to_string();
        let new_body = match edit {
            BlockEdit::Replace { block, markdown } => {
                replace_block(&body, &block_ref(block)?, markdown)?
            }
            BlockEdit::Insert { after, markdown } => {
                let r = after.as_deref().map(block_ref).transpose()?;
                insert_block(&body, r.as_ref(), markdown)?
            }
            BlockEdit::Delete { block } => delete_block(&body, &block_ref(block)?)?,
            BlockEdit::Move { block, to } => move_block(&body, &block_ref(block)?, *to)?,
            BlockEdit::ToggleTask { block, line } => {
                toggle_task_item(&body, &block_ref(block)?, *line)?
            }
        };
        doc.set_body(&new_body);
        let entry = self.save_page(path, &doc.to_text(), Some(current))?;
        Ok(entry.revision)
    }
}

fn encode_path(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    for b in p.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
