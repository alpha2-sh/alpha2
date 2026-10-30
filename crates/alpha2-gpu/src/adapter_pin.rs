//! Which adapter a product device is opened on: `ALPHA2_GPU_ADAPTER`, and the order adapters are
//! tried in without a pin.

/// How the renderer's adapter is chosen (`ALPHA2_GPU_ADAPTER`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdapterPin {
    /// The first the product order takes.
    Default,
    /// The first that encodes the host's default codec; the host decides which that is, and
    /// device selection outside the host treats it as [`Self::Default`].
    Encoder,
    /// The adapter at this position of its backend's enumeration.
    Index(usize),
    /// The first whose name contains this, ignoring case.
    Name(String),
}

impl AdapterPin {
    /// The pin `value` names; unset and empty are `default`.
    #[must_use]
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            None | Some("" | "default") => Self::Default,
            Some("encoder") => Self::Encoder,
            Some(text) => text
                .parse::<usize>()
                .map_or_else(|_| Self::Name(text.to_owned()), Self::Index),
        }
    }

    /// The pin `ALPHA2_GPU_ADAPTER` names.
    #[must_use]
    pub fn from_env() -> Self {
        Self::parse(std::env::var("ALPHA2_GPU_ADAPTER").ok().as_deref())
    }

    /// Whether this pin names one adapter.
    #[must_use]
    pub const fn is_pin(&self) -> bool {
        matches!(self, Self::Index(_) | Self::Name(_))
    }

    /// Whether this pin names the adapter at enumeration position `index` called `name`.
    #[must_use]
    pub fn names(&self, index: usize, name: &str) -> bool {
        match self {
            Self::Index(pinned) => index == *pinned,
            Self::Name(pinned) => name.to_lowercase().contains(&pinned.to_lowercase()),
            Self::Default | Self::Encoder => false,
        }
    }

    /// The setting as `ALPHA2_GPU_ADAPTER` spells it.
    #[must_use]
    pub fn setting(&self) -> String {
        match self {
            Self::Default => "ALPHA2_GPU_ADAPTER=default".to_owned(),
            Self::Encoder => "ALPHA2_GPU_ADAPTER=encoder".to_owned(),
            Self::Index(index) => format!("ALPHA2_GPU_ADAPTER={index}"),
            Self::Name(name) => format!("ALPHA2_GPU_ADAPTER={name}"),
        }
    }
}

/// Where an adapter of `device_type` comes in the high-performance order: discrete, integrated,
/// other, virtual, CPU, as wgpu-core ranks a high-performance request.
#[must_use]
pub fn high_performance_rank(device_type: wgpu::DeviceType) -> u8 {
    match device_type {
        wgpu::DeviceType::DiscreteGpu => 1,
        wgpu::DeviceType::IntegratedGpu => 2,
        wgpu::DeviceType::Other => 3,
        wgpu::DeviceType::VirtualGpu => 4,
        wgpu::DeviceType::Cpu => 5,
    }
}

/// The enumeration positions of `kinds` in the order a product request tries them: by
/// [`high_performance_rank`], enumeration order kept within a rank.
#[must_use]
pub fn product_order(kinds: &[(usize, wgpu::DeviceType)]) -> Vec<usize> {
    let mut ordered = kinds.to_vec();
    // A stable sort, so adapters of one kind keep the backend's own order.
    ordered.sort_by_key(|(_, kind)| high_performance_rank(*kind));
    ordered.into_iter().map(|(index, _)| index).collect()
}

/// Whether an OpenGL adapter whose `GL_VERSION` is `version` can run alpha2's shaders: desktop GL
/// 4.3 or GLES 3.1 and newer. A version that does not parse is accepted; the device request then
/// decides. Returns the refusal reason otherwise.
///
/// # Errors
/// The reason the version is too old.
pub fn gl_version_supported(version: &str) -> Result<(), String> {
    let (es, rest) = match version.trim().strip_prefix("OpenGL ES") {
        Some(rest) => (true, rest.trim_start()),
        None => (false, version.trim()),
    };
    let mut numbers = rest
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .map(str::parse::<u32>);
    let (Some(Ok(major)), Some(Ok(minor))) = (numbers.next(), numbers.next()) else {
        return Ok(());
    };
    let (needed, name) = if es {
        ((3, 1), "OpenGL ES")
    } else {
        ((4, 3), "OpenGL")
    };
    if (major, minor) >= needed {
        Ok(())
    } else {
        Err(format!(
            "{name} {major}.{minor} cannot run alpha2's shaders (needs desktop GL 4.3 or GLES 3.1)"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wgpu::DeviceType::{Cpu, DiscreteGpu, IntegratedGpu, Other, VirtualGpu};

    #[test]
    fn a_pin_is_an_index_a_name_or_a_policy() {
        for (value, pin) in [
            (None, AdapterPin::Default),
            (Some(""), AdapterPin::Default),
            (Some(" default "), AdapterPin::Default),
            (Some("encoder"), AdapterPin::Encoder),
            (Some("1"), AdapterPin::Index(1)),
            (Some("NVIDIA"), AdapterPin::Name("NVIDIA".to_owned())),
        ] {
            assert_eq!(AdapterPin::parse(value), pin, "{value:?}");
        }
        assert!(!AdapterPin::Encoder.is_pin());
        assert!(AdapterPin::Index(0).is_pin());
        assert!(AdapterPin::Name("intel".to_owned()).names(3, "Intel(R) Iris(R) Xe Graphics"));
        assert!(!AdapterPin::Name("intel".to_owned()).names(0, "NVIDIA GeForce MX450"));
        assert!(AdapterPin::Index(2).names(2, "any"));
        assert_eq!(AdapterPin::Index(2).setting(), "ALPHA2_GPU_ADAPTER=2");
    }

    #[test]
    fn gl_versions_below_desktop_4_3_and_gles_3_1_are_refused() {
        let refused = gl_version_supported("3.3.0 NVIDIA 580.105.08").expect_err("GL 3.3");
        assert!(refused.contains("OpenGL 3.3"), "{refused}");
        assert!(
            refused.contains("needs desktop GL 4.3 or GLES 3.1"),
            "{refused}"
        );
        assert_eq!(
            gl_version_supported("4.5 (Core Profile) Mesa 23.2.1"),
            Ok(())
        );
        assert_eq!(gl_version_supported("OpenGL ES 3.2 Mesa"), Ok(()));
        let es = gl_version_supported("OpenGL ES 3.0 ANGLE").expect_err("GLES 3.0");
        assert!(es.contains("OpenGL ES 3.0"), "{es}");
        assert_eq!(gl_version_supported("unknown"), Ok(()));
    }

    #[test]
    fn product_order_follows_the_high_performance_rank_and_keeps_enumeration_order_within_a_rank() {
        assert_eq!(
            product_order(&[
                (0, Cpu),
                (1, IntegratedGpu),
                (2, DiscreteGpu),
                (3, VirtualGpu),
                (4, IntegratedGpu),
                (5, Other),
                (6, DiscreteGpu),
            ]),
            [2, 6, 1, 4, 5, 3, 0]
        );
    }
}
