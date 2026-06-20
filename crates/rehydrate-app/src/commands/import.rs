use rehydrate_core::{DocumentSummary, ImportKind};
use tauri::{AppHandle, State};
use tauri_plugin_dialog::DialogExt;

use crate::state::AppState;
use crate::util::{err, lib_arc};

/// Raster image formats that `import_dropped_file` will silently
/// convert to a single-page PDF before importing. The conversion is
/// done entirely in-process via `rehydrate_render::build_pdf_from_image_bytes`;
/// no external tools are required.
pub(crate) const CONVERTIBLE_IMAGE_EXTS: &[&str] =
    &["png", "jpg", "jpeg", "gif", "bmp", "tiff", "tif", "webp"];

/// Document formats that LibreOffice can convert to PDF via its headless
/// mode. Requires LibreOffice to be installed; `find_libreoffice` probes
/// the standard macOS locations and PATH at call time.
pub(crate) const CONVERTIBLE_DOC_EXTS: &[&str] = &["docx", "doc", "odt", "rtf"];

/// Hard cap on `import_dropped_file`. Real PDFs and EPUBs top out at
/// a few hundred MB even for textbook-sized documents; reMarkable
/// notebook PDFs are rarely past ~20 MB.
///
/// The bytes traverse a JSON-encoded IPC channel (each byte becomes
/// 1–4 ASCII chars in a `number[]`), so the renderer-side encoder and
/// the Rust-side JSON decoder both allocate ~4× the cap before this
/// guard fires. 64 MB on the wire ⇒ ~256 MB worst case in the JSON
/// decoder — uncomfortable but survivable on every system the app
/// runs on. A streaming `tauri::ipc::Channel<Vec<u8>>` path would
/// eliminate the multiplier; we keep the JSON-array path for now to
/// avoid an extra dep on either side, and pay for it with a tight
/// cap.
///
/// Mirrored by `MAX_IMPORT_FILE_BYTES` in `ui/src/ipc.ts`. The
/// renderer preflights `file.size` against that constant so an
/// oversize drop never reaches `arrayBuffer()` and never blows up
/// renderer memory before this backend guard fires (issue #24). If
/// you change this number, change the TS constant too.
pub(crate) const MAX_IMPORT_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Reject oversize drops with a stable, user-facing error string.
/// Pulled out of `import_dropped_file` so it can be unit-tested
/// without standing up the AppState/Tauri runtime — the renderer's
/// preflight relies on the same numeric cap (see
/// `MAX_IMPORT_FILE_BYTES` in `ui/src/ipc.ts`), so the contract here
/// is the last line of defence on the wire.
pub(crate) fn enforce_import_size_cap(file_name: &str, byte_len: u64) -> Result<(), String> {
    if byte_len > MAX_IMPORT_FILE_BYTES {
        Err(format!(
            "{file_name} is too large to import (limit is {} MiB)",
            MAX_IMPORT_FILE_BYTES / 1024 / 1024
        ))
    } else {
        Ok(())
    }
}

/// Locate the LibreOffice `soffice` binary. Checks the standard macOS
/// app-bundle path, Homebrew prefix, and PATH (via `which`) in that order.
fn find_libreoffice() -> Option<std::path::PathBuf> {
    const CANDIDATES: &[&str] = &[
        "/Applications/LibreOffice.app/Contents/MacOS/soffice",
        "/opt/homebrew/bin/soffice",
        "/usr/local/bin/soffice",
    ];
    for p in CANDIDATES {
        let path = std::path::Path::new(p);
        if path.exists() {
            return Some(path.to_path_buf());
        }
    }
    // Fall back to PATH resolution via `which`.
    if let Ok(out) = std::process::Command::new("which").arg("soffice").output() {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return Some(std::path::PathBuf::from(s));
            }
        }
    }
    None
}

/// Import a PDF or EPUB from disk into the library.
///
/// Audit fix H6: the OS file picker runs server-side here; the
/// renderer can no longer hand us a path of its choosing (e.g. a
/// symlink `evil.pdf → ~/.ssh/id_rsa`). Returns `None` if the user
/// cancelled the dialog. We also sniff the magic bytes after picking
/// so a renamed-but-not-actually-PDF/EPUB is rejected early.
#[tauri::command]
pub async fn import_file(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Option<DocumentSummary>, String> {
    let lib = lib_arc(&state).await?;

    // The dialog is a blocking OS call; run it on a worker thread so
    // we don't tie up the Tauri main thread.
    let app_for_pick = app.clone();
    let picked = tauri::async_runtime::spawn_blocking(move || {
        app_for_pick
            .dialog()
            .file()
            .add_filter("Documents", &["pdf", "epub"])
            .set_title("Import a PDF or EPUB")
            .blocking_pick_file()
    })
    .await
    .map_err(err)?;

    let Some(file_path) = picked else {
        return Ok(None);
    };
    let path = file_path
        .into_path()
        .map_err(|e| format!("could not resolve picked path: {e}"))?;

    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .ok_or_else(|| "file has no extension".to_string())?;
    let kind = ImportKind::from_extension(ext).ok_or_else(|| {
        format!("unsupported file type: .{ext} — only PDF and EPUB are supported")
    })?;

    // Magic-byte sniff: refuse a "*.pdf" symlink that actually points
    // at, say, an SSH private key. PDF starts with "%PDF-", EPUB is a
    // ZIP ("PK\x03\x04").
    let mut head = [0u8; 5];
    {
        use std::io::Read;
        let mut f = std::fs::File::open(&path).map_err(err)?;
        let _ = f.read(&mut head).map_err(err)?;
    }
    let looks_pdf = head.starts_with(b"%PDF-");
    let looks_epub = head.starts_with(b"PK\x03\x04");
    let extension_kind_ok = match kind {
        ImportKind::Pdf => looks_pdf,
        ImportKind::Epub => looks_epub,
    };
    if !extension_kind_ok {
        return Err(format!(
            "{} does not look like a {} file (header check failed)",
            path.display(),
            ext.to_uppercase()
        ));
    }

    let visible_name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Untitled")
        .to_string();
    // Run on the blocking pool: `import_file` does synchronous file
    // I/O (open + stream-hash) and a SQLite commit; on the
    // single-threaded executor it would stall every other IPC call
    // for the duration of an import.
    let name_for_task = visible_name.clone();
    let import =
        tauri::async_runtime::spawn_blocking(move || lib.import_file(&path, kind, &name_for_task, None))
            .await
            .map_err(err)?
            .map_err(err)?;
    let _ = visible_name;
    Ok(Some(import))
}

/// Import a file the user dragged onto the window. WebView security
/// hides the real filesystem path of an OS-level drop, so the frontend
/// streams the bytes over IPC; we stage them to a tempfile so the
/// existing path-based `Library::import_file` can do its work
/// unchanged. The `file_name` is used to derive the extension *and*
/// the default visible-name (sans extension), mirroring the file
/// picker path.
///
/// Same magic-byte sniff as `import_file`: a "*.pdf" that doesn't
/// start with "%PDF-" is rejected before any blob is written.
#[tauri::command]
pub async fn import_dropped_file(
    file_name: String,
    bytes: Vec<u8>,
    parent_id: Option<String>,
    state: State<'_, AppState>,
) -> Result<DocumentSummary, String> {
    let lib = lib_arc(&state).await?;

    enforce_import_size_cap(&file_name, bytes.len() as u64)?;

    let ext_lower = std::path::Path::new(&file_name)
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .ok_or_else(|| "file has no extension".to_string())?;

    // If the dropped file is a raster image, convert it to a
    // single-page PDF in-process before the rest of the pipeline runs.
    // The visible name and extension are updated so downstream code
    // treats the result as an ordinary PDF drop.
    let (bytes, file_name, ext_lower) = if CONVERTIBLE_IMAGE_EXTS.contains(&ext_lower.as_str()) {
        let stem = std::path::Path::new(&file_name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("image")
            .to_string();
        // Image decode + PDF render are CPU-bound; run on the blocking pool.
        let bytes_for_render = bytes.clone();
        let name_for_render = file_name.clone();
        let pdf_bytes =
            tauri::async_runtime::spawn_blocking(move || {
                rehydrate_render::build_pdf_from_image_bytes(&name_for_render, &bytes_for_render)
            })
            .await
            .map_err(err)?
            .map_err(|e| format!("image→PDF conversion failed: {e}"))?;
        (pdf_bytes, format!("{stem}.pdf"), "pdf".to_string())
    } else if CONVERTIBLE_DOC_EXTS.contains(&ext_lower.as_str()) {
        let stem = std::path::Path::new(&file_name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("document")
            .to_string();
        let soffice = find_libreoffice().ok_or_else(|| {
            "LibreOffice not found — install it from libreoffice.org to import Word documents"
                .to_string()
        })?;
        let ext = ext_lower.clone();
        // LibreOffice is blocking I/O + subprocess; run on the blocking pool.
        let pdf_bytes =
            tauri::async_runtime::spawn_blocking(move || -> Result<Vec<u8>, String> {
                let tmp =
                    tempfile::tempdir().map_err(|e| format!("temp dir: {e}"))?;
                let in_path = tmp.path().join(format!("input.{ext}"));
                std::fs::write(&in_path, &bytes)
                    .map_err(|e| format!("write temp file: {e}"))?;
                let out = std::process::Command::new(&soffice)
                    .args([
                        "--headless",
                        "--norestore",
                        "--convert-to",
                        "pdf",
                        "--outdir",
                        tmp.path().to_str().unwrap_or("/tmp"),
                        in_path.to_str().unwrap_or(""),
                    ])
                    .output()
                    .map_err(|e| format!("LibreOffice failed to start: {e}"))?;
                if !out.status.success() {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    return Err(format!("LibreOffice exited with error: {stderr}"));
                }
                let pdf_path = tmp.path().join("input.pdf");
                std::fs::read(&pdf_path)
                    .map_err(|e| format!("read converted PDF: {e}"))
                // `tmp` is dropped here, cleaning up the temp directory.
            })
            .await
            .map_err(err)?
            .map_err(|e| format!("document→PDF conversion failed: {e}"))?;
        (pdf_bytes, format!("{stem}.pdf"), "pdf".to_string())
    } else {
        (bytes, file_name, ext_lower)
    };

    let kind = ImportKind::from_extension(&ext_lower).ok_or_else(|| {
        format!(
            "unsupported file type: .{ext_lower} — \
             only PDF, EPUB, images (PNG/JPEG/GIF/BMP/TIFF/WEBP), \
             and documents (DOCX/DOC/ODT/RTF) are supported"
        )
    })?;

    // Magic-byte sniff before we touch disk or the library.
    let looks_pdf = bytes.starts_with(b"%PDF-");
    let looks_epub = bytes.starts_with(b"PK\x03\x04");
    let extension_kind_ok = match kind {
        ImportKind::Pdf => looks_pdf,
        ImportKind::Epub => looks_epub,
    };
    if !extension_kind_ok {
        return Err(format!(
            "{file_name} does not look like a {} file (header check failed)",
            ext_lower.to_uppercase()
        ));
    }

    let visible_name = std::path::Path::new(&file_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Untitled")
        .to_string();

    // Stage bytes to a tempfile so we can reuse the path-based
    // import. NamedTempFile auto-deletes on drop even if import_file
    // panics — no orphans in /tmp on failure.
    let mut tf = tempfile::NamedTempFile::new().map_err(err)?;
    {
        use std::io::Write;
        tf.write_all(&bytes).map_err(err)?;
        tf.flush().map_err(err)?;
    }
    // Same rationale as `import_file`: hand the synchronous import
    // off to the blocking pool so the executor stays responsive.
    let path = tf.path().to_path_buf();
    let parent_id_ref = parent_id.as_deref().map(str::to_owned);
    tauri::async_runtime::spawn_blocking(move || {
        lib.import_file(&path, kind, &visible_name, parent_id_ref.as_deref())
    })
    .await
    .map_err(err)?
    .map_err(err)
}

#[cfg(test)]
mod import_size_cap_tests {
    use super::{enforce_import_size_cap, MAX_IMPORT_FILE_BYTES};

    #[test]
    fn cap_value_is_64_mib() {
        // Lock the constant. The renderer's
        // MAX_IMPORT_FILE_BYTES in ui/src/ipc.ts must match this
        // number so the preflight in handleExternalFileDrop
        // (App.tsx) rejects exactly the same set of files this
        // backend would reject. Issue #24: a divergence here would
        // re-open the OOM/freeze path that the renderer preflight
        // is supposed to close.
        assert_eq!(MAX_IMPORT_FILE_BYTES, 64 * 1024 * 1024);
    }

    #[test]
    fn at_or_below_cap_is_accepted() {
        assert!(enforce_import_size_cap("ok.pdf", 0).is_ok());
        assert!(enforce_import_size_cap("ok.pdf", 1).is_ok());
        assert!(enforce_import_size_cap("ok.pdf", MAX_IMPORT_FILE_BYTES).is_ok());
    }

    #[test]
    fn one_byte_over_cap_is_rejected() {
        let err = enforce_import_size_cap("huge.pdf", MAX_IMPORT_FILE_BYTES + 1)
            .expect_err("byte over the cap must be rejected");
        // Surface the file name so the toast/error UI can identify
        // which drop was rejected when the user dropped a batch.
        assert!(
            err.contains("huge.pdf"),
            "error must mention file name: {err}"
        );
        assert!(
            err.contains("64 MiB"),
            "error must spell out the limit: {err}"
        );
    }
}
