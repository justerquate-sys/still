use objc2_foundation::{NSBundle, NSString, NSUserDefaults, ns_string};
use objc2_service_management::{SMAppService, SMAppServiceStatus};
use still::model::{ShortcutStyle, TrayState};

pub fn load() -> TrayState {
    let defaults = NSUserDefaults::standardUserDefaults();
    let shortcut_style = match defaults
        .stringForKey(ns_string!("shortcutStyle"))
        .map(|s| s.to_string())
        .as_deref()
    {
        Some("modified") => ShortcutStyle::Modified,
        _ => ShortcutStyle::Plain,
    };
    let mut state = TrayState {
        screen_access: false,
        launch_at_login: false,
        login_needs_approval: false,
        shortcut_style,
        is_capturing: false,
        copied_count: defaults.integerForKey(ns_string!("copiedCount")).max(0) as usize,
        error: None,
        update_status: Default::default(),
    };
    refresh(&mut state);
    state
}

pub fn refresh(state: &mut TrayState) {
    // SAFETY: screen access queries are process-local; ServiceManagement's main
    // application API is used on the main thread for our own signed bundle.
    unsafe {
        state.screen_access = objc2_core_graphics::CGPreflightScreenCaptureAccess();
        let status = SMAppService::mainAppService().status();
        state.launch_at_login = status == SMAppServiceStatus::Enabled;
        state.login_needs_approval = status == SMAppServiceStatus::RequiresApproval;
    }
}

pub fn save_shortcuts(style: ShortcutStyle) {
    let value = NSString::from_str(if style == ShortcutStyle::Plain {
        "plain"
    } else {
        "modified"
    });
    // SAFETY: NSString is a valid property-list value.
    unsafe {
        NSUserDefaults::standardUserDefaults()
            .setObject_forKey(Some(&value), ns_string!("shortcutStyle"));
    }
}

pub fn did_copy(state: &mut TrayState, persist: bool) {
    state.copied_count = state
        .copied_count
        .saturating_add(1)
        .min(isize::MAX as usize);
    if persist {
        NSUserDefaults::standardUserDefaults()
            .setInteger_forKey(state.copied_count as isize, ns_string!("copiedCount"));
    }
}

pub fn set_login(enabled: bool) -> Result<(), String> {
    // SAFETY: operated on the application's main app service on the main thread.
    unsafe {
        let service = SMAppService::mainAppService();
        let status = service.status();
        if (enabled
            && (status == SMAppServiceStatus::Enabled
                || status == SMAppServiceStatus::RequiresApproval))
            || (!enabled && status == SMAppServiceStatus::NotRegistered)
        {
            return Ok(());
        }
        let result = if enabled {
            service.registerAndReturnError()
        } else {
            service.unregisterAndReturnError()
        };
        result.map_err(|e| {
            format!(
                "Couldn’t update launch at login. {}",
                e.localizedDescription()
            )
        })
    }
}

pub fn setup_login() -> Result<(), String> {
    let defaults = NSUserDefaults::standardUserDefaults();
    let path = NSBundle::mainBundle().bundlePath().to_string();
    let personal = std::env::var("HOME")
        .map(|h| format!("{h}/Applications/"))
        .unwrap_or_default();
    if !(path.starts_with("/Applications/")
        || (!personal.is_empty() && path.starts_with(&personal)))
        || defaults.boolForKey(ns_string!("didSetUpLogin"))
    {
        return Ok(());
    }
    // SAFETY: own main app service, main thread.
    if unsafe { SMAppService::mainAppService().status() } == SMAppServiceStatus::NotRegistered {
        set_login(true)?;
    }
    defaults.setBool_forKey(true, ns_string!("didSetUpLogin"));
    Ok(())
}
