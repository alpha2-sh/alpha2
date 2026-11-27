//! Where a request's original or resource is: a path, or an `https:` URL.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer};

use super::RequestError;

/// The longest media URL a request may name.
pub const MAX_MEDIA_URL_BYTES: usize = 8192;

/// A value of `assets` or `resources`: an `https:` URL (scheme compared ASCII case-insensitively),
/// or a path. An `http:` URL is refused.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MediaLocation {
    /// A file.
    Path(PathBuf),
    /// An absolute `https:` URL without user info or fragment; its query is kept.
    Url(String),
}

impl MediaLocation {
    /// Reads a request's value.
    ///
    /// # Errors
    /// Why an `http:` or `https:` value is not a URL the host reads, without its query.
    pub fn parse(value: &str) -> Result<Self, String> {
        if starts_with_scheme(value, "http://") {
            return Err(format!(
                "{}: only https: media URLs are read",
                crate::remote::shown_url(value)
            ));
        }
        if !starts_with_scheme(value, "https://") {
            return Ok(Self::Path(PathBuf::from(value)));
        }
        media_url(value).map(Self::Url)
    }

    /// The path, if it is one.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Path(path) => Some(path),
            Self::Url(_) => None,
        }
    }

    /// The URL, if it is one.
    #[must_use]
    pub fn url(&self) -> Option<&str> {
        match self {
            Self::Path(_) => None,
            Self::Url(url) => Some(url),
        }
    }

    /// The location as messages show it: a path whole, a URL as
    /// [`crate::remote::shown_url`] shows it.
    #[must_use]
    pub fn shown(&self) -> String {
        match self {
            Self::Path(path) => path.display().to_string(),
            Self::Url(url) => crate::remote::shown_url(url),
        }
    }
}

impl From<PathBuf> for MediaLocation {
    fn from(path: PathBuf) -> Self {
        Self::Path(path)
    }
}

impl<'de> Deserialize<'de> for MediaLocation {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

fn starts_with_scheme(value: &str, scheme: &str) -> bool {
    value
        .get(..scheme.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(scheme))
}

/// An `https:` URL as the host sends it: scheme lower-cased, fragment dropped.
fn media_url(value: &str) -> Result<String, String> {
    let shown = crate::remote::shown_url(value);
    let refused = |why: &str| format!("{shown}: {why}");
    if value.len() > MAX_MEDIA_URL_BYTES {
        return Err(refused(&format!(
            "a media URL is at most {MAX_MEDIA_URL_BYTES} bytes"
        )));
    }
    if value.bytes().any(|byte| byte <= b' ' || byte == 0x7f) || !value.is_ascii() {
        return Err(refused(
            "a media URL holds no space, control or non-ASCII character",
        ));
    }
    let rest = &value["https://".len()..];
    let without_fragment = rest.split('#').next().unwrap_or_default();
    let authority_end = without_fragment
        .find(['/', '?'])
        .unwrap_or(without_fragment.len());
    let authority = &without_fragment[..authority_end];
    if authority.contains('@') {
        return Err(refused("a media URL carries no user info"));
    }
    let bracketed_host = authority.starts_with('[');
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, after) = bracketed
            .split_once(']')
            .ok_or_else(|| refused("the URL's IPv6 host is not closed"))?;
        if host.parse::<std::net::Ipv6Addr>().is_err() {
            return Err(refused("the URL's IPv6 host is not an address"));
        }
        match after {
            "" => (host, None),
            after => (
                host,
                Some(
                    after
                        .strip_prefix(':')
                        .ok_or_else(|| refused("the URL's port is malformed"))?,
                ),
            ),
        }
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    if host.is_empty()
        || !(bracketed_host
            || host
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-._~".contains(&byte)))
    {
        return Err(refused("the URL's host is not an ASCII host name"));
    }
    if let Some(port) = port
        && !port
            .parse::<u16>()
            .is_ok_and(|number| number > 0 && number.to_string() == port)
    {
        return Err(refused("the URL's port is not a number from 1 to 65535"));
    }
    Ok(format!("https://{without_fragment}"))
}

pub(super) fn refuse_url(field: &str, path: &Path) -> Result<(), RequestError> {
    let text = path.to_string_lossy();
    if starts_with_scheme(&text, "https://") || starts_with_scheme(&text, "http://") {
        return Err(RequestError::invalid_request(format!(
            "{field} takes a path, not a URL: {}",
            crate::remote::shown_url(&text)
        )));
    }
    Ok(())
}

pub(super) fn refuse_urls<'a>(
    proxies: &HashMap<String, Vec<super::ProxyRequest>>,
    frames: &HashMap<String, Vec<Option<PathBuf>>>,
    effect_assets: &HashMap<String, PathBuf>,
    fonts: impl IntoIterator<Item = &'a PathBuf>,
) -> Result<(), RequestError> {
    for proxy in proxies.values().flatten() {
        refuse_url("proxies", &proxy.path)?;
    }
    for path in frames.values().flatten().flatten() {
        refuse_url("frames", path)?;
    }
    for path in effect_assets.values() {
        refuse_url("effectAssets", path)?;
    }
    for path in fonts {
        refuse_url("fonts", path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_https_value_is_a_url_and_any_other_a_path() {
        assert_eq!(
            MediaLocation::parse("HTTPS://Media.example/a.mp4?sig=1#t=2"),
            Ok(MediaLocation::Url(
                "https://Media.example/a.mp4?sig=1".into()
            ))
        );
        assert_eq!(
            MediaLocation::parse("take:1.mov"),
            Ok(MediaLocation::Path("take:1.mov".into()))
        );
        assert_eq!(
            MediaLocation::parse("https://[::1]:8443/a"),
            Ok(MediaLocation::Url("https://[::1]:8443/a".into()))
        );
    }

    #[test]
    fn a_media_url_is_absolute_short_ascii_and_has_no_user_info() {
        for refused in [
            "https://user:pass@media.example/a.mp4",
            "https://media.example:0/a",
            "https://media.example:65536/a",
            "https://media.example:+80/a",
            "https://media.example:/a",
            "https://m\u{e9}dia.example/a",
            "https:///a",
            "https://media.example/a b",
        ] {
            assert!(MediaLocation::parse(refused).is_err(), "{refused}");
        }
        let long = format!("https://media.example/{}", "a".repeat(MAX_MEDIA_URL_BYTES));
        assert!(MediaLocation::parse(&long).is_err());
        let refusal = MediaLocation::parse("http://media.example/a?sig=secret").unwrap_err();
        assert_eq!(
            refusal,
            "http://media.example/a: only https: media URLs are read"
        );
    }
}
