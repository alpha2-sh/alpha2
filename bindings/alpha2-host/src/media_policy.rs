//! What the operator who starts the host allows remote media, read once from its flags. A
//! request names URLs inside this policy and cannot widen it.

use std::collections::HashMap;
use std::sync::OnceLock;

use alpha2_host_core::remote::{
    RefusalPolicy, RemoteError, RemoteFailure, TransportFailure, shown_url,
};
use alpha2_host_core::requests::MediaLocation;
use serde_json::{Value, json};

use crate::origin::Origin;
use crate::protocol::HostError;

/// The origins remote media may be read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaOrigins {
    /// None: every URL is refused before anything is contacted.
    None,
    /// These, and redirects among them.
    List(Vec<Origin>),
}

/// Whether the host connects to an address that is not global unicast.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrivateAddresses {
    /// Never.
    Refuse,
    /// Like any other.
    Allow,
}

/// The operator's remote media policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaPolicy {
    /// `--media-origins`.
    pub origins: MediaOrigins,
    /// `--media-private-addresses`.
    pub private_addresses: PrivateAddresses,
}

impl Default for MediaPolicy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl MediaPolicy {
    /// A host started without media flags.
    pub const DEFAULT: Self = Self {
        origins: MediaOrigins::None,
        private_addresses: PrivateAddresses::Refuse,
    };

    /// Reads `--media-origins`.
    pub(crate) fn set_origins(&mut self, value: &str) -> Result<(), String> {
        self.origins = if value == "none" {
            MediaOrigins::None
        } else {
            MediaOrigins::List(
                value
                    .split(',')
                    .map(|entry| Origin::parse(entry.trim(), "--media-origins"))
                    .collect::<Result<Vec<_>, _>>()?,
            )
        };
        Ok(())
    }

    /// Reads `--media-private-addresses`.
    pub(crate) fn set_private_addresses(&mut self, value: &str) -> Result<(), String> {
        self.private_addresses = match value {
            "refuse" => PrivateAddresses::Refuse,
            "allow" => PrivateAddresses::Allow,
            _ => {
                return Err(format!(
                    "--media-private-addresses takes refuse or allow, got {value}"
                ));
            }
        };
        Ok(())
    }

    /// What `capabilities` reports of the policy as `remoteMedia`.
    pub fn capabilities_json(&self) -> Value {
        json!({
            "origins": match self.origins {
                MediaOrigins::None => "none",
                MediaOrigins::List(_) => "listed",
            },
            "privateAddresses": match self.private_addresses {
                PrivateAddresses::Refuse => "refuse",
                PrivateAddresses::Allow => "allow",
            },
        })
    }

    fn lists(&self, origin: &Origin) -> bool {
        matches!(&self.origins, MediaOrigins::List(list) if list.contains(origin))
    }

    /// Whether `url` may be contacted, checked before it is: `policy` names the rule a refusal
    /// reports (`Origins` for a request's URL, `Redirect` for a redirect's target).
    ///
    /// # Errors
    /// The refusal, with the URL shown without its query.
    pub fn check(&self, url: &str, policy: RefusalPolicy) -> Result<(), TransportFailure> {
        let origin = Origin::of_url(url);
        if origin.as_ref().is_some_and(|origin| self.lists(origin)) {
            return Ok(());
        }
        let shown = shown_url(url);
        let message = match (&self.origins, origin) {
            (MediaOrigins::None, _) => format!(
                "{shown}: this host was started with --media-origins none, so it reads no remote \
                 media"
            ),
            (_, Some(origin)) if policy == RefusalPolicy::Redirect => format!(
                "{shown}: a redirect leaves the origins this host was started with in \
                 --media-origins ({origin} is not one)"
            ),
            (_, Some(origin)) => format!(
                "{shown}: {origin} is not an origin this host was started with in --media-origins"
            ),
            (_, None) => format!("{shown}: only https: media URLs are read"),
        };
        Err(TransportFailure::Refused { policy, message })
    }
}

/// The protocol error a remote read of `source`'s `url` failed with: its code, and `details`
/// naming the source, the shown URL, the status and the refusing rule when there are ones.
pub fn media_error(source: &str, url: &str, error: &RemoteError) -> HostError {
    let mut details = json!({ "source": source, "url": shown_url(url) });
    if let Some(status) = error.status {
        details["status"] = json!(status);
    }
    match error.failure {
        RemoteFailure::Refused(policy) => details["policy"] = json!(policy.name()),
        RemoteFailure::Busy { retry_after_ms } => details["retryAfterMs"] = json!(retry_after_ms),
        _ => {}
    }
    HostError::new(error.failure.code(), error.message.clone()).with_details(details)
}

/// `MEDIA_REFUSED` for the first URL of `assets` or `resources`, by source ID, that the
/// policy refuses, before anything is contacted.
///
/// # Errors
/// That refusal.
pub fn refuse_unlisted(
    policy: &MediaPolicy,
    assets: &HashMap<String, MediaLocation>,
    resources: &HashMap<String, HashMap<String, MediaLocation>>,
) -> Result<(), HostError> {
    let mut urls = assets
        .iter()
        .filter_map(|(source, location)| Some((source.as_str(), location.url()?)))
        .chain(resources.iter().flat_map(|(source, locators)| {
            locators
                .values()
                .filter_map(move |location| Some((source.as_str(), location.url()?)))
        }))
        .collect::<Vec<_>>();
    urls.sort_unstable();
    for (source, url) in urls {
        policy
            .check(url, RefusalPolicy::Origins)
            .map_err(|refusal| media_error(source, url, &RemoteError::from(refusal)))?;
    }
    Ok(())
}

static PROCESS: OnceLock<MediaPolicy> = OnceLock::new();

#[cfg(test)]
thread_local! {
    static OVERRIDE: std::cell::RefCell<Option<MediaPolicy>> =
        const { std::cell::RefCell::new(None) };
}

/// Takes `--media-origins` and `--media-private-addresses` out of a `serve`, `export` or `conform`
/// command line and makes them the process's policy. No request can set it.
///
/// # Errors
/// A flag without a value or with one it does not take.
pub fn take_flags(arguments: &mut Vec<String>) -> Result<(), String> {
    let policy = flags(arguments)?;
    let _ = PROCESS.set(policy);
    Ok(())
}

fn flags(arguments: &mut Vec<String>) -> Result<MediaPolicy, String> {
    let mut policy = MediaPolicy::DEFAULT;
    if !matches!(
        arguments.first().map(String::as_str),
        Some("serve" | "export" | "conform")
    ) {
        return Ok(policy);
    }
    let mut at = 1;
    while at < arguments.len() {
        let flag = arguments[at].as_str();
        if !matches!(flag, "--media-origins" | "--media-private-addresses") {
            at += 1;
            continue;
        }
        let value = arguments
            .get(at + 1)
            .cloned()
            .ok_or_else(|| format!("{flag} needs a value"))?;
        if flag == "--media-origins" {
            policy.set_origins(&value)?;
        } else {
            policy.set_private_addresses(&value)?;
        }
        arguments.drain(at..at + 2);
    }
    Ok(policy)
}

/// The policy requests on this thread read: the process's, or under test one the test set.
pub fn current() -> MediaPolicy {
    #[cfg(test)]
    if let Some(policy) = OVERRIDE.with(|policy| policy.borrow().clone()) {
        return policy;
    }
    PROCESS.get().cloned().unwrap_or_default()
}

#[cfg(test)]
pub struct TestPolicy(Option<MediaPolicy>);

#[cfg(test)]
impl Drop for TestPolicy {
    fn drop(&mut self) {
        let previous = self.0.take();
        OVERRIDE.with(|policy| *policy.borrow_mut() = previous);
    }
}

/// Makes `policy` the one requests on this thread read until the guard drops.
#[cfg(test)]
pub fn set_for_test(policy: MediaPolicy) -> TestPolicy {
    TestPolicy(OVERRIDE.with(|current| current.borrow_mut().replace(policy)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    fn policy(flags: &str) -> Result<MediaPolicy, String> {
        super::flags(&mut arguments(&format!("serve {flags}")))
    }

    #[test]
    fn media_origins_take_the_plugin_origin_entries() {
        assert_eq!(policy(""), Ok(MediaPolicy::DEFAULT));
        assert_eq!(
            policy("--media-origins https://A.example,https://b.example:8443/").map(|p| p.origins),
            Ok(MediaOrigins::List(vec![
                Origin::of_url("https://a.example").expect("origin"),
                Origin::of_url("https://b.example:8443").expect("origin"),
            ]))
        );
        for refused in [
            "http://a.example",
            "/srv/media",
            "https://user@a.example",
            "https://a.example:port",
            "https://a.example/path",
            "https://a.example?q",
        ] {
            let error = policy(&format!("--media-origins {refused}")).unwrap_err();
            assert!(
                error.starts_with("--media-origins takes https://host[:port] entries"),
                "{refused}: {error}"
            );
        }
        assert_eq!(
            policy("--media-private-addresses allow").map(|p| p.private_addresses),
            Ok(PrivateAddresses::Allow)
        );
        assert!(policy("--media-private-addresses some").is_err());
    }

    #[test]
    fn the_media_flags_leave_the_command_line_of_the_commands_that_take_them() {
        let mut line = arguments(
            "export --project p.json --media-origins https://a.example --out o.mp4 \
             --media-private-addresses allow",
        );
        let read = flags(&mut line).expect("flags");
        assert_eq!(line, arguments("export --project p.json --out o.mp4"));
        assert_eq!(read.private_addresses, PrivateAddresses::Allow);
        let mut line = arguments("probe-gpu-encode --media-origins none");
        flags(&mut line).expect("not a media command");
        assert_eq!(line.len(), 3);
        assert!(flags(&mut arguments("serve --media-origins")).is_err());
    }

    #[test]
    fn a_url_off_the_list_is_refused_before_it_is_contacted() {
        let listed = policy("--media-origins https://media.example").expect("policy");
        assert_eq!(
            listed.check("https://media.example/a.mp4?sig=1", RefusalPolicy::Origins),
            Ok(())
        );
        let Err(TransportFailure::Refused { policy, message }) = listed.check(
            "https://other.example/a.mp4?sig=secret",
            RefusalPolicy::Origins,
        ) else {
            panic!("an unlisted origin is refused");
        };
        assert_eq!(policy, RefusalPolicy::Origins);
        assert!(!message.contains("secret"), "{message}");
        let Err(TransportFailure::Refused { message, .. }) =
            MediaPolicy::DEFAULT.check("https://media.example/a.mp4", RefusalPolicy::Origins)
        else {
            panic!("no origin is listed by default");
        };
        assert!(message.contains("--media-origins none"), "{message}");
    }
}
