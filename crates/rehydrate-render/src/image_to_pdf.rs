//! Image → PDF assembly. `build_pdf_from_pngs` is the
//! notebook-thumbnail fallback path (used when a notebook has no
//! parseable `.rm` ink files); `build_pdf_from_image_bytes` is the
//! single-page drop-an-image-onto-the-app importer.

use image::ImageReader;
use printpdf::{Mm, Op, PdfDocument, PdfPage, PdfSaveOptions, Pt, RawImage, XObjectTransform};

use crate::dims::{mm_to_pt, MM_PER_INCH, PAGE_H_MM, PAGE_W_MM, PT_PER_INCH};

/// Build a multi-page PDF from PNG byte buffers. Used as a fallback when
/// a notebook has no parseable `.rm` ink files. Pages are A4 portrait;
/// thumbnails are scaled to fit while preserving aspect ratio.
pub fn build_pdf_from_pngs(title: &str, pages: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    if pages.is_empty() {
        return Err("notebook has no thumbnail pages to render".to_string());
    }

    let mut doc = PdfDocument::new(title);
    let mut warnings = Vec::new();
    let mut pdf_pages = Vec::with_capacity(pages.len());

    let page_w_in = PAGE_W_MM / MM_PER_INCH;
    let page_h_in = PAGE_H_MM / MM_PER_INCH;
    let page_w_pt = mm_to_pt(PAGE_W_MM);
    let page_h_pt = mm_to_pt(PAGE_H_MM);

    for (i, png_bytes) in pages.iter().enumerate() {
        let dims = ImageReader::new(std::io::Cursor::new(png_bytes))
            .with_guessed_format()
            .map_err(|e| format!("page {i}: {e}"))?
            .into_dimensions()
            .map_err(|e| format!("page {i} dimensions: {e}"))?;

        let raw = RawImage::decode_from_bytes(png_bytes, &mut warnings)
            .map_err(|e| format!("page {i} decode: {e}"))?;
        let xobject_id = doc.add_image(&raw);

        let dpi_for_width = dims.0 as f32 / page_w_in;
        let dpi_for_height = dims.1 as f32 / page_h_in;
        let dpi = dpi_for_width.max(dpi_for_height);

        let drawn_w_in = dims.0 as f32 / dpi;
        let drawn_h_in = dims.1 as f32 / dpi;
        let drawn_w_pt = drawn_w_in * PT_PER_INCH;
        let drawn_h_pt = drawn_h_in * PT_PER_INCH;
        let translate_x_pt = (page_w_pt - drawn_w_pt) / 2.0;
        let translate_y_pt = (page_h_pt - drawn_h_pt) / 2.0;

        let page = PdfPage::new(
            Mm(PAGE_W_MM),
            Mm(PAGE_H_MM),
            vec![Op::UseXobject {
                id: xobject_id,
                transform: XObjectTransform {
                    translate_x: Some(Pt(translate_x_pt)),
                    translate_y: Some(Pt(translate_y_pt)),
                    scale_x: None,
                    scale_y: None,
                    rotate: None,
                    dpi: Some(dpi),
                },
            }],
        );
        pdf_pages.push(page);
    }

    Ok(doc
        .with_pages(pdf_pages)
        .save(&PdfSaveOptions::default(), &mut warnings))
}

/// Convert any raster image (PNG, JPEG, GIF, BMP, TIFF, WEBP) to a
/// single-page PDF sized to the reMarkable canvas, centred and scaled
/// to fit. Uses the same DPI / centering math as `build_pdf_from_pngs`.
///
/// The image is always re-encoded as PNG before embedding so that
/// `printpdf`'s `RawImage::decode_from_bytes` path is used
/// unconditionally — it avoids having to plumb format-specific PDF
/// stream types for each source format.
///
/// If the source image has fewer pixels than A4 at `TARGET_DPI`, it is
/// upsampled with a Lanczos3 filter before embedding. This prevents small
/// images from looking blocky when stretched to fill the page.
pub fn build_pdf_from_image_bytes(name: &str, bytes: &[u8]) -> Result<Vec<u8>, String> {
    // Decode to DynamicImage; `with_guessed_format` probes the byte
    // header so the file extension is not required.
    let img = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| format!("couldn't read '{name}': {e}"))?
        .decode()
        .map_err(|e| format!("couldn't decode '{name}': {e}"))?;

    // Ensure the embedded image has enough pixels for decent on-page
    // resolution. Target: fit within A4 at TARGET_DPI. If the source is
    // smaller in either axis, upsample with Lanczos3. Images already at
    // or above this resolution are kept at their native size.
    const TARGET_DPI: f32 = 200.0;
    let target_w = (PAGE_W_MM / MM_PER_INCH * TARGET_DPI) as u32; // ~1654 px
    let target_h = (PAGE_H_MM / MM_PER_INCH * TARGET_DPI) as u32; // ~2339 px
    let scale = (target_w as f32 / img.width() as f32)
        .min(target_h as f32 / img.height() as f32);
    let img = if scale > 1.0 {
        let new_w = (img.width() as f32 * scale).round() as u32;
        let new_h = (img.height() as f32 * scale).round() as u32;
        img.resize(new_w, new_h, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };

    // Re-encode as PNG (lossless, always supported by printpdf).
    let mut png = Vec::new();
    img.write_to(
        &mut std::io::Cursor::new(&mut png),
        image::ImageFormat::Png,
    )
    .map_err(|e| format!("couldn't encode '{name}' as PNG: {e}"))?;

    build_pdf_from_pngs(name, &[png])
}
