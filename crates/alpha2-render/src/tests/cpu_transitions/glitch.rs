//! The "Glitch" group's transitions on the CPU against the GPU.

use super::{Tolerance, check};
use crate::ParamValue;

#[test]
fn strip_datamosh_glitch_matches() {
    check(
        "strip-datamosh-glitch",
        Tolerance::Edges,
        &[
            &[
                ("strength", ParamValue::Float(1.8)),
                ("horizontal_bars", ParamValue::Float(13.0)),
                ("vertical_slits", ParamValue::Float(7.0)),
                ("tear", ParamValue::Float(0.6)),
                ("chroma", ParamValue::Float(0.1)),
            ],
            &[
                ("strength", ParamValue::Float(0.5)),
                ("residue", ParamValue::Float(1.0)),
                ("noise_amount", ParamValue::Float(0.6)),
                ("scan_amount", ParamValue::Float(0.5)),
                ("flash_amount", ParamValue::Float(0.9)),
            ],
        ],
    );
}

#[test]
fn drop_zone_flicker_matches() {
    check(
        "drop-zone-flicker",
        Tolerance::Edges,
        &[
            &[
                ("frame_rate", ParamValue::Float(9.0)),
                ("rgb_offset", ParamValue::Float(0.06)),
                ("block_amount", ParamValue::Float(1.0)),
            ],
            &[
                ("frame_rate", ParamValue::Float(41.0)),
                ("ghost_amount", ParamValue::Float(1.0)),
                ("red_cyan", ParamValue::Float(1.0)),
                ("scanline", ParamValue::Float(0.3)),
            ],
        ],
    );
}

#[test]
fn glitch_displace_matches() {
    check("glitch-displace", Tolerance::Edges, &[]);
}

#[test]
fn old_tv_lost_signal_matches() {
    check("old-tv-lost-signal", Tolerance::Edges, &[]);
}

#[test]
fn static_fade_matches() {
    check(
        "static-fade",
        Tolerance::Edges,
        &[
            &[
                ("n_noise_pixels", ParamValue::Float(37.0)),
                ("static_luminosity", ParamValue::Float(0.4)),
            ],
            &[("n_noise_pixels", ParamValue::Float(1.0))],
        ],
    );
}

#[test]
fn tv_static_matches() {
    check(
        "tv-static",
        Tolerance::Close,
        &[
            &[("offset", ParamValue::Float(0.3))],
            &[("offset", ParamValue::Float(0.0))],
        ],
    );
}

#[test]
fn parametric_glitch_matches() {
    check(
        "parametric-glitch",
        Tolerance::Unstable,
        &[&[
            ("amp_x", ParamValue::Float(4.0)),
            ("amp_y", ParamValue::Float(0.5)),
        ]],
    );
}

#[test]
fn glitch_matches() {
    check(
        "glitch",
        Tolerance::Edges,
        &[
            &[
                ("amount", ParamValue::Float(0.5)),
                ("block_size", ParamValue::Float(0.13)),
                ("seed", ParamValue::Float(17.0)),
            ],
            &[
                ("block_size", ParamValue::Float(0.005)),
                ("seed", ParamValue::Float(1000.0)),
            ],
        ],
    );
}
