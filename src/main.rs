#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
#[cfg(target_os = "macos")]
mod app;
#[cfg(target_os = "macos")]
mod hotkeys;
#[cfg(target_os = "macos")]
mod preferences;
fn main() {
    #[cfg(target_os = "macos")]
    app::run();
    #[cfg(target_os = "windows")]
    still::windows::run();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    eprintln!("Still currently supports macOS and Windows.");
}
