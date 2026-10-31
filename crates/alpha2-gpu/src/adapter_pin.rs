//! Which adapter a product device is opened on: `ALPHA2_GPU_ADAPTER`, and the order adapters are
//! tried in without a pin.

/// How the renderer's adapter is chosen (`ALPHA2_GPU_ADAPTER`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdapterPolicy {
    /// The host ranks the hardware adapters by what each encodes and decodes on the GPU, then by
    /// the high-performance order ([`crate::rank_adapters`]). Device selection that knows nothing
    /// of video (a product request on its own) takes the high-performance order alone. The
    /// default on Linux and macOS.
    Auto,
    /// The first hardware adapter in the high-performance order, whatever it encodes or decodes.
    /// The default on Windows ([`Self::platform_default`]).
    Performance,
    /// The adapter at this position of its backend's enumeration.
    Index(usize),
    /// The first whose name contains this, ignoring case.
    Name(String),
}

impl AdapterPolicy {
    /// The policy when `ALPHA2_GPU_ADAPTER` is unset: `auto` on Linux and macOS, `performance` on
    /// Windows. On a hybrid Windows laptop with an Intel integrated GPU, `auto` would often render
    /// there (its video engines encode H.264 and HEVC), and with the wgpu-hal this build uses some
    /// Intel integrated GPUs draw leftover memory into DX12 textures that should start out
    /// cleared. Until that is fixed, Windows renders on the first hardware adapter in the
    /// high-performance order and `auto` is there to ask for.
    #[must_use]
    pub const fn platform_default() -> Self {
        if cfg!(target_os = "windows") {
            Self::Performance
        } else {
            Self::Auto
        }
    }

    /// The policy `value` names; unset and empty are [`Self::platform_default`]. Any text that is
    /// not a policy or a number is a name, so a removed policy name is a pin that matches no
    /// adapter.
    #[must_use]
    pub fn parse(value: Option<&str>) -> Self {
        Self::parse_set(value).unwrap_or_else(Self::platform_default)
    }

    /// The policy `value` names, or `None` when it names none (unset or empty).
    #[must_use]
    pub fn parse_set(value: Option<&str>) -> Option<Self> {
        match value.map(str::trim) {
            None | Some("") => None,
            Some("auto") => Some(Self::Auto),
            Some("performance") => Some(Self::Performance),
            Some(text) => Some(
                text.parse::<usize>()
                    .map_or_else(|_| Self::Name(text.to_owned()), Self::Index),
            ),
        }
    }

    /// The policy `ALPHA2_GPU_ADAPTER` names, else [`Self::platform_default`].
    #[must_use]
    pub fn from_env() -> Self {
        Self::parse(std::env::var("ALPHA2_GPU_ADAPTER").ok().as_deref())
    }

    /// The policy `ALPHA2_GPU_ADAPTER` names, or `None` when it is unset.
    #[must_use]
    pub fn set_in_env() -> Option<Self> {
        Self::parse_set(std::env::var("ALPHA2_GPU_ADAPTER").ok().as_deref())
    }

    /// Whether this policy names one adapter.
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
            Self::Auto | Self::Performance => false,
        }
    }

    /// The setting as `ALPHA2_GPU_ADAPTER` spells it.
    #[must_use]
    pub fn setting(&self) -> String {
        match self {
            Self::Auto => "ALPHA2_GPU_ADAPTER=auto".to_owned(),
            Self::Performance => "ALPHA2_GPU_ADAPTER=performance".to_owned(),
            Self::Index(index) => format!("ALPHA2_GPU_ADAPTER={index}"),
            Self::Name(name) => format!("ALPHA2_GPU_ADAPTER={name}"),
        }
    }

    /// The policy as a capability report names it: `auto`, `performance`, `index:<n>` or
    /// `name:<text>`.
    #[must_use]
    pub fn report_name(&self) -> String {
        match self {
            Self::Auto => "auto".to_owned(),
            Self::Performance => "performance".to_owned(),
            Self::Index(index) => format!("index:{index}"),
            Self::Name(name) => format!("name:{name}"),
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
    fn a_policy_is_auto_performance_an_index_or_a_name_and_unset_is_the_platform_default() {
        assert_eq!(
            AdapterPolicy::platform_default(),
            if cfg!(target_os = "windows") {
                AdapterPolicy::Performance
            } else {
                AdapterPolicy::Auto
            }
        );
        for (value, policy) in [
            (None, AdapterPolicy::platform_default()),
            (Some(""), AdapterPolicy::platform_default()),
            (Some(" auto "), AdapterPolicy::Auto),
            (Some("performance"), AdapterPolicy::Performance),
            (Some("1"), AdapterPolicy::Index(1)),
            (Some("NVIDIA"), AdapterPolicy::Name("NVIDIA".to_owned())),
            // Removed policies are names, which no adapter has.
            (Some("encoder"), AdapterPolicy::Name("encoder".to_owned())),
            (Some("default"), AdapterPolicy::Name("default".to_owned())),
        ] {
            assert_eq!(AdapterPolicy::parse(value), policy, "{value:?}");
        }
        assert!(!AdapterPolicy::Performance.is_pin());
        assert!(AdapterPolicy::Index(0).is_pin());
        assert!(AdapterPolicy::Name("intel".to_owned()).names(3, "Intel(R) Iris(R) Xe Graphics"));
        assert!(!AdapterPolicy::Name("intel".to_owned()).names(0, "NVIDIA GeForce MX450"));
        assert!(AdapterPolicy::Index(2).names(2, "any"));
        assert_eq!(AdapterPolicy::Index(2).setting(), "ALPHA2_GPU_ADAPTER=2");
        assert_eq!(AdapterPolicy::Index(2).report_name(), "index:2");
        assert_eq!(
            AdapterPolicy::Name("Iris".to_owned()).report_name(),
            "name:Iris"
        );
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
