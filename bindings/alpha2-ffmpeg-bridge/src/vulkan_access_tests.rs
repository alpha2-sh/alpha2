//! What a frame access writes back to a real `FFmpeg` Vulkan frame, and what
//! it unlocks, whatever its record was changed to.
//!
//! The frames come from a device `FFmpeg` creates itself on the default
//! Vulkan device; without one the tests skip, recorded as the `vulkan`
//! capability. Each test runs in a child process of its own: the device's
//! driver is then loaded outside any memcheck run of this binary (whose
//! dynamic loader it trips on reading an ICD's search path), and a lock an
//! access leaked hangs only the child. The children are ignored tests the
//! parents run by name.

use std::{
    io::Read,
    process::{Command, Stdio},
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

use alpha2_test_support::{Capability, skip};

use super::*;

/// Runs the ignored `*_child` test `test` in a child process, which fails the
/// test when it fails, did not run, or is still running after a minute.
fn run_child(test: &str) {
    let mut child = Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            &format!("vulkan_access_tests::{test}"),
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .expect("child");
    let mut stdout = child.stdout.take().expect("child stdout");
    let reader = std::thread::spawn(move || {
        let mut printed = String::new();
        let _ = stdout.read_to_string(&mut printed);
        printed
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().expect("child status") {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{test} hung");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let printed = reader.join().expect("child stdout reader");
    assert!(
        status.success() && printed.contains("1 passed"),
        "{test} failed or did not run: {status}\n{printed}"
    );
}

/// One NV12 frame of a pool on a device `FFmpeg` made, or `None` (and a
/// note) without a Vulkan device.
fn vulkan_frame() -> Option<VulkanFrameLease> {
    let device = match AVHWDeviceContext::create(ffi::AV_HWDEVICE_TYPE_VULKAN, None, None, 0) {
        Ok(device) => device,
        Err(error) => {
            skip!(Capability::Vulkan, "FFmpeg made no Vulkan device ({error})");
            return None;
        }
    };
    let mut frames = device.hwframe_ctx_alloc();
    {
        let data = frames.data();
        data.format = ffi::AV_PIX_FMT_VULKAN;
        data.sw_format = ffi::AV_PIX_FMT_NV12;
        data.width = 64;
        data.height = 64;
    }
    frames.init().expect("frames context");
    let mut frame = AVFrame::new();
    frames.get_buffer(&mut frame).expect("frame");
    Some(VulkanFrameLease::new(frame))
}

/// Whether the frame can be locked again within a few seconds: it cannot
/// while an earlier access left its frame or queue lock held.
fn locks_again(lease: &Arc<VulkanFrameLease>) -> bool {
    let (sender, receiver) = mpsc::channel();
    let lease = Arc::clone(lease);
    // Left blocked, the thread is abandoned with the lease it holds.
    std::thread::spawn(move || {
        let locked = lease.lock_access().is_ok();
        // Before the answer: a caller that then unwraps its lease finds no other holder.
        drop(lease);
        let _ = sender.send(locked);
    });
    receiver.recv_timeout(Duration::from_secs(5)) == Ok(true)
}

#[test]
fn a_frame_access_unlocks_what_it_locked_whatever_its_record_holds() {
    run_child("replaced_record_child");
}

#[test]
#[ignore = "run by a_frame_access_unlocks_what_it_locked_whatever_its_record_holds"]
fn replaced_record_child() {
    let Some(lease) = vulkan_frame() else {
        return;
    };
    let lease = Arc::new(lease);
    {
        let mut guard = lease.lock_access().expect("access");
        // SAFETY: the committed record names no image of this frame, so nothing is written back:
        // the frame keeps its own state.
        unsafe { *guard.access_mut() = VulkanFrameAccess::default() };
        guard.commit();
    }
    assert!(
        locks_again(&lease),
        "the access released the locks its record named, not the ones it took"
    );
    // The same through an encoder-pool frame's access, which borrows the frame instead.
    let Ok(lease) = Arc::try_unwrap(lease) else {
        panic!("the lease has another holder");
    };
    let mut frame = lease.frame.into_inner().expect("frame");
    {
        let mut access = VulkanEncodeFrameAccess::begin(&mut frame).expect("access");
        // SAFETY: as above.
        unsafe { *access.access_mut() = VulkanFrameAccess::default() };
        access.commit();
    }
    let lease = Arc::new(VulkanFrameLease::new(frame));
    assert!(locks_again(&lease), "the encode access leaked its locks");
}

#[test]
fn a_committed_timeline_value_never_goes_below_the_frames() {
    run_child("lowered_timeline_child");
}

#[test]
#[ignore = "run by a_committed_timeline_value_never_goes_below_the_frames"]
fn lowered_timeline_child() {
    let Some(lease) = vulkan_frame() else {
        return;
    };
    // A queue transfer waits for the frame's value and signals the next, so the frame then records
    // a value above zero that its semaphore has reached.
    lease.download().expect("download");
    let before = lease.info().expect("info");
    let planes = usize::try_from(before.plane_count).expect("planes");
    // FFmpeg copies through the host instead when the device has VK_EXT_host_image_copy (lavapipe
    // since Mesa 25), and that signals nothing. A device alpha2 hands FFmpeg never enables it.
    if before.semaphore_values[..planes].contains(&0) {
        eprintln!("skipped: the transfer went through the host and left a timeline value at zero");
        return;
    }
    {
        let mut guard = lease.lock_access().expect("access");
        // SAFETY: zero is a value the semaphore has passed, and the layouts are the frame's own.
        let descriptor = unsafe { guard.access_mut() }.frame_mut();
        descriptor.semaphore_values.fill(0);
        guard.commit();
    }
    let after = lease.info().expect("info");
    assert_eq!(
        after.semaphore_values, before.semaphore_values,
        "a lowered timeline value was written back: the frame's next signal would not exceed it"
    );
    // The frame is still usable: its next transfer waits for the value it really recorded.
    lease
        .download()
        .expect("download after the refused write-back");
}
