//! macOS sleep/wake detection using IOKit power management.
//!
//! This module registers for system sleep notifications and triggers
//! cleanup of auto-cleanup-enabled instances when the system sleeps.
//!
//! Uses IOPMAssertion to delay sleep while cleanup is in progress.

use anyhow::{Context, Result};
use chrono::Local;
use std::ffi::c_void;
use std::ptr;

/// Log a message with timestamp.
fn log(msg: &str) {
    let ts = Local::now().format("%Y-%m-%d %H:%M:%S");
    eprintln!("[{}] [spotty watch] {}", ts, msg);
}

// IOKit and CoreFoundation FFI declarations
#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IORegisterForSystemPower(
        refcon: *mut c_void,
        notify_port_ref: *mut *mut c_void,
        callback: extern "C" fn(*mut c_void, u32, u32, *mut c_void),
        notifier: *mut u32,
    ) -> u32;

    fn IODeregisterForSystemPower(notifier: *mut u32) -> u32;
    fn IOAllowPowerChange(root_port: u32, notification_id: isize) -> u32;
    fn IONotificationPortGetRunLoopSource(notify_port: *mut c_void) -> *mut c_void;

    // Power assertion functions
    fn IOPMAssertionCreateWithName(
        assertion_type: *const c_void,  // CFStringRef
        level: u32,
        name: *const c_void,  // CFStringRef
        assertion_id: *mut u32,
    ) -> i32;
    fn IOPMAssertionRelease(assertion_id: u32) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRunLoopGetCurrent() -> *mut c_void;
    fn CFRunLoopAddSource(rl: *mut c_void, source: *mut c_void, mode: *const c_void);
    fn CFRunLoopRun();

    static kCFRunLoopDefaultMode: *const c_void;

    // CFString functions
    fn CFStringCreateWithCString(
        alloc: *const c_void,
        c_str: *const i8,
        encoding: u32,
    ) -> *const c_void;
    fn CFRelease(cf: *const c_void);
}

// IOKit message types
const K_IO_MESSAGE_SYSTEM_WILL_SLEEP: u32 = 0xe0000280;
const K_IO_MESSAGE_CAN_SYSTEM_SLEEP: u32 = 0xe0000270;
const K_IO_MESSAGE_SYSTEM_WILL_POWER_ON: u32 = 0xe0000320;
const K_IO_MESSAGE_SYSTEM_HAS_POWERED_ON: u32 = 0xe0000300;

// Power assertion constants
const K_IOPM_ASSERTION_LEVEL_ON: u32 = 255;
const K_CF_STRING_ENCODING_UTF8: u32 = 0x08000100;

struct PowerCallbackContext {
    root_port: u32,
}

/// Create a CFString from a Rust string.
fn cfstring(s: &str) -> *const c_void {
    let c_str = std::ffi::CString::new(s).unwrap();
    // SAFETY: CFStringCreateWithCString is a stable macOS API that accepts
    // a null allocator (uses default) and a valid C string pointer.
    // c_str is valid for the duration of the call.
    unsafe { CFStringCreateWithCString(ptr::null(), c_str.as_ptr(), K_CF_STRING_ENCODING_UTF8) }
}

/// Perform cleanup synchronously (blocks until done).
fn do_cleanup_sync() {
    log("Starting cleanup...");

    // Create a new tokio runtime for this sync context
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            log(&format!("Failed to create runtime: {}", e));
            return;
        }
    };

    let result = rt.block_on(async {
        crate::aws::instances::stop_auto_cleanup_instances(None).await
    });

    match result {
        Ok(ids) => {
            if ids.is_empty() {
                log("No instances to clean up");
            } else {
                log(&format!("Terminated {} instance(s):", ids.len()));
                for id in &ids {
                    log(&format!("  - {}", id));
                }
            }
        }
        Err(e) => {
            log(&format!("Failed to clean up: {:?}", e));
        }
    }

    log("Cleanup complete");
}

/// Callback invoked by IOKit on power events.
extern "C" fn power_callback(
    refcon: *mut c_void,
    _service: u32,
    message_type: u32,
    message_argument: *mut c_void,
) {
    // SAFETY: refcon was created as a Box::leak'd PowerCallbackContext pointer
    // in run_watch() and passed to IORegisterForSystemPower. IOKit guarantees
    // it passes the same pointer back in callbacks.
    let ctx = unsafe { &*(refcon as *const PowerCallbackContext) };

    match message_type {
        K_IO_MESSAGE_CAN_SYSTEM_SLEEP => {
            // System is asking if we can sleep - allow it immediately
            // (we'll do cleanup in WILL_SLEEP which gives us more time)
            log("Received CAN_SYSTEM_SLEEP - allowing sleep");
            // SAFETY: IOAllowPowerChange is called with the root_port from
            // IORegisterForSystemPower and the notification ID from this callback.
            // This is the documented usage pattern from Apple's IOKit documentation.
            unsafe {
                IOAllowPowerChange(ctx.root_port, message_argument as isize);
            }
        }
        K_IO_MESSAGE_SYSTEM_WILL_SLEEP => {
            // System is going to sleep - create assertion to delay it
            log("Received WILL_SLEEP - delaying sleep for cleanup...");

            // Create power assertion to prevent sleep while we clean up
            let assertion_type = cfstring("PreventUserIdleSystemSleep");
            let assertion_name = cfstring("spotty: terminating AWS instances");
            let mut assertion_id: u32 = 0;

            // SAFETY: IOPMAssertionCreateWithName takes valid CFString pointers
            // (created above via cfstring()) and writes to assertion_id which
            // is a valid mutable reference. The CFStrings remain valid until
            // CFRelease is called below.
            let result = unsafe {
                IOPMAssertionCreateWithName(
                    assertion_type,
                    K_IOPM_ASSERTION_LEVEL_ON,
                    assertion_name,
                    &mut assertion_id,
                )
            };

            if result == 0 {
                log("Power assertion created, starting cleanup...");
            } else {
                log(&format!("Warning: Failed to create power assertion (error {}), cleanup may be interrupted", result));
            }

            // Do cleanup synchronously
            do_cleanup_sync();

            // Release assertion
            if result == 0 {
                // SAFETY: assertion_id was successfully created above (result == 0),
                // and we are releasing it exactly once.
                unsafe {
                    IOPMAssertionRelease(assertion_id);
                }
                log("Power assertion released");
            }

            // Release CFStrings
            // SAFETY: assertion_type and assertion_name were created via cfstring()
            // and have not been released. Each CFString is released exactly once.
            unsafe {
                CFRelease(assertion_type);
                CFRelease(assertion_name);
            }

            // Now allow sleep to proceed
            log("Allowing sleep to proceed");
            // SAFETY: Same as K_IO_MESSAGE_CAN_SYSTEM_SLEEP case above.
            unsafe {
                IOAllowPowerChange(ctx.root_port, message_argument as isize);
            }
        }
        K_IO_MESSAGE_SYSTEM_WILL_POWER_ON => {
            log("Received WILL_POWER_ON - system waking up");
        }
        K_IO_MESSAGE_SYSTEM_HAS_POWERED_ON => {
            log("Received POWERED_ON - system awake");
        }
        _ => {
            log(&format!("Received power event: 0x{:08x}", message_type));
        }
    }
}

/// Run the watch daemon, listening for sleep events.
/// This function blocks indefinitely.
pub async fn run_watch() -> Result<()> {
    log("Starting sleep watcher daemon...");
    log("Will terminate instances with AutoCleanup=true on sleep");

    // Run IOKit registration in a blocking task (this blocks forever)
    let iokit_result = tokio::task::spawn_blocking(move || {
        let mut notify_port_ref: *mut c_void = ptr::null_mut();
        let mut notifier: u32 = 0;

        // Create context that will be passed to callback
        // Use Box::leak to ensure it lives for the lifetime of the program
        let ctx = Box::leak(Box::new(PowerCallbackContext { root_port: 0 }));

        // Register for power notifications
        // SAFETY: We pass a valid pointer to our context (Box::leak ensures it
        // lives for the program duration), valid mutable pointers for output
        // parameters, and a valid extern "C" callback function.
        let root_port = unsafe {
            IORegisterForSystemPower(
                ctx as *mut PowerCallbackContext as *mut c_void,
                &mut notify_port_ref,
                power_callback,
                &mut notifier,
            )
        };

        if root_port == 0 {
            return Err(anyhow::anyhow!("Failed to register for system power notifications"));
        }

        ctx.root_port = root_port;

        // Get the run loop source and add it to the current run loop
        // SAFETY: notify_port_ref was successfully initialized by
        // IORegisterForSystemPower (we checked root_port != 0).
        let run_loop_source = unsafe { IONotificationPortGetRunLoopSource(notify_port_ref) };
        if run_loop_source.is_null() {
            return Err(anyhow::anyhow!("Failed to get run loop source"));
        }

        // SAFETY: CFRunLoopGetCurrent() returns the current thread's run loop,
        // CFRunLoopAddSource adds the source we just got from IOKit.
        // kCFRunLoopDefaultMode is a valid static CFString from CoreFoundation.
        unsafe {
            let run_loop = CFRunLoopGetCurrent();
            CFRunLoopAddSource(run_loop, run_loop_source, kCFRunLoopDefaultMode);
        }

        log("Listening for sleep events... (press Ctrl+C to stop)");

        // Run the event loop (this blocks forever)
        // SAFETY: CFRunLoopRun is safe to call after setting up the run loop
        // with a source. It blocks until CFRunLoopStop is called or the source
        // is removed.
        unsafe {
            CFRunLoopRun();
        }

        // Cleanup (not reached unless run loop stops)
        // SAFETY: notifier was initialized by IORegisterForSystemPower and
        // we are deregistering the same notification.
        unsafe {
            IODeregisterForSystemPower(&mut notifier);
        }

        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("IOKit task panicked")?;

    iokit_result
}
