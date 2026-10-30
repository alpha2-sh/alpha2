//! Prints the native hashes of the fixed mix, its waveform summary, its
//! loudness report, the time-mapped mix, the resampled source, the mono mix,
//! the 44.1 kHz mix in uneven blocks, and the stretched mix; with `fixture`, the browser fixture
//! (`tests/golden/audio_mix.json`).

fn main() {
    if std::env::args().nth(1).as_deref() == Some("fixture") {
        let fixture = alpha2_audio_determinism::browser_fixture();
        println!("{}", serde_json::to_string_pretty(&fixture).expect("JSON"));
        return;
    }
    println!("{:016x}", alpha2_audio_determinism::render_hash());
    println!("{:016x}", alpha2_audio_determinism::waveform_hash());
    println!("{:016x}", alpha2_audio_determinism::loudness_hash());
    println!("{:016x}", alpha2_audio_determinism::time_map_hash());
    println!("{:016x}", alpha2_audio_determinism::resample_hash());
    println!("{:016x}", alpha2_audio_determinism::mono_hash());
    println!("{:016x}", alpha2_audio_determinism::rate_and_blocks_hash());
    println!("{:016x}", alpha2_audio_determinism::stretch_hash());
}
