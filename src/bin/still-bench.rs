#[cfg(target_os = "macos")]
#[path = "../bench_macos.rs"]
mod macos;
fn main() {
    #[cfg(target_os = "macos")]
    macos::main();
    #[cfg(not(target_os = "macos"))]
    eprintln!("This benchmark measures the macOS renderer. Use Still --self-test on Windows.");
}
