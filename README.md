<img src="assets/icon.svg" width="72" height="72" alt="Still icon">

# Still

A native screenshot utility that lives in the macOS menu bar or Windows notification tray, written in Rust. There is no home window to open. Freeze the view, select a region, and carry on. macOS uses AppKit, ScreenCaptureKit, and CoreGraphics. Windows uses Win32, Direct2D, DirectWrite, and a pure Rust annotation rasterizer. PNG encoding runs in Rust on both platforms. Screenshots stay on your device. There are no accounts or screenshot history; network requests only check and download signed software updates from GitHub.

[Download the latest release](https://github.com/justerquate-sys/still/releases/latest) · [Source](https://github.com/justerquate-sys/still)

## Automatic updates

Install an updater-enabled release once on each device. Still checks GitHub shortly after launch and every six hours. It downloads and verifies newer releases in the background, then installs and restarts when no capture, annotation, export, or dropdown is open. The footer in the dropdown shows update status and lets you check immediately. Quitting normally does not force a pending restart.

Update metadata is signed with a pinned Ed25519 key, and each download must match its signed SHA-256 hash and size. A separate Rust helper waits for the app to exit, verifies the staged package again, and preserves the old version for recovery. Preferences and the clipboard are retained. Offline checks fail without interrupting capture.

Mac personal releases use the same app-specific signing certificate across versions. They are **not Apple-notarized**; macOS may require its normal first-install or Screen Recording approval. Still does not disable Gatekeeper, remove quarantine, or change certificate trust. Developer ID notarization can be configured later. See [release setup](docs/RELEASING.md) for the publishing workflow and signing requirements.

## Your two keys

| Key | From any app | Inside capture |
| --- | --- | --- |
| **↑** | Freeze the screen in copy mode | Switch to copy mode; copy if already drawing |
| **↓** | Freeze the screen in drawing mode | Switch to drawing mode; copy the finished drawing |
| **Control–Option Space / Ctrl+Alt+Space** | Freeze the screen in copy mode | — |
| **Esc** | — | Cancel without changing the clipboard |
| **Space** | — | Select the entire current display |
| **⌘C / Ctrl+C / Return** | — | Copy the selected image |
| **⌘Z / Ctrl+Z**; add **Shift** to redo | — | Undo / redo an annotation |
| **Type or paste** | — | Add text at the current annotation position |
| **Return / Shift–Return** while entering text | — | Commit text / insert a new line |
| **Tab / Shift–Tab** | — | Move through toolbar controls |

**Capture and copy:** Press **↑**, drag a region, release. It is on your clipboard.

**Capture and draw:** Press **↓**, drag a region, and release. The selection automatically zooms into a focused canvas with the pen ready. Press **↓** again to copy the image and annotations. Export keeps the original pixel resolution.

Plain **↑ / ↓** are the default global shortcuts. Switch to **Control–Option + ↑ / ↓** on Mac or **Ctrl+Alt + ↑ / ↓** on Windows in the icon’s menu if desired. Left and Right remain available to other apps. Carbon hotkeys on Mac and `RegisterHotKey` on Windows need no keyboard hook or input-monitoring permission. Conflicts are reported, and the icon’s menu remains available for capture.

The annotation toolbar contains pen, arrow, rectangle, highlighter, text, pixelation rectangle, and pixelation brush. Select a tool or color with its button; letters and numbers always remain text. Click to position text, then type, paste, or dictate. Return commits the text; Return with an empty editor or **↓** copies the finished capture. Escape clears a draft first, then cancels capture. macOS uses a native `NSTextView` and Windows uses a native `EDIT` control, preserving normal keyboard layouts, IME composition, selection, and paste. The Mac field exposes text and selection through Accessibility for tools such as Sotto; live Sotto audio delivery has not been tested here.

Pixelation uses square mosaic cells like the supplied reference. Drag a rectangle for a whole area or use the round brush for precise masking. It includes annotations already on the image, supports undo/redo, and is flattened into the copied pixels.

## Build and install on macOS

Requires **macOS 14 or later**, Xcode Command Line Tools, and **Rust/Cargo 1.99.1**. `rust-toolchain.toml` pins the toolchain, including Rustfmt and Clippy. Install Rust through [rustup](https://rust-lang.org/tools/install/); Cargo is normally in `~/.cargo/bin`.

```sh
./scripts/build.sh
./scripts/install.sh
```

The release build produces `build/Still.app`. Installation checks the bundle identity and signature, preserves a unique backup under `build/backups`, and installs to `~/Applications/Still.app`. Quit Still before installation. Build output and installed bundles are staged before replacement; failed replacement restores the previous bundle.

Opening Still adds its crop icon to the menu bar without opening an app window or a Dock icon. Click the icon to open a compact dropdown containing capture, Screen Recording access, launch at login, shortcut preferences, update status, and Quit. Click outside or press Escape to dismiss it. It uses macOS ServiceManagement for launch at login, respects the system’s actual setting, and preserves the previous version’s shortcut preference, login state, and capture count. Opening the app again reveals that same dropdown or active capture.

Enable **Screen Recording / Screen & System Audio Recording** when macOS requests it. If macOS asks you to quit and reopen, do so. Local builds are ad hoc signed; rebuilding can require renewed permission. For distribution, use the persistent personal signing or Developer ID packaging described in [release setup](docs/RELEASING.md).

## Windows download and build

Portable Windows packages are on [the latest GitHub release](https://github.com/justerquate-sys/still/releases/latest). Extract it into a permanent folder and open `Still.exe`. The app starts in the notification area without a home window. Click the crop icon for the compact dropdown; right-click for the native fallback menu. Enable **Launch at login** there after placing it in that folder. It uses the current user's startup registry entry; no administrator privileges are required. Turn that preference off before removing or moving the executable.

The Windows port targets **Windows 10 version 1803+ and Windows 11**, with release builds for x64 and ARM64. The local package is cross-compiled on Mac and unsigned. Shared logic and image tests run on Mac; actual Windows desktop behavior and 240 Hz hardware presentation still need Windows hardware validation. Native Windows CI runs unit tests and the executable self-test.

To build on Windows, install Rust/Cargo **1.99.1** and Microsoft's C++ Build Tools with the Windows SDK. In PowerShell:

```powershell
cargo test --target x86_64-pc-windows-msvc --all-targets --locked
cargo clippy --target x86_64-pc-windows-msvc --all-targets --locked -- -D warnings
./scripts/build-windows.ps1
```

This produces the portable ZIP with dependency licenses and a SHA-256 checksum. Native MSVC builds statically link the C runtime. Resource generation, including the original icon, DPI manifest, and file version, is Rust code in `build.rs`; no extra resource compiler is needed for LLVM cross builds. The PowerShell script only builds and packages the app.

Windows capture uses a single frozen virtual-desktop image with physical pixel coordinates, including monitors with negative origins and different scaling. Direct2D retains uploaded BGRA textures. Committed marks are rasterized on background workers; only the current stroke and uncached marks use dynamic vector drawing. PNG and DIBV5 clipboard formats preserve original pixel dimensions. Capture and export have timeouts, canceled workers cannot write to the clipboard, and a busy clipboard leaves the crop available for retry. Secure/protected surfaces cannot be captured; HDR color fidelity and very large desktops beyond the GPU's texture limit are not guaranteed.

`Still.exe --self-test` exercises image encoding, clipboard buffer allocation, the native timer, native Unicode text insertion, text rasterization, both pixelation tools, and Direct2D/DirectWrite in a hidden window without changing preferences, registering shortcuts, or touching the system clipboard. Use `Start-Process -Wait -PassThru` in PowerShell to read its exit code. `Still.exe --ui-test` provides the synthetic capture/drawing workflow with private copy validation. Quit the ordinary app first.

## Interface

Still uses graphite surfaces, the original lime crop mark, and paired ↑/↓ keycaps. A compact dropdown attached to the icon is the resting interface. A compact toolbar appears only during capture; after zooming into the crop, a single bottom toolbar holds the annotation controls. Bright red, lime, cyan, and white ink use a dark outline to remain readable over screenshots. Interface accents stay separate from annotation colors. Mac controls use SF Symbols, system type, and keyboard focus rings.

Mac design values live in `src/ui.rs`; Windows rendering lives in `src/windows`. Both panels use graphite, lime, the crop logo, and system typography (SF on Mac, Segoe UI on Windows). Spacing follows a four-point scale. Mac hover and selection feedback lasts 120 ms and control/toast fades last 160 ms; the crop zoom lasts 180 ms on both platforms and respects the OS animation preference. Pointer tracking has no easing or trailing animation. Windows includes keyboard focus and scaled controls; its custom controls have not yet been validated with Windows screen readers.

The interface was redesigned with [unslop](https://github.com/heydamir/unslop) by Damir Sokolovsky ([@heydamir](https://github.com/heydamir)), used under [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/). Its design guidance was adapted for native Rust/AppKit rather than web components.

## Refresh rate and image handling

Both platforms target the active display's refresh rate **up to 240 Hz**, defined in `src/refresh.rs`. A 240 Hz display must be configured to that refresh rate in the operating system; a 120 Hz panel can display at most 120 frames per second. macOS uses a view-bound display link. Windows uses a high-resolution one-shot timer, keeps deadlines from drifting, skips missed frames after stalls, and lets DWM present the window. Neither platform continuously redraws at rest. These targets are requests to the OS, not a guarantee of compositor presentation under all GPU loads.

The macOS pipeline also provides:

- Display snapshots run concurrently. All required images arrive before selection overlays appear; failed or disconnected displays never produce a partial capture.
- PNG/TIFF encoding and annotation rasterization run on background queues. Canceling invalidates pending work before it can touch the clipboard.
- Mosaic preparation runs on one background worker per capture revision. It uses the current annotated image, rejects stale results after undo/redo, and stores only each committed effect’s affected image patch. Pointer movement does no mosaic resampling.
- Frozen screenshots and cached annotations stay in Core Animation image layers. Selection movement changes four dimming rectangles; zoom transforms the crop layer. Neither resamples the entire screenshot on the CPU each frame. Toolbar panels and symbols have separate backing layers and stay out of pointer redraws. Undo/redo rebuilds the annotation cache without blocking input, and stale workers cannot restore removed marks.
- A view-bound `CADisplayLink` requests the current display's maximum refresh rate up to 240 Hz, including 120 Hz ProMotion. Frames use the latest pointer state, with no implicit easing on pointer movement. The 180 ms crop zoom uses the upcoming presentation timestamp and respects Reduce Motion. Its backing dimensions and shadow mask remain fixed throughout the transform. See Apple's [AppKit display-link introduction](https://developer.apple.com/videos/play/wwdc2023/10054/).
- PNG uses lossless Rust encoding with sRGB tagging. The clipboard contains PNG and TIFF. Pixel layout, color space, transparency, Retina crop rounding, and image orientation are checked explicitly.
- There is no continuous recording or idle polling. The display link pauses when the overlay stops changing and is invalidated when capture closes.
- Screenshots remain in memory. Each selection belongs to one display; mixed display scales are handled independently. Apps continue running underneath the frozen view. macOS capture restrictions still apply to protected content.

The app uses typed `objc2` bindings to Apple frameworks without a Swift or Objective-C bridge. Release builds use thin LTO. Image and stroke allocations are bounded, and the overlay remains copyable/cancelable at its annotation limit.

All application code is Rust, including UI layout and drawing, control behavior, animation timing, display callbacks, capture orchestration, annotation history, and clipboard export. Apple frameworks and Win32 supply native operating-system facilities through typed Rust bindings. There is no web view or JavaScript UI. Build scripts invoke Cargo and platform packaging tools.

## Verification

```sh
cargo test --all-targets --locked
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo audit
./scripts/build.sh

# These checks use no general clipboard contents:
build/Still.app/Contents/MacOS/Still --check-clipboard

# Quit the running app first so its own keys do not conflict:
build/Still.app/Contents/MacOS/Still --check-hotkeys

# Requires Screen Recording; writes no screenshot or clipboard data:
build/Still.app/Contents/MacOS/Still --check-capture

# Menu-bar-only startup and native text/pixelation diagnostics:
build/Still.app/Contents/MacOS/Still --check-menu
build/Still.app/Contents/MacOS/Still --check-panel
build/Still.app/Contents/MacOS/Still --check-annotations

# Interactive native UI with a synthetic desktop and private clipboard:
build/Still.app/Contents/MacOS/Still --demo --ui-test

# Native selection, zoom, drawing cadence, and idle-pause diagnostic:
build/Still.app/Contents/MacOS/Still --check-rendering
```

`--demo` immediately opens a synthetic drawing capture. Combine it with `--ui-test` to use the private clipboard. UI-test mode skips login registration, global shortcut registration, and preference writes. Capture buttons still exercise the real selection, zoom, tools, export, and dismissal paths; plain arrows work inside its overlay.

`--check-menu` verifies that startup creates the native status icon and hidden dropdown without a standalone app window. `--check-panel` opens the actual native popover attached to that icon, checks its compact dimensions and dismissal/reopening, renders only its own controls, then quits. `--panel-demo` opens the dropdown in isolated test mode; its capture actions use synthetic pixels and a private clipboard. `--check-annotations` checks native Unicode paste and Accessibility text readback, text commit, both pixelation tools, undo/redo, and original-size PNG export. It writes only its own synthetic fixture and interface render under `artifacts/`, then quits. Neither diagnostic changes the system clipboard or user preferences.

`--check-rendering` automatically uses that same isolated test mode, runs a synthetic selection/zoom/stroke sequence on the attached screen, prints actual callback arrival intervals separately from display-link timestamps and frame-preparation timing, verifies that the display link pauses at rest, then quits. These timings measure AppKit callbacks and main-thread preparation; they do not measure hardware input latency or prove that every frame was presented by the compositor. macOS still controls adaptive refresh and power limits.

The tests cover geometry and invalid inputs, Retina and fractional crops, orientation, all drawing tools, alpha blending, original-image immutability, PNG/TIFF round trips, BGRA stride handling, sRGB conversion, asynchronous capture ordering and timeouts, stroke limits, undo/redo, and incremental raster equivalence. A model test interleaves 5,000 history/worker operations to check stale-cache rejection.

Run the release benchmarks separately from interactive testing:

```sh
cargo run --release --bin still-bench -- \
  --iterations 21 --texture --compare-png --stroke-study \
  --json artifacts/rust-benchmark.json
```

Benchmarks use deterministic synthetic images, including a textured stress case. They measure crop, rasterization, PNG/TIFF encoding, and cached drawing separately; they do not represent end-to-end capture latency. Actual latency depends on display setup, content, and macOS.

The original Swift source and app from this local migration are preserved under the ignored `build/backups` directory. The active application and tests are Rust.
