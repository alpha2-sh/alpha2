//! Spans resolved to looks: the face, size, spacing and paint each byte of a request draws with.

use super::{FontStyle, MAX_FONT_SIZE, MAX_TEXT_SPANS, TextFill, TextRasterRequest, TextStroke};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Look {
    pub(crate) family: Option<String>,
    pub(crate) weight: u16,
    pub(crate) style: FontStyle,
    pub(crate) size: f32,
    pub(crate) spacing: f32,
    pub(crate) fill: TextFill,
    pub(crate) strokes: Vec<TextStroke>,
    pub(crate) background: Option<[u8; 4]>,
    pub(crate) underline: bool,
    pub(crate) line_through: bool,
}

impl Look {
    pub(crate) fn of_request(request: &TextRasterRequest) -> Self {
        Self {
            family: request.font_family.clone(),
            weight: request.font_weight,
            style: FontStyle::Normal,
            size: request.font_size,
            spacing: request.letter_spacing,
            fill: request.fill.clone(),
            strokes: request.strokes.clone(),
            background: None,
            underline: false,
            line_through: false,
        }
    }

    pub(crate) fn shapes_like(&self, other: &Self) -> bool {
        self.family == other.family
            && self.weight == other.weight
            && self.style == other.style
            && self.size.to_bits() == other.size.to_bits()
            && self.spacing.to_bits() == other.spacing.to_bits()
    }
}

pub(crate) fn paints_per_cluster(request: &TextRasterRequest) -> bool {
    request.spans.iter().any(|span| {
        span.fill != request.fill
            || span.strokes != request.strokes
            || span.background_color.is_some()
            || span.underline
            || span.line_through
    })
}

pub(crate) fn valid_spans(request: &TextRasterRequest) -> bool {
    let text = &request.text;
    request.spans.len() <= MAX_TEXT_SPANS
        && request
            .spans
            .windows(2)
            .all(|pair| pair[0].bytes.end <= pair[1].bytes.start)
        && request.spans.iter().all(|span| {
            span.bytes.start < span.bytes.end
                && span.bytes.end <= text.len()
                && text.is_char_boundary(span.bytes.start)
                && text.is_char_boundary(span.bytes.end)
                && span.font_size.is_finite()
                && span.font_size > 0.0
                && span.font_size <= MAX_FONT_SIZE
                && span.letter_spacing.is_finite()
                && span.letter_spacing >= super::LETTER_SPACING_RANGE.start() * span.font_size
                && span.letter_spacing <= super::LETTER_SPACING_RANGE.end() * span.font_size
                && (1..=1000).contains(&span.font_weight)
                && span.fill.is_valid()
                && super::valid_strokes(&span.strokes)
        })
}

#[derive(Debug)]
pub(crate) struct Shaping {
    pub(crate) looks: Vec<Look>,
    pub(crate) orders: Vec<Vec<usize>>,
    at: Vec<u16>,
}

impl Shaping {
    pub(crate) fn looks(request: &TextRasterRequest, map: &[usize]) -> (Vec<Look>, Vec<u16>) {
        let base = Look::of_request(request);
        let mut looks = vec![base.clone()];
        let mut ranges = Vec::new();
        for span in &request.spans {
            let look = Look {
                family: span.font_family.clone(),
                weight: span.font_weight,
                style: span.font_style,
                size: span.font_size,
                spacing: span.letter_spacing,
                fill: span.fill.clone(),
                strokes: span.strokes.clone(),
                background: span.background_color,
                underline: span.underline,
                line_through: span.line_through,
            };
            if look == base {
                continue;
            }
            let index = looks
                .iter()
                .position(|held| *held == look)
                .unwrap_or_else(|| {
                    looks.push(look);
                    looks.len() - 1
                });
            ranges.push((span.bytes.clone(), u16::try_from(index).unwrap_or(0)));
        }
        if ranges.is_empty() {
            return (looks, Vec::new());
        }
        let at = map
            .iter()
            .take(map.len().saturating_sub(1))
            .map(|&source| {
                let found = ranges.partition_point(|(range, _)| range.end <= source);
                ranges
                    .get(found)
                    .filter(|(range, _)| range.contains(&source))
                    .map_or(0, |(_, look)| *look)
            })
            .collect();
        (looks, at)
    }

    pub(crate) const fn new(looks: Vec<Look>, orders: Vec<Vec<usize>>, at: Vec<u16>) -> Self {
        Self { looks, orders, at }
    }

    pub(crate) fn at(&self, byte: usize) -> u16 {
        self.at.get(byte).copied().unwrap_or(0)
    }

    pub(crate) fn base(&self) -> &Look {
        &self.looks[0]
    }

    pub(crate) fn uniform(&self) -> bool {
        let base = self.base().size;
        self.looks.iter().all(|look| look.size <= base)
    }
}
