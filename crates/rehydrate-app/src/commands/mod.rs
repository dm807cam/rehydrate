//! Tauri command handlers, split by domain.
//!
//! The public surface is the union of `pub use` glob re-exports below,
//! so `lib.rs` continues to reference each command by its short name
//! (`commands::open_library`) without caring which sub-file it lives in.
//! The glob form is required because `#[tauri::command]` generates
//! `__cmd__<name>` helpers per command that `tauri::generate_handler!`
//! needs at the re-exported path; a selective `pub use` only brings the
//! function and loses the macro shim.
//!
//! Sub-modules are private; if a helper is needed by a sibling module
//! it's marked `pub(super)` in `util.rs`.

mod device;
mod documents;
mod export;
mod folders;
mod import;
mod library;
mod logs;
mod misc;
mod sync;
mod util;

pub use device::*;
pub use documents::*;
pub use export::*;
pub use folders::*;
pub use import::*;
pub use library::*;
pub use logs::*;
pub use misc::*;
pub use sync::*;

// `pdf_annotation_plan` is crate-private and used by the sibling
// `crate::export` module (the top-level `src/export.rs`, not the
// command submodule). A glob `pub use` can't carry `pub(crate)`
// items across a module boundary, so re-export it explicitly with
// crate visibility preserved.
pub(crate) use export::pdf_annotation_plan;
