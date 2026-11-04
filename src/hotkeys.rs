//! Carbon hotkeys reserve only the user's chosen keys, without an event tap or
//! Accessibility permission. Registration and teardown run on the main thread.
use std::{
    ffi::c_void,
    ptr,
    sync::atomic::{AtomicU8, Ordering},
};
use still::model::ShortcutStyle;

type EventRef = *mut c_void;
type HandlerRef = *mut c_void;
type HotKeyRef = *mut c_void;
type Status = i32;
const KEYBOARD: u32 = u32::from_be_bytes(*b"keyb");
const PRESSED: u32 = 5;
const RELEASED: u32 = 6;
const SIGNATURE: u32 = u32::from_be_bytes(*b"STIL");
const NOT_HANDLED: Status = -9874;
static HELD: AtomicU8 = AtomicU8::new(0);

#[repr(C)]
struct EventTypeSpec {
    class: u32,
    kind: u32,
}
#[repr(C)]
#[derive(Default)]
struct HotKeyId {
    signature: u32,
    id: u32,
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn GetApplicationEventTarget() -> *mut c_void;
    fn InstallEventHandler(
        target: *mut c_void,
        handler: unsafe extern "C" fn(*mut c_void, EventRef, *mut c_void) -> Status,
        count: usize,
        events: *const EventTypeSpec,
        context: *mut c_void,
        result: *mut HandlerRef,
    ) -> Status;
    fn RemoveEventHandler(handler: HandlerRef) -> Status;
    fn GetEventKind(event: EventRef) -> u32;
    fn GetEventParameter(
        event: EventRef,
        name: u32,
        ty: u32,
        actual_type: *mut u32,
        buffer_size: usize,
        actual_size: *mut usize,
        data: *mut c_void,
    ) -> Status;
    fn RegisterEventHotKey(
        key: u32,
        modifiers: u32,
        id: HotKeyId,
        target: *mut c_void,
        options: u32,
        result: *mut HotKeyRef,
    ) -> Status;
    fn UnregisterEventHotKey(hotkey: HotKeyRef) -> Status;
}

unsafe extern "C" fn handler(_: *mut c_void, event: EventRef, _: *mut c_void) -> Status {
    if event.is_null() {
        return NOT_HANDLED;
    }
    let mut id = HotKeyId::default();
    // SAFETY: Carbon supplies a live EventRef; the destination has precisely the
    // C EventHotKeyID layout and size, and no pointer escapes this callback.
    let status = unsafe {
        GetEventParameter(
            event,
            u32::from_be_bytes(*b"----"),
            u32::from_be_bytes(*b"hkid"),
            ptr::null_mut(),
            std::mem::size_of::<HotKeyId>(),
            ptr::null_mut(),
            (&mut id as *mut HotKeyId).cast(),
        )
    };
    if status != 0 || id.signature != SIGNATURE || !(1..=3).contains(&id.id) {
        return NOT_HANDLED;
    }
    let bit = 1 << (id.id - 1);
    // SAFETY: event was validated above and is live for this callback.
    match unsafe { GetEventKind(event) } {
        PRESSED => {
            if HELD.fetch_or(bit, Ordering::Relaxed) & bit == 0 {
                crate::app::enqueue(move |app| app.hotkey(id.id));
            }
        }
        RELEASED => {
            HELD.fetch_and(!bit, Ordering::Relaxed);
        }
        _ => return NOT_HANDLED,
    }
    0
}

pub fn reset_held() {
    HELD.store(0, Ordering::Relaxed);
}

pub struct Hotkeys {
    references: Vec<HotKeyRef>,
    handler: HandlerRef,
}

impl Hotkeys {
    pub fn new() -> Result<Self, String> {
        let events = [
            EventTypeSpec {
                class: KEYBOARD,
                kind: PRESSED,
            },
            EventTypeSpec {
                class: KEYBOARD,
                kind: RELEASED,
            },
        ];
        let mut result = ptr::null_mut();
        // SAFETY: main thread, matching C ABI; the callback has no borrowed context.
        let status = unsafe {
            InstallEventHandler(
                GetApplicationEventTarget(),
                handler,
                2,
                events.as_ptr(),
                ptr::null_mut(),
                &mut result,
            )
        };
        if status != 0 || result.is_null() {
            return Err(format!("Couldn’t listen for shortcuts ({status})."));
        }
        Ok(Self {
            references: Vec::with_capacity(3),
            handler: result,
        })
    }

    pub fn register(&mut self, style: ShortcutStyle) -> Option<String> {
        self.clear();
        let modifiers = if style == ShortcutStyle::Modified {
            (1 << 12) | (1 << 11)
        } else {
            0
        };
        let mut failed = Vec::new();
        for (id, key, modifier, label) in [
            (1, 126, modifiers, "↑"),
            (2, 125, modifiers, "↓"),
            (3, 49, (1 << 12) | (1 << 11), "⌃⌥ Space"),
        ] {
            let mut reference = ptr::null_mut();
            // SAFETY: main thread; Carbon owns the returned opaque handle until unregistered.
            let status = unsafe {
                RegisterEventHotKey(
                    key,
                    modifier,
                    HotKeyId {
                        signature: SIGNATURE,
                        id,
                    },
                    GetApplicationEventTarget(),
                    1,
                    &mut reference,
                )
            };
            if status == 0 && !reference.is_null() {
                self.references.push(reference);
            } else {
                failed.push(label);
            }
        }
        (!failed.is_empty()).then(|| format!("Shortcut unavailable: {}. Another app may be using it. You can still capture from the menu bar.",failed.join(", ")))
    }

    /// Exercise Carbon's real conflict behavior without installing a duplicate handler.
    pub fn verify_conflict(&self) -> Result<(), String> {
        let mut duplicate = ptr::null_mut();
        // SAFETY: diagnostic-only registration uses the same documented ABI;
        // any unexpectedly returned handle is immediately unregistered.
        let status = unsafe {
            RegisterEventHotKey(
                126,
                0,
                HotKeyId {
                    signature: SIGNATURE,
                    id: 900,
                },
                GetApplicationEventTarget(),
                1,
                &mut duplicate,
            )
        };
        if !duplicate.is_null() {
            unsafe {
                UnregisterEventHotKey(duplicate);
            }
        }
        if status == 0 {
            Err("Duplicate exclusive hotkeys were not reported.".into())
        } else {
            Ok(())
        }
    }

    fn clear(&mut self) {
        for reference in self.references.drain(..) {
            // SAFETY: every handle belongs to this instance and is unregistered once.
            unsafe {
                UnregisterEventHotKey(reference);
            }
        }
        HELD.store(0, Ordering::Relaxed);
    }
}

impl Drop for Hotkeys {
    fn drop(&mut self) {
        self.clear();
        // SAFETY: installed by this instance and not previously removed.
        unsafe {
            RemoveEventHandler(self.handler);
        }
    }
}
