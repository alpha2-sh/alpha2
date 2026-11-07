//! Prints the native render hashes and the parity project's frame times in ticks.

fn main() {
    println!("{:016x}", alpha2_render_determinism::graph_hash());
    println!("{:016x}", alpha2_render_determinism::project_hash());
    println!("{:016x}", alpha2_render_determinism::blur_and_stroke_hash());
    println!("{:016x}", alpha2_render_determinism::text_fit_hash());
    let times: Vec<String> = alpha2_render_determinism::project_frame_times()
        .iter()
        .map(ToString::to_string)
        .collect();
    println!("{}", times.join(","));
}
