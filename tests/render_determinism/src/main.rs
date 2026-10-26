//! Prints the native hashes of the fixed graph and of the parity project's
//! frames, then the project's frame times in ticks, comma-separated.

fn main() {
    println!("{:016x}", alpha2_render_determinism::graph_hash());
    println!("{:016x}", alpha2_render_determinism::project_hash());
    let times: Vec<String> = alpha2_render_determinism::project_frame_times()
        .iter()
        .map(ToString::to_string)
        .collect();
    println!("{}", times.join(","));
}
