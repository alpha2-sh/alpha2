//! What the operator who starts `serve` allows plugins, read once from its flags. A request
//! chooses inside this policy and cannot widen it.

use std::path::{Path, PathBuf};

use alpha2_host_core::plugins::{PluginError, ResourceRoot, codes};

/// Whether plugins run, and which kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginMode {
    /// No plugin runs.
    Off,
    /// Only wasm artifacts run; a request's `allow_native` is ignored.
    Wasm,
    /// A request may also run a native artifact with `allow_native`.
    Native,
}

/// An `https:` origin: a lower-case host and a port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Origin {
    host: String,
    port: u16,
}

impl Origin {
    /// The origin of an `https:` URL; `None` for any other URL.
    #[must_use]
    pub fn of_url(url: &str) -> Option<Self> {
        let rest = url
            .get(..8)
            .filter(|scheme| scheme.eq_ignore_ascii_case("https://"))
            .map(|_| &url[8..])?;
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        let host_port = authority
            .rsplit_once('@')
            .map_or(authority, |(_, after)| after);
        let (host, port) = if let Some(bracketed) = host_port.strip_prefix('[') {
            let (host, after) = bracketed.split_once(']')?;
            (format!("[{host}]"), after.strip_prefix(':'))
        } else {
            match host_port.rsplit_once(':') {
                Some((host, port)) => (host.to_owned(), Some(port)),
                None => (host_port.to_owned(), None),
            }
        };
        if host.is_empty() || host.contains(char::is_whitespace) {
            return None;
        }
        let port = match port {
            None | Some("") => 443,
            Some(port) => port.parse().ok()?,
        };
        Some(Self {
            host: host.to_ascii_lowercase(),
            port,
        })
    }

    /// Reads a `--plugin-origins` entry, `https://host[:port]` with nothing after it.
    fn parse(entry: &str) -> Result<Self, String> {
        let refused = || format!("--plugin-origins takes https://host[:port] entries, got {entry}");
        let bare = entry.strip_suffix('/').unwrap_or(entry);
        let authority = bare.get(8..).ok_or_else(refused)?;
        if authority.contains(['/', '?', '#', '@']) {
            return Err(refused());
        }
        Self::of_url(bare).ok_or_else(refused)
    }
}

impl std::fmt::Display for Origin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.port == 443 {
            write!(formatter, "https://{}", self.host)
        } else {
            write!(formatter, "https://{}:{}", self.host, self.port)
        }
    }
}

/// Where plugin files may be fetched from over the network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origins {
    /// A manifest from any `https:` origin; its files and redirects only on that origin.
    ManifestOnly,
    /// No network fetch.
    None,
    /// Manifests, files and redirects only on these origins, and across them.
    List(Vec<Origin>),
}

/// Where local manifests may lie.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Root {
    /// Anywhere.
    Any,
    /// Nowhere: only `https:` manifests.
    None,
    /// Under this canonical directory, after links are resolved.
    Dir(PathBuf),
}

/// The hidden subcommand a host runs a native plugin in, as its own child.
pub const PLUGIN_WORKER: &str = "plugin-worker";

/// In a test, this test binary's [`TEST_WORKER`] test stands in for the subcommand.
fn worker_arguments() -> Vec<std::ffi::OsString> {
    #[cfg(not(test))]
    {
        vec![PLUGIN_WORKER.into()]
    }
    #[cfg(test)]
    {
        [
            "--exact",
            TEST_WORKER,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ]
        .map(Into::into)
        .to_vec()
    }
}

fn worker_environment() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    #[cfg(not(test))]
    {
        Vec::new()
    }
    #[cfg(test)]
    {
        vec![(TEST_WORKER_ENV.into(), "1".into())]
    }
}

#[cfg(test)]
pub const TEST_WORKER: &str = "tests::plugins::plugin_worker_child";

#[cfg(test)]
pub const TEST_WORKER_ENV: &str = "ALPHA2_TEST_PLUGIN_WORKER";

/// Where native artifacts run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginIsolation {
    /// In a worker process per run, `alpha2-host plugin-worker`.
    Process,
    /// In the host process.
    InProcess,
}

/// The operator's plugin policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginPolicy {
    /// `--plugins`.
    pub mode: PluginMode,
    /// `--plugin-origins`.
    pub origins: Origins,
    /// `--plugin-root`.
    pub root: Root,
    /// `--plugin-isolation`.
    pub isolation: PluginIsolation,
    /// `--plugin-resource-root`.
    pub resource_root: ResourceRoot,
}

fn refused(message: impl Into<String>) -> PluginError {
    PluginError::new(codes::REFUSED, message)
}

impl PluginPolicy {
    /// A host started without plugin flags.
    pub const DEFAULT: Self = Self {
        mode: PluginMode::Wasm,
        origins: Origins::ManifestOnly,
        root: Root::Any,
        isolation: PluginIsolation::Process,
        resource_root: ResourceRoot::None,
    };

    /// Reads `--plugins`.
    pub(crate) fn set_mode(&mut self, value: &str) -> Result<(), String> {
        self.mode = match value {
            "off" => PluginMode::Off,
            "wasm" => PluginMode::Wasm,
            "native" => PluginMode::Native,
            _ => return Err(format!("--plugins takes off, wasm or native, got {value}")),
        };
        Ok(())
    }

    /// Reads `--plugin-origins`.
    pub(crate) fn set_origins(&mut self, value: &str) -> Result<(), String> {
        self.origins = if value == "none" {
            Origins::None
        } else {
            let list = value
                .split(',')
                .map(|entry| Origin::parse(entry.trim()))
                .collect::<Result<Vec<_>, _>>()?;
            Origins::List(list)
        };
        Ok(())
    }

    /// Reads `--plugin-root`; a directory must exist and is resolved against the working directory.
    pub(crate) fn set_root(&mut self, value: &str) -> Result<(), String> {
        self.root = match value {
            "any" => Root::Any,
            "none" => Root::None,
            directory => Root::Dir(std::fs::canonicalize(directory).map_err(|error| {
                format!("--plugin-root takes any, none or a directory, got {directory}: {error}")
            })?),
        };
        Ok(())
    }

    /// Reads `--plugin-isolation`.
    pub(crate) fn set_isolation(&mut self, value: &str) -> Result<(), String> {
        self.isolation = match value {
            "process" => PluginIsolation::Process,
            "in-process" => PluginIsolation::InProcess,
            _ => {
                return Err(format!(
                    "--plugin-isolation takes process or in-process, got {value}"
                ));
            }
        };
        Ok(())
    }

    /// Reads `--plugin-resource-root`; a directory must exist and is resolved against the working
    /// directory.
    pub(crate) fn set_resource_root(&mut self, value: &str) -> Result<(), String> {
        self.resource_root = match value {
            "any" => ResourceRoot::Any,
            "none" => ResourceRoot::None,
            directory => ResourceRoot::Dir(std::fs::canonicalize(directory).map_err(|error| {
                format!(
                    "--plugin-resource-root takes none, any or a directory, got {directory}: \
                     {error}"
                )
            })?),
        };
        Ok(())
    }

    /// Where a native artifact runs under `--plugin-isolation`.
    ///
    /// # Errors
    /// `PLUGIN_LOAD_FAILED` when this host cannot name its own program.
    pub fn isolation(&self) -> Result<alpha2_host_core::plugins::Isolation, PluginError> {
        use alpha2_host_core::plugins::{Isolation, WorkerCommand};
        Ok(match self.isolation {
            PluginIsolation::InProcess => Isolation::InProcess,
            PluginIsolation::Process => Isolation::Child(WorkerCommand {
                program: std::env::current_exe().map_err(|error| {
                    PluginError::new(
                        codes::LOAD_FAILED,
                        format!(
                            "the host cannot name its own program for a plugin worker: {error}"
                        ),
                    )
                })?,
                args: worker_arguments(),
                envs: worker_environment(),
            }),
        })
    }

    /// Refuses every plugin under `--plugins off`.
    ///
    /// # Errors
    /// `PLUGIN_REFUSED`.
    pub fn allows_plugins(&self) -> Result<(), PluginError> {
        if self.mode == PluginMode::Off {
            return Err(refused(
                "this host was started with --plugins off, so it runs no plugin",
            ));
        }
        Ok(())
    }

    /// Whether a request's `allow_native` takes effect.
    #[must_use]
    pub fn allows_native(&self) -> bool {
        self.mode == PluginMode::Native
    }

    /// Checks the URL of a manifest before it is contacted.
    ///
    /// # Errors
    /// `PLUGIN_REFUSED` naming `--plugin-origins`.
    pub fn check_manifest_url(&self, url: &str) -> Result<Origin, PluginError> {
        let origin = Origin::of_url(url).ok_or_else(|| {
            refused(format!(
                "{}: not an https: URL with a host",
                crate::plugin_fetch::shown_url(url)
            ))
        })?;
        match &self.origins {
            Origins::ManifestOnly => Ok(origin),
            Origins::None => Err(refused(
                "this host was started with --plugin-origins none, so it fetches no plugin over \
                 the network",
            )),
            Origins::List(list) if list.contains(&origin) => Ok(origin),
            Origins::List(_) => Err(refused(format!(
                "{origin} is not an origin this host was started with in --plugin-origins"
            ))),
        }
    }

    /// Checks the path of a local manifest before it is read.
    ///
    /// # Errors
    /// `PLUGIN_REFUSED` naming `--plugin-root`.
    pub fn check_manifest_path(&self, path: &Path) -> Result<(), PluginError> {
        match &self.root {
            Root::Any => Ok(()),
            Root::None => Err(refused(
                "this host was started with --plugin-root none, so it reads no local manifest",
            )),
            Root::Dir(root) => match std::fs::canonicalize(path) {
                Ok(resolved) if resolved.starts_with(root) => Ok(()),
                _ => Err(refused(format!(
                    "{}: not under the directory this host was started with in --plugin-root",
                    path.display()
                ))),
            },
        }
    }

    /// Whether a plugin file or a redirect hop may be fetched from `origin`, for a manifest from
    /// `manifest` (`None` for a local manifest).
    #[must_use]
    pub fn allows_origin(&self, origin: &Origin, manifest: Option<&Origin>) -> bool {
        match &self.origins {
            Origins::ManifestOnly => manifest == Some(origin),
            Origins::None => false,
            Origins::List(list) => list.contains(origin),
        }
    }

    /// Why `origin` was refused, naming the flag.
    #[must_use]
    pub fn origin_refusal(&self, url: &str, origin: Option<&Origin>) -> PluginError {
        match (&self.origins, origin) {
            (_, None) => refused(format!("{url}: only https: URLs are fetched")),
            (Origins::None, _) => refused(format!(
                "{url}: this host was started with --plugin-origins none"
            )),
            (Origins::ManifestOnly, Some(origin)) => refused(format!(
                "{url}: {origin} is not the manifest's own origin; list it in --plugin-origins to \
                 fetch from it"
            )),
            (Origins::List(_), Some(origin)) => refused(format!(
                "{url}: {origin} is not an origin this host was started with in --plugin-origins"
            )),
        }
    }
}

impl Default for PluginPolicy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The policy of a host started without plugin flags.
pub static DEFAULT_POLICY: PluginPolicy = PluginPolicy::DEFAULT;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_origin_is_the_lower_case_host_and_port_of_an_https_url() {
        let origin = |url| Origin::of_url(url).map(|origin| origin.to_string());
        assert_eq!(
            origin("https://Example.COM/p/m.json?x"),
            Some("https://example.com".to_owned())
        );
        assert_eq!(
            origin("https://user:pw@example.com:8443"),
            Some("https://example.com:8443".to_owned())
        );
        assert_eq!(
            origin("https://example.com:443/"),
            origin("https://example.com")
        );
        assert_eq!(
            origin("https://[::1]:9/a"),
            Some("https://[::1]:9".to_owned())
        );
        for refused in [
            "http://example.com/",
            "https:///a",
            "https://a:x/",
            "file:///a",
        ] {
            assert_eq!(origin(refused), None, "{refused}");
        }
    }
}
