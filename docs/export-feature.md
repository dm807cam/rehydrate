# Implementing bulk export in reHydrate

This document describes the export feature added in this fork so it can be ported back to reHydrate. It is written as a briefing for Claude Opus — read it fully before touching any file.

## What the feature does

A new Tauri command `export_as_pdfs` exports the entire library (or a subtree) to a target folder on disk as PDFs, incrementally. On each run it:

1. Reads a state file (`.rehydrate-export.json`) from the target folder to know what was already exported and at which version.
2. Skips documents whose `current_version_id` and output path have not changed since the last run.
3. For documents that need (re-)export:
   - **PDF/EPUB with body blob** — copies the body blob verbatim; if `include_annotations` is true and the document is a PDF, composites the `.rm` annotation strokes on top using `overlay_annotations_on_pdf`.
   - **Notebooks (`.rm` only)** — renders strokes to PDF via the existing `build_pdf_from_rm_files`; falls back to thumbnail PNG collation if rendering fails or the format is too old.
4. Deletes output files for documents that have been trashed, moved out of scope, or renamed.
5. Mirrors the folder hierarchy from the library into subdirectories of the target folder.
6. Emits `export:progress` Tauri events (`{ current, total, current_file }`) so the UI can show a progress indicator.
7. Returns `{ exported, skipped, deleted, target_dir }`.

## New code to add

### 1. `crates/rehydrate-render/src/lib.rs` — three additions

**a. `Tool::Shader` in the render pipeline**

Wherever `Tool::Highlighter` is matched (opacity, cap style, width, colour), add `Tool::Shader` alongside it. Shader behaves identically to Highlighter for rendering purposes.

**b. `overlay_annotations_on_pdf(pdf_bytes: &[u8], annotations: &[(usize, Vec<u8>)]) -> Result<Vec<u8>, String>`**

Composites `.rm` v6 strokes onto an existing PDF. `annotations` is a list of `(page_index, rm_bytes)` pairs (0-based, matching PDF page order). Pages with no entry are left unchanged.

Implementation outline:
- Parse the PDF with `lopdf::Document::load_mem`.
- For each `(page_index, rm_bytes)` pair, parse the `.rm` bytes with the existing stroke parser, then stroke each path onto the PDF page as a PDF content stream appended to the page's existing content.
- The reMarkable canvas is 1404 × 1872 pt (RM_CANVAS_WIDTH / RM_CANVAS_HEIGHT). v6 x-coordinates are centred at 0 (range −702 to +702); y is top-to-bottom (0 at top). Map to PDF coordinates (origin bottom-left, y-up) when emitting path commands.
- Return the modified PDF bytes via `lopdf::Document::save_to_bytes`.

Add `lopdf = "0.39"` to the workspace `Cargo.toml` and to `crates/rehydrate-render/Cargo.toml`.

**c. `build_pdf_from_pngs(title: &str, pages: &[Vec<u8>]) -> Result<Vec<u8>, String>`**

Same signature pattern as `build_pdf_from_rm_files` but accepts raw PNG bytes. Used as the thumbnail fallback path. Add it alongside the existing function.

### 2. `crates/rehydrate-app/src/commands.rs` — two additions

**a. `pub(crate) fn pdf_annotation_pairs(manifest, doc_id, lib) -> Result<Vec<(usize, Vec<u8>)>, String>`**

Resolves the ordered list of `.rm` blobs for a PDF document's annotations:
1. Find the `{doc_id}.content` file in the manifest; return `Ok(vec![])` if absent.
2. Parse it as JSON; extract `pages: [uuid, …]`.
3. Build a map of `uuid → sha256` from all `{doc_id}/{uuid}.rm` entries in the manifest.
4. For each UUID in page order, if a `.rm` blob exists, read it and push `(page_index, rm_bytes)` into the result.

**b. Wire `include_annotations` into `open_document`**

In the existing `open_document` command, after reading the PDF body blob, call `pdf_annotation_pairs` and, if non-empty, call `overlay_annotations_on_pdf`. Keep a `warn!` + fall-through to the unmodified bytes if the overlay fails.

Also change the cache key from `body.sha256` to `manifest.sha256` (i.e. `doc.current_manifest`) so that annotation edits — which change the manifest but not the body blob — correctly bust the cache.

### 3. `crates/rehydrate-app/src/export.rs` — new file

New module, registered in `lib.rs` with `mod export;` and the command added to `tauri::Builder::invoke_handler`.

Key types:

```rust
struct ExportStateEntry { version_id: i64, path: String, include_annotations: bool }
struct ExportState { documents: HashMap<String, ExportStateEntry> }
struct ExportProgress { current: usize, total: usize, current_file: String }  // #[derive(Serialize, Clone)]
struct ExportAsPdfsResult { exported: usize, skipped: usize, deleted: usize, target_dir: PathBuf }
```

State is persisted as pretty-printed JSON in `.rehydrate-export.json` at the root of the target folder.

Command signature:

```rust
#[tauri::command]
pub async fn export_as_pdfs(
    target_dir: String,
    root_folder_id: Option<String>,
    include_annotations: bool,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<ExportAsPdfsResult, String>
```

The folder-path resolution (for nested subfolder mirroring) walks `FolderEntry.parent` links bottom-up and joins with `/`. Filenames are sanitised by replacing `/`, `\`, `:` with `-` and trimming leading/trailing dots and spaces.

Skip documents whose `parent == Some("trash")`. EPUBs use extension `epub` and are never annotation-overlaid.

## What does NOT need to change

- `rehydrate-core`, `rehydrate-sync`, `rehydrate-device` — no changes required.
- The existing `prepare_export_pdf` command (single-document drag-to-export) can adopt `pdf_annotation_pairs` + `overlay_annotations_on_pdf` opportunistically, but it is not required for the bulk export feature to work.
- CI, release workflow, dependency auditing — untouched.
