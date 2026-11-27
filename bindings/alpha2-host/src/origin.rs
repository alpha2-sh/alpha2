//! An `https:` origin, as the plugin and media origin lists name it.

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

    /// Reads an origin list's entry, `https://host[:port]` with nothing after it; `flag` names the
    /// list in the refusal.
    pub(crate) fn parse(entry: &str, flag: &str) -> Result<Self, String> {
        let refused = || format!("{flag} takes https://host[:port] entries, got {entry}");
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
