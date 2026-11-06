//! Desktop adapters. Shared types are re-exported for existing integrations.
pub use deepseek_code_core::*;
pub mod bridge;
pub mod api;
pub mod commands;
pub mod updater;
pub mod remotion;
pub mod secure_storage;
pub mod launch_permissions;

/// Android and iOS entry point.
///
/// The app runs the engine in-process: the Bridge listens on loopback with a
/// per-process key, exactly like Desktop, and stores data in the app's private
/// directory. The login screen can instead connect the app to a dcd-server.
#[cfg(mobile)]
#[tauri::mobile_entry_point]
pub fn mobile_main() {
    use tauri::Manager;
    // stderr reaches logcat (tag RustStdoutStderr) on Android.
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::new("warn,LinuxEnv=info,Bridge=info"))
        .try_init();
    let result = tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_haptics::init())
        .plugin(tauri_plugin_barcode_scanner::init())
        .invoke_handler(tauri::generate_handler![commands::get_bridge_endpoint])
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            start_mobile_bridge(data_dir);
            Ok(())
        })
        .run(tauri::generate_context!());
    if let Err(error) = result {
        tracing::error!(module = "FATAL", error = %error);
    }
}

/// Starts the in-process Bridge used by the mobile app's local mode.
#[cfg(mobile)]
fn start_mobile_bridge(data_dir: std::path::PathBuf) {
    // The engine, tools and runtime paths resolve the data directory and home
    // from the environment; Android apps have neither by default.
    std::env::set_var("DEEPSEEK_CODE_DATA_DIR", &data_dir);
    if std::env::var_os("HOME").is_none() {
        std::env::set_var("HOME", &data_dir);
    }
    prepare_linux_environment(&data_dir);
    tauri::async_runtime::spawn(async move {
        let db = match db::DbManager::new(data_dir.join(deepseek_code_core::runtime::DATABASE_FILE)) {
            Ok(db) => std::sync::Arc::new(db),
            Err(error) => return tracing::error!(module = "Bridge", "Cannot open database: {error}"),
        };
        if let Err(error) = db.init() {
            return tracing::error!(module = "Bridge", "Cannot initialize database: {error}");
        }
        let bridge = bridge::BridgeServer::new(data_dir, db);
        if let Err(error) = bridge.start(30085).await {
            tracing::error!(module = "Bridge", "Server failed to start: {error}");
        }
    });
}

/// Directory the APK's native libraries were extracted to: the only app
/// location Android allows to execute, where PRoot ships as `libproot.so`.
#[cfg(mobile)]
fn native_library_dir() -> Option<std::path::PathBuf> {
    let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
    maps.lines()
        .filter_map(|line| line.split_whitespace().nth(5))
        .find(|path| path.ends_with("/libdeepseek_desktop_tauri_lib.so"))
        .and_then(|path| std::path::Path::new(path).parent().map(std::path::Path::to_path_buf))
}

/// Points the engine at the bundled PRoot and installs the Ubuntu root
/// filesystem on first launch, so model commands get a full Linux userland.
/// Until it is ready, commands run in the system shell.
#[cfg(mobile)]
fn prepare_linux_environment(data_dir: &std::path::Path) {
    use deepseek_code_core::tools::linux_env;
    let Some(libs) = native_library_dir() else {
        return tracing::warn!(module = "LinuxEnv", "Native library directory not found");
    };
    let proot = libs.join("libproot.so");
    if !proot.is_file() {
        return tracing::warn!(module = "LinuxEnv", "PRoot is not bundled in this build");
    }
    let rootfs = data_dir.join("linux").join("rootfs");
    std::env::set_var(linux_env::PROOT_ENV, &proot);
    std::env::set_var(linux_env::LOADER_ENV, libs.join("libproot-loader.so"));
    std::env::set_var(linux_env::ROOTFS_ENV, &rootfs);
    let Some((url, sha256)) = linux_env::default_rootfs() else { return };
    tauri::async_runtime::spawn(async move {
        tracing::info!(module = "LinuxEnv", "Installing Ubuntu environment");
        match linux_env::install(&rootfs, url, sha256).await {
            Ok(()) => tracing::info!(module = "LinuxEnv", "Ubuntu environment ready"),
            Err(error) => tracing::error!(module = "LinuxEnv", "Install failed: {error:#}"),
        }
    });
}
