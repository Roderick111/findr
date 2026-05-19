//! macOS FSEvents integration — reads the kernel filesystem change journal.
//! Uses fsevent-sys which is deprecated in favor of objc2-core-services.
//! Migration tracked but not urgent — fsevent-sys is stable thin FFI over Apple's C API.
#![allow(deprecated)]

use core_foundation::array::CFArray;
use core_foundation::base::TCFType;
use core_foundation::runloop::{kCFRunLoopDefaultMode, CFRunLoop};
use core_foundation::string::CFString;
use fsevent_sys::*;
use std::ffi::CStr;
use std::os::raw::c_void;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

pub struct FsChange {
    pub path: String,
    pub created: bool,
    pub modified: bool,
    pub removed: bool,
    pub renamed: bool,
    pub must_scan_dir: bool,
}

pub struct FsEventResult {
    pub changes: Vec<FsChange>,
    pub new_event_id: u64,
}

/// Query macOS FSEvents for all file changes since the given event ID.
/// Returns None if event ID is 0 (first run) or stream creation fails.
pub fn get_changes_since(
    since_event_id: u64,
    watch_paths: &[String],
) -> Option<FsEventResult> {
    if since_event_id == 0 || watch_paths.is_empty() {
        return None;
    }

    let paths_owned: Vec<String> = watch_paths.to_vec();
    let (tx, rx) = mpsc::channel::<(String, u32, u64)>();

    // Run everything on a dedicated thread that owns the CFRunLoop
    std::thread::spawn(move || {
        let cf_strings: Vec<CFString> = paths_owned.iter().map(|p| CFString::new(p)).collect();
        let cf_array = CFArray::from_CFTypes(&cf_strings);

        let tx_box = Box::new(tx);
        let tx_ptr = Box::into_raw(tx_box);

        let context = FSEventStreamContext {
            version: 0,
            info: tx_ptr as *mut c_void,
            retain: None,
            release: None,
            copy_description: None,
        };

        let flags = kFSEventStreamCreateFlagFileEvents | kFSEventStreamCreateFlagNoDefer;

        unsafe {
            let stream = FSEventStreamCreate(
                std::ptr::null_mut(),
                callback,
                &context as *const FSEventStreamContext,
                cf_array.as_concrete_TypeRef(),
                since_event_id,
                0.0,
                flags,
            );

            if stream.is_null() {
                let _ = Box::from_raw(tx_ptr);
                return;
            }

            let run_loop = CFRunLoop::get_current();
            FSEventStreamScheduleWithRunLoop(
                stream,
                run_loop.as_concrete_TypeRef(),
                kCFRunLoopDefaultMode,
            );
            FSEventStreamStart(stream);

            // Run loop processes events and calls our callback.
            // Historical replay is near-instant; timeout is safety net.
            CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, Duration::from_secs(5), false);

            FSEventStreamStop(stream);
            FSEventStreamInvalidate(stream);
            FSEventStreamRelease(stream);

            let _ = Box::from_raw(tx_ptr);
        }
    });

    // Collect events from the callback
    let mut changes: Vec<FsChange> = Vec::new();
    let mut new_event_id = since_event_id;

    while let Ok((path, flags, event_id)) = rx.recv_timeout(Duration::from_secs(6)) {
        if event_id > new_event_id {
            new_event_id = event_id;
        }

        if flags & kFSEventStreamEventFlagHistoryDone != 0 {
            break;
        }

        let must_scan = flags & kFSEventStreamEventFlagMustScanSubDirs != 0;
        let is_file = flags & kFSEventStreamEventFlagItemIsFile != 0;
        let created = flags & kFSEventStreamEventFlagItemCreated != 0;
        let modified = flags & kFSEventStreamEventFlagItemModified != 0;
        let removed = flags & kFSEventStreamEventFlagItemRemoved != 0;
        let renamed = flags & kFSEventStreamEventFlagItemRenamed != 0;

        if must_scan {
            changes.push(FsChange {
                path, created: false, modified: false,
                removed: false, renamed: false, must_scan_dir: true,
            });
        } else if is_file && (created || modified || removed || renamed) {
            changes.push(FsChange {
                path, created, modified, removed, renamed,
                must_scan_dir: false,
            });
        }
    }

    let current = current_event_id();
    if current > new_event_id {
        new_event_id = current;
    }

    Some(FsEventResult { changes, new_event_id })
}

/// Get the current system FSEvents event ID.
pub fn current_event_id() -> u64 {
    unsafe { FSEventsGetCurrentEventId() }
}

/// Check if file exists (for resolving renames).
pub fn resolve_rename(path: &str) -> bool {
    Path::new(path).exists()
}

extern "C" fn callback(
    _stream_ref: FSEventStreamRef,
    info: *mut c_void,
    num_events: usize,
    event_paths: *mut c_void,
    event_flags: *const FSEventStreamEventFlags,
    event_ids: *const FSEventStreamEventId,
) {
    let tx = unsafe { &*(info as *const mpsc::Sender<(String, u32, u64)>) };
    let paths = unsafe { std::slice::from_raw_parts(event_paths as *const *const i8, num_events) };
    let flags = unsafe { std::slice::from_raw_parts(event_flags, num_events) };
    let ids = unsafe { std::slice::from_raw_parts(event_ids, num_events) };

    for i in 0..num_events {
        let path = unsafe { CStr::from_ptr(paths[i]).to_string_lossy().to_string() };
        let _ = tx.send((path, flags[i], ids[i]));
    }
}
