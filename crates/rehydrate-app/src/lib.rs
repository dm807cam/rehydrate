//! Tauri binary entry point. Holds AppState and exposes commands. No
//! business logic — every command delegates to the rehydrate-* crates.

use tauri::Manager;

mod commands;
mod config;
mod export;
mod keychain;
mod logging;
mod ocr_commands;
mod state;
mod util;

pub use state::AppState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    logging::init();
    tracing::info!("rehydrate starting; log dir = {:?}", logging::log_dir());

    // Audit fix M6: tauri-plugin-shell was registered but never used
    // from Rust; the renderer's `plugin:shell|open` IPC was a free
    // surface for opening arbitrary http/tel/mailto URLs. The opener
    // plugin handles the legitimate "open the document I just
    // exported" path on its own.
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        // Native OS drag-source so the renderer can drag a rendered
        // notebook PDF (or a stored PDF/EPUB blob) out of the window
        // to Finder / the Desktop / Mail. The plugin exposes a
        // `plugin:drag|start_drag` IPC command — invoked from
        // `@crabnebula/tauri-plugin-drag` in `ui/src/drag.ts`. The
        // Rust side does not touch this plugin directly; it just
        // needs to be registered so the JS half can call it.
        .plugin(tauri_plugin_drag::init())
        .manage(AppState::new())
        .setup(|app| {
            commands::spawn_reachability_watcher(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::ping,
            commands::get_recent_logs,
            commands::default_library_path,
            commands::open_library,
            commands::auto_open_library,
            commands::switch_library,
            commands::pick_library_directory,
            commands::pick_export_directory,
            commands::list_recent_libraries,
            commands::library_summary,
            commands::list_documents,
            commands::list_folders,
            commands::list_archived,
            commands::move_document,
            commands::rename_document,
            commands::rename_folder,
            commands::create_folder,
            commands::delete_folder,
            commands::revert_unpushed_changes,
            commands::reorder_folder,
            commands::archive_document,
            commands::unarchive_document,
            commands::purge_archived_document,
            commands::open_document,
            commands::prepare_export_pdf,
            commands::document_thumbnail,
            commands::get_history,
            commands::set_version_note,
            commands::export_version,
            export::export_as_pdfs,
            export::export_selected_as_pdfs,
            commands::verify_library,
            commands::import_file,
            commands::import_dropped_file,
            commands::garbage_collect,
            commands::device_state,
            commands::save_device_password,
            commands::forget_device_password,
            commands::forget_device_host_key,
            commands::connect_device,
            commands::disconnect_device,
            commands::purge_device_trash,
            commands::pull_plan,
            commands::pull_execute,
            commands::push_plan,
            commands::push_execute,
            commands::sync_two_way,
            commands::restore_version,
            commands::open_support_url,
            commands::app_version,
            commands::cancel_sync,
            commands::cancel_ocr,
            commands::reveal_log_dir,
            // OCR + CMS — Phase 1.0 OCR uses an Ollama daemon the
            // user runs themselves; the Settings modal lets them
            // pick base URL + model.
            ocr_commands::ocr_status,
            ocr_commands::transcribe_document,
            ocr_commands::get_transcript,
            ocr_commands::export_transcript,
            ocr_commands::get_ollama_config,
            ocr_commands::save_ollama_config,
            ocr_commands::ping_ollama,
            ocr_commands::list_curated_ollama_models,
            ocr_commands::default_ollama_model,
            ocr_commands::list_documents_needing_ocr,
            ocr_commands::publish_transcript,
            ocr_commands::publish_credential_status,
            ocr_commands::ping_publish_target,
            ocr_commands::open_publish_url,
            ocr_commands::set_ghost_credentials,
            ocr_commands::forget_ghost_credentials,
            ocr_commands::set_wordpress_credentials,
            ocr_commands::forget_wordpress_credentials,
        ])
        .build(tauri::generate_context!())
        .expect("error while building rehydrate")
        .run(|app, event| {
            // Signal background tasks (reachability watcher, progress
            // forwarders) that the app is going away so they can exit
            // their loops cleanly instead of being torn down with the
            // runtime.
            if let tauri::RunEvent::ExitRequested { .. } | tauri::RunEvent::Exit = event {
                let state = app.state::<AppState>();
                state
                    .shutdown_requested
                    .store(true, std::sync::atomic::Ordering::Release);
            }
        });
}
