//! What runs paint besides their glyphs: background boxes and decoration lines.

use super::{Faces, Layout, runs::Shaping};

#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct RunBox {
    pub(crate) left: f32,
    pub(crate) top: f32,
    pub(crate) right: f32,
    pub(crate) bottom: f32,
    /// Straight sRGB RGBA.
    pub(crate) color: [u8; 4],
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct DecorationBox {
    pub(crate) owner: usize,
    pub(crate) rect: [f32; 4],
    pub(crate) affine: [f32; 6],
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct LineMetric {
    pub(crate) top: f32,
    pub(crate) thickness: f32,
}

/// OpenType `post` underline and `OS/2` strikeout metrics, variations applied.
pub(crate) fn decoration_metrics(face: &rustybuzz::ttf_parser::Face<'_>) -> [LineMetric; 2] {
    let em = f32::from(face.units_per_em());
    let checked = |metrics: Option<rustybuzz::ttf_parser::LineMetrics>, fallback_top: f32| {
        metrics
            .map(|metrics| (f32::from(metrics.position), f32::from(metrics.thickness)))
            .filter(|&(position, thickness)| {
                thickness > 0.0 && thickness <= 0.25 * em && position.abs() <= em
            })
            .map_or(
                LineMetric {
                    top: fallback_top * em,
                    thickness: 0.05 * em,
                },
                |(position, thickness)| LineMetric {
                    top: -position,
                    thickness,
                },
            )
    };
    [
        checked(face.underline_metrics(), 0.1),
        checked(face.strikeout_metrics(), -0.325),
    ]
}

#[derive(Debug, Default)]
pub(crate) struct LinePaint {
    pub(crate) boxes: Vec<RunBox>,
    pub(crate) decorations: Vec<DecorationBox>,
}

impl LinePaint {
    /// Adds line `index`'s run backgrounds and decorations: `extents` gives each cluster's
    /// advance and the part of it a background is drawn behind, `None` for none.
    pub(crate) fn add_line(
        &mut self,
        faces: &Faces<'_>,
        shaping: &Shaping,
        layout: &Layout,
        index: usize,
        extents: (
            impl Fn(usize) -> (f32, f32),
            impl Fn(usize) -> Option<(f32, f32)>,
        ),
        affine: impl Fn(usize) -> [f32; 6],
    ) {
        let (extent, shown) = extents;
        let baseline = layout.baseline(index);
        let look_of = |cluster: usize| layout.clusters[cluster].look;
        let mut order = Vec::<usize>::new();
        for glyph in &layout.lines[index].glyphs {
            if order.last() != Some(&glyph.cluster) {
                order.push(glyph.cluster);
            }
        }
        let mut start = 0;
        while start < order.len() {
            let look = look_of(order[start]);
            let end = (start..order.len())
                .find(|&at| look_of(order[at]) != look)
                .unwrap_or(order.len());
            let fragment = &order[start..end];
            start = end;
            let paint = &shaping.looks[usize::from(look)];
            let size = paint.size;
            let behind = fragment
                .iter()
                .filter_map(|&cluster| shown(cluster))
                .reduce(|held, (start, end)| (held.0.min(start), held.1.max(end)));
            if let (Some(color), Some((left, right))) = (paint.background, behind) {
                self.boxes.push(RunBox {
                    left,
                    top: baseline - 0.8 * size,
                    right,
                    bottom: baseline + 0.2 * size,
                    color,
                });
            }
            if !paint.underline && !paint.line_through {
                continue;
            }
            let primary = shaping.orders[usize::from(look)][0];
            let scale = size / f32::from(faces.parsed[primary].units_per_em());
            let [under, through] = decoration_metrics(&faces.parsed[primary]);
            let lines = [(paint.underline, under), (paint.line_through, through)];
            let mut first = 0;
            while first < fragment.len() {
                let map = affine(fragment[first]);
                let last = (first..fragment.len())
                    .find(|&at| affine(fragment[at]).map(f32::to_bits) != map.map(f32::to_bits))
                    .unwrap_or(fragment.len());
                let (left, right) = span_of(&fragment[first..last], &extent);
                for (drawn, metric) in lines {
                    if drawn {
                        let top = baseline + metric.top * scale;
                        self.decorations.push(DecorationBox {
                            owner: fragment[first],
                            rect: [left, top, right, top + metric.thickness * scale],
                            affine: map,
                        });
                    }
                }
                first = last;
            }
        }
    }
}

fn span_of(clusters: &[usize], extent: &impl Fn(usize) -> (f32, f32)) -> (f32, f32) {
    clusters
        .iter()
        .fold((f32::MAX, f32::MIN), |(left, right), &cluster| {
            let (start, end) = extent(cluster);
            (left.min(start), right.max(end))
        })
}
