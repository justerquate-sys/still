//! One-shot, concurrent ScreenCaptureKit snapshots. No stream or polling runs
//! while Still is idle, and all displays are ready before an overlay is shown.

use std::ptr::NonNull;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueGlobalPriority, DispatchTime, GlobalQueueIdentifier};
use objc2::{AllocAnyThread, rc::autoreleasepool};
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{CGImage, kCGColorSpaceSRGB};
use objc2_foundation::{NSArray, NSError, NSRect};
use objc2_screen_capture_kit::{
    SCCaptureResolutionType, SCContentFilter, SCRunningApplication, SCScreenshotManager,
    SCShareableContent, SCStreamConfiguration, SCWindow,
};

use crate::imaging::{Image, validate_dimensions};

#[derive(Clone, Debug)]
pub struct DisplayRequest {
    pub id: u32,
    pub frame: NSRect,
    pub width: usize,
    pub height: usize,
}

#[derive(Clone, Debug)]
pub struct FrozenDisplay {
    pub request: DisplayRequest,
    pub image: Image,
}

type CaptureResult = Result<Vec<FrozenDisplay>, String>;
type Completion = Box<dyn FnOnce(CaptureResult) + Send + 'static>;

struct Batch {
    results: Vec<Option<Result<FrozenDisplay, String>>>,
    remaining: usize,
    completion: Option<Completion>,
}

impl Batch {
    fn new(count: usize, completion: Completion) -> Self {
        Self {
            results: (0..count).map(|_| None).collect(),
            remaining: count,
            completion: Some(completion),
        }
    }

    fn record(shared: &Mutex<Self>, index: usize, result: Result<FrozenDisplay, String>) {
        let ready = {
            let mut batch = shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if batch.completion.is_none()
                || batch.results.get(index).is_none_or(|slot| slot.is_some())
            {
                return;
            }
            batch.results[index] = Some(result);
            batch.remaining -= 1;
            if batch.remaining == 0 {
                let results = std::mem::take(&mut batch.results)
                    .into_iter()
                    .map(|entry| {
                        entry.unwrap_or_else(|| Err("A display snapshot did not arrive.".into()))
                    })
                    .collect::<Result<Vec<_>, _>>();
                batch
                    .completion
                    .take()
                    .map(|completion| (completion, results))
            } else {
                None
            }
        };
        // User code must never execute while the batch mutex is held.
        if let Some((completion, results)) = ready {
            completion(results);
        }
    }

    fn fail(shared: &Mutex<Self>, message: String) {
        let completion = {
            let mut batch = shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            batch.results.clear();
            batch.remaining = 0;
            batch.completion.take()
        };
        if let Some(completion) = completion {
            completion(Err(message));
        }
    }
}

/// Start one snapshot per display. Completion is invoked once, on an arbitrary
/// thread, with results in the same order as `requests`. Caller presents UI on
/// the main queue and discards results for canceled capture generations.
pub fn capture_displays(
    requests: Vec<DisplayRequest>,
    completion: impl FnOnce(CaptureResult) + Send + 'static,
) {
    if requests.is_empty() {
        completion(Err("No connected display is available.".into()));
        return;
    }
    for request in &requests {
        if let Err(error) = validate_dimensions(request.width, request.height) {
            completion(Err(error));
            return;
        }
    }
    let batch = Arc::new(Mutex::new(Batch::new(requests.len(), Box::new(completion))));
    // A delayed one-shot only holds a Weak reference: successful snapshots are
    // freed immediately, not kept alive until the timeout fires.
    let weak_batch = Arc::downgrade(&batch);
    if let Ok(deadline) = DispatchTime::try_from(Duration::from_secs(12)) {
        let _ = DispatchQueue::global_queue(GlobalQueueIdentifier::Priority(DispatchQueueGlobalPriority::Default))
            .after(deadline, move || {
                if let Some(batch) = weak_batch.upgrade() {
                    Batch::fail(&batch, "Screen capture timed out. Try again, or check Screen Recording access in System Settings.".into());
                }
            });
    }
    let callback = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            autoreleasepool(|_| {
                // SAFETY: ScreenCaptureKit supplies borrowed callback arguments,
                // valid through this invocation; each pointer is checked for null.
                if let Some(error) = unsafe { error.as_ref() } {
                    Batch::fail(&batch, error.localizedDescription().to_string());
                    return;
                }
                let Some(content) = (unsafe { content.as_ref() }) else {
                    Batch::fail(
                        &batch,
                        "ScreenCaptureKit returned no display information.".into(),
                    );
                    return;
                };
                if batch
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .completion
                    .is_none()
                {
                    return;
                }
                // SAFETY: These are documented immutable SCK content properties.
                let displays = unsafe { content.displays() };
                let applications = unsafe { content.applications() };
                let own_apps: Vec<_> = applications
                    .iter()
                    .filter(|app| unsafe { app.processID() } as u32 == std::process::id())
                    .collect();
                let excluded = NSArray::<SCRunningApplication>::from_retained_slice(&own_apps);
                let exceptions = NSArray::<SCWindow>::new();
                for (index, request) in requests.iter().enumerate() {
                    let Some(display) = displays
                        .iter()
                        .find(|display| unsafe { display.displayID() } == request.id)
                    else {
                        Batch::record(
                            &batch,
                            index,
                            Err("A display disconnected while capturing. Try again.".into()),
                        );
                        continue;
                    };
                    // SAFETY: All arguments are live retained framework objects.
                    // Excluding-app filters include the menu bar by default, so we
                    // avoid the macOS 14.2-only includeMenuBar setter on macOS 14.0.
                    let filter = unsafe {
                        SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
                            SCContentFilter::alloc(),
                            &display,
                            &excluded,
                            &exceptions,
                        )
                    };
                    let configuration = unsafe { SCStreamConfiguration::new() };
                    unsafe {
                        configuration.setWidth(request.width);
                        configuration.setHeight(request.height);
                        configuration.setShowsCursor(false);
                        configuration.setCaptureResolution(SCCaptureResolutionType::Best);
                        configuration.setColorSpaceName(kCGColorSpaceSRGB);
                    }
                    let batch = Arc::clone(&batch);
                    let request = request.clone();
                    let callback = RcBlock::new(move |image: *mut CGImage, error: *mut NSError| {
                        autoreleasepool(|_| {
                            // SAFETY: Same borrowed callback argument guarantees as
                            // above; retain the CGImage before the callback returns.
                            let result = if let Some(error) = unsafe { error.as_ref() } {
                                Err(error.localizedDescription().to_string())
                            } else if let Some(image) = NonNull::new(image) {
                                Image::from_cg(unsafe { CFRetained::retain(image) }).map(|image| {
                                    FrozenDisplay {
                                        request: request.clone(),
                                        image,
                                    }
                                })
                            } else {
                                Err("ScreenCaptureKit returned an empty screenshot.".into())
                            };
                            Batch::record(&batch, index, result);
                        });
                    });
                    // SAFETY: SCK copies the completion block and retains its
                    // configuration/filter for the asynchronous snapshot request.
                    unsafe {
                        SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
                            &filter,
                            &configuration,
                            Some(&callback),
                        );
                    }
                }
            });
        },
    );
    // SAFETY: SCK copies the callback block for asynchronous invocation.
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            false, true, &callback,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imaging::synthetic;
    use objc2_foundation::{NSPoint, NSSize};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn display(id: u32) -> FrozenDisplay {
        FrozenDisplay {
            request: DisplayRequest {
                id,
                frame: NSRect::new(NSPoint::ZERO, NSSize::new(4.0, 4.0)),
                width: 4,
                height: 4,
            },
            image: synthetic(4, 4).unwrap(),
        }
    }

    #[test]
    fn concurrent_completion_preserves_display_order_and_completes_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let output = Arc::new(Mutex::new(Vec::new()));
        let (call_count, captured) = (Arc::clone(&calls), Arc::clone(&output));
        let batch = Arc::new(Mutex::new(Batch::new(
            3,
            Box::new(move |result| {
                call_count.fetch_add(1, Ordering::SeqCst);
                *captured.lock().unwrap() = result
                    .unwrap()
                    .into_iter()
                    .map(|display| display.request.id)
                    .collect();
            }),
        )));
        let mut threads = Vec::new();
        for index in [2, 0, 1] {
            let batch = Arc::clone(&batch);
            threads.push(std::thread::spawn(move || {
                Batch::record(&batch, index, Ok(display(index as u32)))
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        Batch::record(&batch, 0, Ok(display(99)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(*output.lock().unwrap(), vec![0, 1, 2]);
    }

    #[test]
    fn timeout_ignores_late_completions() {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&calls);
        let batch = Mutex::new(Batch::new(
            1,
            Box::new(move |result| {
                assert_eq!(result.unwrap_err(), "timeout");
                count.fetch_add(1, Ordering::SeqCst);
            }),
        ));
        Batch::fail(&batch, "timeout".into());
        Batch::record(&batch, 0, Ok(display(1)));
        Batch::fail(&batch, "again".into());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn one_display_error_rejects_partial_capture() {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&calls);
        let batch = Mutex::new(Batch::new(
            2,
            Box::new(move |result| {
                assert_eq!(result.unwrap_err(), "disconnected");
                count.fetch_add(1, Ordering::SeqCst);
            }),
        ));
        Batch::record(&batch, 1, Err("disconnected".into()));
        Batch::record(&batch, 0, Ok(display(1)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
