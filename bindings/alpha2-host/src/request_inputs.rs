//! The files a request reads, and the refusal of an output that is one of
//! them.
//!
//! Replacing a file while it is decoded is the one way an output could
//! corrupt the render that writes it, so an output that is the same file as
//! any input is refused before any work, whether or not the request
//! overwrites. Files are compared by identity ([`alpha2_os::same_file_at`]),
//! not by path, so a hard link or a path through a symbolic link is caught.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::assets::ProxyRequest;
use crate::protocol::HostError;

/// Every file path a request names as an input.
pub struct RequestInputs<'a> {
    pub assets: &'a HashMap<String, PathBuf>,
    pub proxies: &'a HashMap<String, Vec<ProxyRequest>>,
    pub frames: &'a HashMap<String, Vec<Option<PathBuf>>>,
    pub resources: &'a HashMap<String, HashMap<String, PathBuf>>,
    pub effect_assets: &'a HashMap<String, PathBuf>,
    pub font_files: &'a [PathBuf],
}

impl RequestInputs<'_> {
    /// Each input path with the words that name it in a refusal, in a fixed
    /// order (sorted by ID within each kind), so the input a refusal names
    /// does not depend on hash order.
    fn named(&self) -> Vec<(String, &Path)> {
        let mut named = Vec::new();
        for (id, path) in sorted(self.assets) {
            named.push((format!("the asset {id:?}"), path.as_path()));
        }
        for (id, proxies) in sorted(self.proxies) {
            for proxy in proxies {
                named.push((format!("a proxy of {id:?}"), proxy.path.as_path()));
            }
        }
        for (id, frames) in sorted(self.frames) {
            for path in frames.iter().flatten() {
                named.push((format!("a frame of {id:?}"), path.as_path()));
            }
        }
        for (id, resources) in sorted(self.resources) {
            for (locator, path) in sorted(resources) {
                named.push((
                    format!("the resource {locator:?} of {id:?}"),
                    path.as_path(),
                ));
            }
        }
        for (id, path) in sorted(self.effect_assets) {
            named.push((format!("the effect asset {id:?}"), path.as_path()));
        }
        for path in self.font_files {
            named.push((format!("the font {}", path.display()), path.as_path()));
        }
        named
    }

    /// Refuses `output` when it is the same file as any input. Nothing at
    /// `output` is fine.
    ///
    /// # Errors
    /// `INVALID_REQUEST` naming the input, or `IO_ERROR` when a file's
    /// identity cannot be read.
    pub fn refuse_output(&self, output: &Path) -> Result<(), HostError> {
        if std::fs::symlink_metadata(output).is_err() {
            return Ok(());
        }
        for (name, input) in self.named() {
            if alpha2_os::same_file_at(output, input).map_err(HostError::io)? {
                return Err(HostError::new(
                    "INVALID_REQUEST",
                    format!(
                        "the output {} is {name}: write it elsewhere",
                        output.display()
                    ),
                ));
            }
        }
        Ok(())
    }
}

fn sorted<T>(map: &HashMap<String, T>) -> Vec<(&String, &T)> {
    let mut entries: Vec<_> = map.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    entries
}
