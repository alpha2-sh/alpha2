//! Request checks for the provider sources the shared provider media reads.

use std::collections::HashMap;
use std::path::PathBuf;

use alpha2_model::{Project, ProviderSource};

/// # Errors
/// Returns an `INVALID_INPUT` message naming a source that is not an SVG or Lottie source of
/// `project`.
pub fn check_resources(
    project: &Project,
    resources: &HashMap<String, HashMap<String, PathBuf>>,
) -> Result<(), String> {
    let mut named = resources.keys().collect::<Vec<_>>();
    named.sort();
    for id in named {
        let reads_resources = project.sources.iter().any(|source| {
            source.id.as_str() == id
                && matches!(
                    source.provider,
                    Some(ProviderSource::Svg | ProviderSource::Lottie { .. })
                )
        });
        if !reads_resources {
            return Err(crate::protocol::code_tag(
                "INVALID_INPUT",
                format!(
                    "resources names {id}, which is not an SVG or Lottie source of the project: \
                     only those read files outside themselves"
                ),
            ));
        }
    }
    Ok(())
}

/// A provider failure as the protocol answers it: tagged with its code when it has one.
#[allow(clippy::needless_pass_by_value)] // Taken by `map_err`.
pub fn tagged(error: alpha2_host_core::providers::ProviderError) -> String {
    match error.code {
        Some(code) => crate::protocol::code_tag(code, error.message),
        None => error.message,
    }
}
