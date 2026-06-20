//! Helpers shared between sibling command modules. `sanitize` and
//! `thumbnail_fallback_pdf` are both used by `documents.rs` (open /
//! preview) and `export.rs` (drag-out / export-version) — keeping
//! them here avoids a peer dependency between those two modules.

use rehydrate_core::{Library, Manifest};

use crate::util::err;

pub(super) fn sanitize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
            out.push(c);
        } else if c == ' ' {
            out.push('-');
        }
    }
    // Strip leading/trailing dots, dashes, and underscores. Windows
    // silently drops trailing dots and spaces on write, so a doc
    // literally named "..." would otherwise become an empty filename
    // (or worse, collide with a parent-directory shortcut). We also
    // strip dashes/underscores because the space→`-` rewrite above
    // turns runs of trailing whitespace into runs of dashes.
    let trimmed = out.trim_matches(|c: char| c == '.' || c == '-' || c == '_');
    if trimmed.is_empty() {
        return "Untitled".into();
    }
    // Reserved-name check looks at the bare stem (everything before
    // the first `.`) case-insensitively — Windows reserves these
    // regardless of extension or casing.
    let stem = trimmed.split('.').next().unwrap_or(trimmed);
    let reserved = matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM0"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT0"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    );
    if reserved {
        // Prefix-rescue a reserved name so the on-disk filename is
        // legal on Windows but still recognisable to the user. A
        // doc the user titled "CON" exports as "doc-CON-…".
        format!("doc-{trimmed}")
    } else {
        trimmed.to_string()
    }
}

pub(super) fn thumbnail_fallback_pdf(
    lib: &Library,
    manifest: &Manifest,
    title: &str,
) -> Result<Vec<u8>, String> {
    let mut thumbs: Vec<_> = manifest
        .files
        .iter()
        .filter(|f| f.path.ends_with(".png") && f.path.contains(".thumbnails"))
        .collect();
    if thumbs.is_empty() {
        return Err(
            "notebook has no .rm ink files we can parse and no thumbnails to fall back on \
             — sync the device once (or open and edit the notebook on the tablet first) \
             and try again"
                .to_string(),
        );
    }
    thumbs.sort_by(|a, b| a.path.cmp(&b.path));
    let mut pages = Vec::with_capacity(thumbs.len());
    for f in &thumbs {
        pages.push(lib.read_blob(&f.sha256).map_err(err)?);
    }
    rehydrate_render::build_pdf_from_pngs(title, &pages)
}

#[cfg(test)]
mod sanitize_tests {
    use super::sanitize;

    #[test]
    fn strips_special_chars_and_normalises_spaces() {
        assert_eq!(sanitize("Hello World"), "Hello-World");
        assert_eq!(sanitize("a/b\\c?d*e:f"), "abcdef");
    }

    #[test]
    fn empty_after_strip_falls_back_to_untitled() {
        assert_eq!(sanitize(""), "Untitled");
        assert_eq!(sanitize("???"), "Untitled");
        assert_eq!(sanitize(".."), "Untitled");
        assert_eq!(sanitize("   "), "Untitled");
    }

    #[test]
    fn trailing_dot_or_space_is_dropped() {
        // Windows would otherwise silently truncate to "foo".
        assert_eq!(sanitize("foo."), "foo");
        assert_eq!(sanitize("foo "), "foo");
        assert_eq!(sanitize(".foo."), "foo");
    }

    #[test]
    fn windows_reserved_names_are_prefixed() {
        // Without the rescue, exporting a doc named "CON" would
        // produce "CON-v…" — a path Windows refuses to create.
        assert_eq!(sanitize("CON"), "doc-CON");
        assert_eq!(sanitize("nul"), "doc-nul");
        assert_eq!(sanitize("LPT1"), "doc-LPT1");
        // Non-reserved names with the same prefix are untouched.
        assert_eq!(sanitize("Console"), "Console");
        assert_eq!(sanitize("Connor"), "Connor");
    }
}
