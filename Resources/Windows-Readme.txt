Still for Windows

Extract the whole ZIP into a permanent folder, then open Still.exe.
No installer, account, administrator privileges, or extra runtime is required.
The packaged x64 edition targets Windows 10 (1803+) and Windows 11 on Intel/AMD.

Up arrow: freeze the desktop, drag an area, release to copy.
Down arrow: freeze, drag, release, then draw on the enlarged crop.
Down arrow again: copy the image with your annotations.
Escape: cancel. Space while selecting: capture the current display.
Ctrl+C or Enter: copy. Ctrl+Z / Ctrl+Shift+Z: undo / redo.
Type or paste to add text; letters and numbers remain text.
Return commits a draft. Shift+Return adds a line. Empty Return copies.
Choose pen, arrow, rectangle, highlighter, text, pixelation rectangle or brush
from the toolbar. The rectangle masks a whole area; the brush masks precisely.
Bright ink has a dark outline. Text and pixelation are included in the copy.

Still lives in the notification area with no home window.
Click the crop icon for its compact dropdown with capture, preferences, and Quit.
The panel opens beside the actual icon and follows the taskbar placement.
Click outside, click the icon again, or press Escape to dismiss it.
Right-click the icon for the native fallback menu.
Windows may put this icon in the hidden-icons menu.
Enable Launch at login after placing the app in its permanent folder.
Choose Ctrl+Alt arrows in the tray menu if you want ordinary arrows elsewhere.
The global freeze shortcut is Ctrl+Alt+Space.

Automatic updates
Install an updater-enabled release once. Still checks GitHub after startup and
every six hours, verifies signed metadata and download hashes, and restarts only
when captures, exports, and the dropdown are closed. The footer checks manually.
Preferences and clipboard contents stay intact. Offline checks do not stop capture.

240 Hz
Still targets the active monitor's configured refresh rate, up to 240 Hz.
Pointer movement has no easing. Only the automatic crop zoom is animated;
it respects Windows' animation setting. Idle Still does not continuously redraw.
Set your monitor to 240 Hz in Windows Display settings to use that refresh rate.
Actual frame presentation depends on your monitor, Windows, and GPU load.

Images stay in memory. PNG and DIBV5 clipboard data retain the crop's original
pixel dimensions. There is no recording history. Network requests only check
and download signed software updates from GitHub.
Windows' protected/secure surfaces cannot be captured. Capture uses SDR pixels;
HDR fidelity is not guaranteed. Very large desktops can exceed a GPU limit.

Validation status
This Windows package was cross-compiled and statically checked on macOS.
Shared geometry, image, history, and refresh-policy tests passed on macOS.
It has not yet been exercised on a Windows desktop or a 240 Hz monitor.
It is unsigned; Windows may show a publisher/reputation prompt.
Do not disable Windows security settings to use it.

Developer checks on Windows
Still.exe --self-test validates image/clipboard encoding, native Unicode input,
text rendering, both pixelation tools, and Direct2D in a hidden window without
touching the system clipboard. In PowerShell, use Start-Process -Wait -PassThru to inspect
its ExitCode. --ui-test opens a synthetic capture workflow, skips preference
writes/global hotkeys, and validates copies privately. Quit normal Still first.

To remove: turn Launch at login off, quit from the tray, and delete the folder.
