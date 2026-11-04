#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
#[cfg(target_os = "macos")]
mod app;
#[cfg(target_os = "macos")]
mod hotkeys;
#[cfg(target_os = "macos")]
mod preferences;
fn main() {
    if std::env::args().any(|arg| arg == "--update-info") {
        println!(
            "{}",
            serde_json::json!({
                "app_id": "app.still.screenshot",
                "version": env!("CARGO_PKG_VERSION"),
                "platform": still::updater::platform(),
            })
        );
        return;
    }
    match still::updater::run_helper_from_args() {
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            eprintln!("Still update failed: {error}");
            std::process::exit(1);
        }
    }
    #[cfg(target_os = "macos")]
    app::run();
    #[cfg(target_os = "windows")]
    still::windows::run();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    eprintln!("Still currently supports macOS and Windows.");
}
