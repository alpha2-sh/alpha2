//! Frame access write-back and unlocking on a real `FFmpeg` Vulkan frame.
//!
//! Each test runs in a child process so the driver loads outside memcheck (it trips valgrind's
//! loader) and a leaked lock hangs only the child. Skips without a Vulkan device.

use std::{
    io::Read,
    process::{Command, Stdio},
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

use alpha2_test_support::{Capability, skip};

use super::*;

/// Runs the ignored `*_child` test `test`; fails if it fails, did not run, or takes over a minute.
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

/// One NV12 frame, or `None` without a Vulkan device.
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

/// Whether the frame can be locked again within a few seconds.
fn locks_again(lease: &Arc<VulkanFrameLease>) -> bool {
    let (sender, receiver) = mpsc::channel();
    let lease = Arc::clone(lease);
    std::thread::spawn(move || {
        let locked = lease.lock_access().is_ok();
        // Drop before answering so the caller can unwrap its lease.
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
        // SAFETY: the committed record names no image of this frame, so nothing is written back.
        unsafe { *guard.access_mut() = VulkanFrameAccess::default() };
        guard.commit();
    }
    assert!(
        locks_again(&lease),
        "the access released the locks its record named, not the ones it took"
    );
    // Same through an encoder-pool frame's access, which borrows the frame.
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
    // A queue transfer leaves a nonzero timeline value.
    lease.download().expect("download");
    let before = lease.info().expect("info");
    let planes = usize::try_from(before.plane_count).expect("planes");
    // With VK_EXT_host_image_copy (lavapipe) FFmpeg copies through the host and signals nothing.
    // alpha2's own devices never enable it.
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
    lease
        .download()
        .expect("download after the refused write-back");
}
