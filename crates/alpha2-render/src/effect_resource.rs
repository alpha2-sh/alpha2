use std::collections::HashMap;
use std::fmt;

use half::f16;
use wgpu::util::DeviceExt;

use crate::{EffectResourceKind, RenderError, Renderer};

/// Canonical RGB samples for one cubic 3D lookup table.
#[derive(Clone, Debug, PartialEq)]
pub struct Lut3dData {
    /// Equal width, height, and depth of the cube.
    pub edge_length: u32,
    /// Per-channel input-domain lower bound declared by the file.
    pub domain_min: [f32; 3],
    /// Per-channel input-domain upper bound declared by the file.
    pub domain_max: [f32; 3],
    /// RGB samples in blue-major, green-middle, red-fastest order.
    pub values: Vec<[f32; 3]>,
}

impl Lut3dData {
    /// Parses the portable 3D subset of the Adobe/Resolve `.cube` text format.
    ///
    /// `TITLE`, comments, per-channel domains, and `LUT_3D_INPUT_RANGE` are
    /// accepted, as are CRLF line ends and a leading UTF-8 byte order mark,
    /// which a browser's text decoding removes and `std::fs::read_to_string`
    /// keeps: every host parses the same file alike. One-dimensional tables
    /// are rejected instead of being silently interpreted as a different
    /// color transform.
    ///
    /// # Errors
    /// Returns a line-oriented diagnostic for malformed or unsupported input.
    pub fn from_cube(text: &str) -> Result<Self, LutParseError> {
        let mut edge = None;
        let mut domain_min = [0.0; 3];
        let mut domain_max = [1.0; 3];
        let mut values = Vec::new();
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        for (index, raw) in text.lines().enumerate() {
            let line_number = index + 1;
            let line = raw.split('#').next().unwrap_or_default().trim();
            if line.is_empty() || line.starts_with("TITLE") {
                continue;
            }
            let fields = line.split_whitespace().collect::<Vec<_>>();
            match fields[0] {
                "LUT_3D_SIZE" if fields.len() == 2 => {
                    let parsed = fields[1]
                        .parse::<u32>()
                        .map_err(|_| LutParseError::new(line_number, "invalid LUT_3D_SIZE"))?;
                    if parsed < 2 {
                        return Err(LutParseError::new(
                            line_number,
                            "LUT_3D_SIZE must be at least two",
                        ));
                    }
                    edge = Some(parsed);
                }
                "LUT_1D_SIZE" => {
                    return Err(LutParseError::new(
                        line_number,
                        "one-dimensional .cube tables are not supported",
                    ));
                }
                "DOMAIN_MIN" if fields.len() == 4 => {
                    domain_min = parse_triplet(&fields[1..], line_number)?;
                }
                "DOMAIN_MAX" if fields.len() == 4 => {
                    domain_max = parse_triplet(&fields[1..], line_number)?;
                }
                "LUT_3D_INPUT_RANGE" if fields.len() == 3 => {
                    let min = parse_component(fields[1], line_number)?;
                    let max = parse_component(fields[2], line_number)?;
                    domain_min = [min; 3];
                    domain_max = [max; 3];
                }
                directive if directive.chars().next().is_some_and(char::is_alphabetic) => {
                    return Err(LutParseError::new(
                        line_number,
                        format!("unsupported .cube directive {directive}"),
                    ));
                }
                _ if fields.len() == 3 => values.push(parse_triplet(&fields, line_number)?),
                _ => {
                    return Err(LutParseError::new(
                        line_number,
                        "expected an RGB sample or supported .cube directive",
                    ));
                }
            }
        }
        let edge = edge.ok_or_else(|| LutParseError::new(0, "missing LUT_3D_SIZE"))?;
        let expected = usize::try_from(edge)
            .ok()
            .and_then(|value| value.checked_pow(3))
            .ok_or_else(|| LutParseError::new(0, "LUT size is too large"))?;
        if values.len() != expected {
            return Err(LutParseError::new(
                0,
                format!("expected {expected} RGB samples, found {}", values.len()),
            ));
        }
        if domain_min
            .iter()
            .zip(domain_max)
            .any(|(min, max)| *min >= max)
        {
            return Err(LutParseError::new(
                0,
                ".cube input domain minimum must be below its maximum",
            ));
        }
        Ok(Self {
            edge_length: edge,
            domain_min,
            domain_max,
            values,
        })
    }

    fn is_valid(&self, maximum: u32) -> bool {
        self.edge_length >= 2
            && self.edge_length <= maximum
            && usize::try_from(self.edge_length)
                .ok()
                .and_then(|value| value.checked_pow(3))
                .is_some_and(|expected| expected == self.values.len())
            && self.values.iter().flatten().all(|value| value.is_finite())
            && self
                .domain_min
                .iter()
                .zip(self.domain_max)
                .all(|(min, max)| min.is_finite() && max.is_finite() && *min < max)
    }
}

fn parse_triplet(fields: &[&str], line: usize) -> Result<[f32; 3], LutParseError> {
    let mut output = [0.0; 3];
    for (target, field) in output.iter_mut().zip(fields) {
        *target = parse_component(field, line)?;
    }
    Ok(output)
}

fn parse_component(field: &str, line: usize) -> Result<f32, LutParseError> {
    field
        .parse::<f32>()
        .ok()
        .filter(|value| value.is_finite())
        .ok_or_else(|| LutParseError::new(line, "invalid finite RGB component"))
}

/// Diagnostic returned while parsing `.cube` text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LutParseError {
    line: usize,
    message: String,
}

impl LutParseError {
    fn new(line: usize, message: impl Into<String>) -> Self {
        Self {
            line,
            message: message.into(),
        }
    }
}

impl fmt::Display for LutParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            formatter.write_str(&self.message)
        } else {
            write!(formatter, ".cube line {}: {}", self.line, self.message)
        }
    }
}

impl std::error::Error for LutParseError {}

#[derive(Debug)]
pub(crate) struct RegisteredEffectResource {
    pub(crate) kind: EffectResourceKind,
    pub(crate) _texture: wgpu::Texture,
    pub(crate) view: wgpu::TextureView,
    pub(crate) metadata: wgpu::Buffer,
}

pub(crate) type EffectResourceRegistry = HashMap<String, RegisteredEffectResource>;

impl Renderer {
    /// Uploads or replaces one filterable 3D lookup-table resource.
    ///
    /// # Errors
    /// Returns [`RenderError::InvalidEffectResource`] for an invalid identity,
    /// cube shape, sample, or device-incompatible edge length, and
    /// [`RenderError::Gpu`] when the device cannot make the texture (natively;
    /// a browser reports that as an uncaptured error). A resource that fails
    /// leaves the registry as it was.
    pub fn register_lut3d(
        &mut self,
        id: impl Into<String>,
        data: &Lut3dData,
    ) -> Result<(), RenderError> {
        let id = id.into();
        if id.is_empty() || !data.is_valid(self.limits.max_texture_dimension_3d) {
            return Err(RenderError::InvalidEffectResource);
        }
        let mut bytes = Vec::with_capacity(data.values.len() * 8);
        for rgb in &data.values {
            for value in [rgb[0], rgb[1], rgb[2], 1.0] {
                bytes.extend_from_slice(&f16::from_f32(value).to_bits().to_le_bytes());
            }
        }
        let (texture, error) = crate::gpu_errors::capture_creation(self.gpu.device(), || {
            self.gpu.device().create_texture_with_data(
                self.gpu.queue(),
                &wgpu::TextureDescriptor {
                    label: Some(&format!("alpha2 LUT resource {id}")),
                    size: wgpu::Extent3d {
                        width: data.edge_length,
                        height: data.edge_length,
                        depth_or_array_layers: data.edge_length,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D3,
                    format: wgpu::TextureFormat::Rgba16Float,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                },
                wgpu::util::TextureDataOrder::LayerMajor,
                &bytes,
            )
        });
        if let Some(kind) = error {
            return Err(RenderError::Gpu { kind });
        }
        let view = crate::pool::default_view(&texture);
        let mut metadata_bytes = Vec::with_capacity(32);
        for value in [
            data.domain_min[0],
            data.domain_min[1],
            data.domain_min[2],
            0.0,
            data.domain_max[0],
            data.domain_max[1],
            data.domain_max[2],
            0.0,
        ] {
            metadata_bytes.extend_from_slice(&value.to_le_bytes());
        }
        let metadata = self
            .gpu
            .device()
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some(&format!("alpha2 LUT metadata {id}")),
                contents: &metadata_bytes,
                usage: wgpu::BufferUsages::UNIFORM,
            });
        self.effect_resources.insert(
            id,
            RegisteredEffectResource {
                kind: EffectResourceKind::Lut3d,
                _texture: texture,
                view,
                metadata,
            },
        );
        // Results name a resource by id; its pixels may have just changed.
        self.clear_result_cache_for(crate::CacheInvalidation::EffectResources);
        Ok(())
    }

    /// Removes one uploaded effect resource and returns whether it existed.
    ///
    /// Removing one clears the cached results, since they may have used it.
    pub fn unregister_effect_resource(&mut self, id: &str) -> bool {
        let removed = self.effect_resources.remove(id).is_some();
        if removed {
            self.clear_result_cache_for(crate::CacheInvalidation::EffectResources);
        }
        removed
    }

    /// Removes all uploaded effect resources and clears the cached results.
    pub fn clear_effect_resources(&mut self) {
        self.effect_resources.clear();
        self.clear_result_cache_for(crate::CacheInvalidation::EffectResources);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_three_dimensional_cube() {
        let data = Lut3dData::from_cube(
            "TITLE \"identity\"\nLUT_3D_SIZE 2\nDOMAIN_MIN 0 0 0\nDOMAIN_MAX 1 1 1\n\
             0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n",
        )
        .expect("valid cube");
        assert_eq!(data.edge_length, 2);
        assert_triplet_eq(data.domain_min, [0.0; 3]);
        assert_eq!(data.values.len(), 8);
        assert_triplet_eq(data.values[7], [1.0, 1.0, 1.0]);
    }

    #[test]
    fn accepts_non_unit_domains_and_rejects_invalid_cubes() {
        let ranged = Lut3dData::from_cube(
            "LUT_3D_SIZE 2\nDOMAIN_MIN -1 -0.5 0\nDOMAIN_MAX 1 1.5 2\n\
             0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n",
        )
        .expect("per-channel domains are supported");
        assert_triplet_eq(ranged.domain_min, [-1.0, -0.5, 0.0]);
        assert_triplet_eq(ranged.domain_max, [1.0, 1.5, 2.0]);
        assert!(
            Lut3dData::from_cube("LUT_3D_SIZE 2\nDOMAIN_MIN 2 2 2\nDOMAIN_MAX 1 1 1\n").is_err()
        );
        assert!(Lut3dData::from_cube("LUT_3D_SIZE 2\n0 0 0\n").is_err());
    }

    #[test]
    fn a_cube_written_with_a_byte_order_mark_or_crlf_parses_as_without() {
        // A browser's UTF-8 decode drops a leading BOM and `fs::read_to_string` keeps it, so
        // the parser drops it too, or a native host refuses the file the browser previews.
        let body = "LUT_3D_SIZE 2\n0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n";
        let plain = Lut3dData::from_cube(body).expect("valid cube");
        for text in [
            format!("\u{feff}{body}"),
            format!("\u{feff}TITLE \"bom\"\n{body}"),
            format!("\u{feff}# written by a Windows tool\n{body}"),
            body.replace('\n', "\r\n"),
            format!("\u{feff}{}", body.replace('\n', "\r\n")),
        ] {
            assert_eq!(Lut3dData::from_cube(&text), Ok(plain.clone()), "{text:?}");
        }
        // Only a leading mark: one anywhere else is not whitespace.
        let inner = Lut3dData::from_cube(&body.replace("0 0 1\n", "\u{feff}0 0 1\n"))
            .expect_err("a mark inside the file is not text the parser knows");
        assert_eq!(inner.line, 6);
    }

    #[test]
    fn a_cube_refuses_one_dimensional_tables_and_unknown_directives_by_line() {
        let one_dimensional = Lut3dData::from_cube("TITLE \"1d\"\nLUT_1D_SIZE 2\n0 0 0\n1 1 1\n")
            .expect_err("a 1D table is a different transform");
        assert_eq!(one_dimensional.line, 2);
        assert!(one_dimensional.to_string().contains("one-dimensional"));
        let unknown = Lut3dData::from_cube("LUT_3D_SIZE 2\nLUT_3D_SHAPER 4\n")
            .expect_err("an unknown directive is refused, not skipped");
        assert_eq!(unknown.line, 2);
        assert!(unknown.to_string().contains("LUT_3D_SHAPER"));
    }

    fn assert_triplet_eq(actual: [f32; 3], expected: [f32; 3]) {
        assert!(
            actual
                .iter()
                .zip(expected)
                .all(|(actual, expected)| (*actual - expected).abs() <= f32::EPSILON)
        );
    }
}
