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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum UpdateStatus {
    #[default]
    NotConfigured,
    Idle,
    Checking,
    UpToDate,
    Ready(String),
    Installing(String),
    Failed(String),
}
impl UpdateStatus {
    pub fn label(&self) -> String {
        match self {
            Self::NotConfigured => "Updates not configured".into(),
            Self::Idle => "Check for updates".into(),
            Self::Checking => "Checking for updates…".into(),
            Self::UpToDate => "Up to date · Check again".into(),
            Self::Ready(version) => format!("Update to {version}"),
            Self::Installing(_) => "Installing update…".into(),
            Self::Failed(_) => "Update check failed · Retry".into(),
        }
    }
    pub fn can_check(&self) -> bool {
        !matches!(
            self,
            Self::NotConfigured | Self::Checking | Self::Installing(_)
        )
    }
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
    pub update_status: UpdateStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayAction {
    CheckUpdates,
    Dismiss,
    Capture(CaptureMode),
    RequestPermission,
    ToggleLogin,
    LoginSettings,
    SetShortcuts(ShortcutStyle),
    Quit,
}
