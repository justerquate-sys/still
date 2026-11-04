#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureMode {
    Copy,
    Markup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShortcutStyle {
    Plain,
    Modified,
}

#[derive(Clone, Debug)]
pub struct TrayState {
    pub screen_access: bool,
    pub launch_at_login: bool,
    pub login_needs_approval: bool,
    pub shortcut_style: ShortcutStyle,
    pub is_capturing: bool,
    pub copied_count: usize,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayAction {
    Dismiss,
    Capture(CaptureMode),
    RequestPermission,
    ToggleLogin,
    LoginSettings,
    SetShortcuts(ShortcutStyle),
    Quit,
}
