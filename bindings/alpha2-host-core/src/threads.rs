//! Thread contracts the native hosts rely on, checked at compile time, and what a thread's panic
//! said.
//!
//! Exports, stills and preview run on host-owned threads and move these values between them.

/// The message of a caught panic's `payload`.
#[must_use]
pub fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic without a message".into())
}

const fn send_sync<T: Send + Sync>() {}
const fn send<T: Send>() {}

#[cfg(feature = "engine")]
const _: () = {
    send_sync::<alpha2_engine::EngineProject>();
    send_sync::<alpha2_engine::Exporter>();
    send_sync::<alpha2_render::Renderer>();
    send_sync::<alpha2_render::CpuRenderer>();
    send::<alpha2_engine::Player>();
};

const _: () = {
    send_sync::<crate::DecodedAudioSources>();
    send_sync::<crate::recovery::Recovery>();
    send::<crate::partial::PartialOutput>();
    send::<crate::audio_frames::EncoderAudioInput>();
    send::<crate::audio_timeline::SourceTimeline>();
};

#[cfg(test)]
mod tests {
    #[test]
    fn panic_text_reads_both_payload_types() {
        let text = |body: fn()| super::panic_text(&*std::panic::catch_unwind(body).unwrap_err());
        assert_eq!(text(|| panic!("plain")), "plain");
        assert_eq!(text(|| panic!("formatted {}", 1)), "formatted 1");
        assert_eq!(
            text(|| std::panic::panic_any(7_u8)),
            "a panic without a message"
        );
    }
}
