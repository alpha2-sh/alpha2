//! Thread contracts the native hosts rely on, checked at compile time.
//!
//! Exports, stills and preview run on host-owned threads and move these values between them.

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
