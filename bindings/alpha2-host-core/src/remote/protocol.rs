//! The checks on each range response (RFC 9110 sections 13, 14 and 15).

use std::time::{Duration, SystemTime};

use super::{Precondition, RangeResponse, RemoteError, RemoteFailure};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ObjectFacts {
    pub total: u64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

impl ObjectFacts {
    pub fn precondition(&self) -> Option<Precondition> {
        self.etag.clone().map(Precondition::IfMatch).or_else(|| {
            self.last_modified
                .clone()
                .map(Precondition::IfUnmodifiedSince)
        })
    }
}

pub(super) enum StatusClass {
    Partial,
    Retry(Option<Duration>),
    Failed(RemoteError),
}

type ContentRange = (Option<(u64, u64)>, Option<u64>);

fn content_range(value: &str) -> Option<ContentRange> {
    let rest = value.trim().strip_prefix("bytes")?.trim_start();
    let (range, total) = rest.split_once('/')?;
    let total = match total.trim() {
        "*" => None,
        digits => Some(digits.parse().ok()?),
    };
    let range = match range.trim() {
        "*" => None,
        range => {
            let (first, last) = range.split_once('-')?;
            let (first, last) = (first.trim().parse().ok()?, last.trim().parse().ok()?);
            (first <= last).then_some(())?;
            Some((first, last))
        }
    };
    Some((range, total))
}

fn opaque_tag(tag: &str) -> &str {
    tag.trim().strip_prefix("W/").unwrap_or(tag.trim())
}

fn is_strong(tag: &str) -> bool {
    let tag = tag.trim();
    !tag.starts_with("W/") && tag.len() >= 2 && tag.starts_with('"') && tag.ends_with('"')
}

/// RFC 9110 section 10.2.3: delay-seconds or an HTTP-date, from `now`.
pub(super) fn retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let at = http_date(value)?;
    Some(at.duration_since(now).unwrap_or(Duration::ZERO))
}

/// An IMF-fixdate (RFC 9110 section 5.6.7), such as `Sun, 06 Nov 1994 08:49:37 GMT`.
pub(super) fn http_date(value: &str) -> Option<SystemTime> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mut parts = value.split_ascii_whitespace();
    let _weekday = parts.next()?.strip_suffix(',')?;
    let day: u64 = parts.next()?.parse().ok()?;
    let name = parts.next()?;
    let month = MONTHS.iter().position(|month| *month == name)?;
    let year: u64 = parts.next()?.parse().ok()?;
    let mut clock = parts.next()?.split(':');
    let hour: u64 = clock.next()?.parse().ok()?;
    let minute: u64 = clock.next()?.parse().ok()?;
    let second: u64 = clock.next()?.parse().ok()?;
    if parts.next()? != "GMT" || parts.next().is_some() || year < 1970 || !(1..=31).contains(&day) {
        return None;
    }
    let leap = |year: u64| {
        (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
    };
    let month_days = [
        31,
        if leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let days = (1970..year)
        .map(|year| if leap(year) { 366 } else { 365 })
        .sum::<u64>()
        + month_days[..month].iter().sum::<u64>()
        + day
        - 1;
    let seconds = ((days * 24 + hour) * 60 + minute) * 60 + second;
    SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
}

/// Statuses RFC 9110 and RFC 6585 mark as transient.
fn retried(status: u16) -> bool {
    matches!(status, 408 | 429 | 500 | 502 | 503 | 504)
}

fn rejected(shown: &str, status: u16) -> RemoteError {
    let message = if matches!(status, 401 | 403) {
        format!("{shown}: the server answered {status}; a signed URL may have expired")
    } else {
        format!("{shown}: the server answered {status}")
    };
    RemoteError::new(RemoteFailure::Rejected, message).with_status(status)
}

pub(super) fn probe_status(shown: &str, response: &RangeResponse) -> StatusClass {
    let status = response.status;
    match status {
        206 => StatusClass::Partial,
        200 => StatusClass::Failed(
            RemoteError::new(
                RemoteFailure::RangeUnsupported,
                format!("{shown}: the server answered 200 to a range request"),
            )
            .with_status(status),
        ),
        404 | 410 => StatusClass::Failed(
            RemoteError::new(
                RemoteFailure::Offline,
                format!("{shown}: the server answered {status}"),
            )
            .with_status(status),
        ),
        416 if response
            .headers
            .content_range
            .as_deref()
            .and_then(content_range)
            == Some((None, Some(0))) =>
        {
            StatusClass::Failed(
                RemoteError::new(
                    RemoteFailure::Empty,
                    format!("{shown}: the object is empty"),
                )
                .with_status(status),
            )
        }
        status if retried(status) => StatusClass::Retry(response.retry_delay()),
        status => StatusClass::Failed(rejected(shown, status)),
    }
}

pub(super) fn range_status(shown: &str, response: &RangeResponse) -> StatusClass {
    let status = response.status;
    match status {
        206 => StatusClass::Partial,
        200 | 404 | 410 | 412 | 416 => StatusClass::Failed(
            RemoteError::new(
                RemoteFailure::Changed,
                format!(
                    "{shown}: the object changed during the request (the server answered {status})"
                ),
            )
            .with_status(status),
        ),
        status if retried(status) => StatusClass::Retry(response.retry_delay()),
        status => StatusClass::Failed(rejected(shown, status)),
    }
}

fn unsupported(shown: &str, why: &str) -> RemoteError {
    RemoteError::new(RemoteFailure::RangeUnsupported, format!("{shown}: {why}")).with_status(206)
}

fn identity_body(response: &RangeResponse) -> bool {
    let identity = response
        .headers
        .content_encoding
        .as_deref()
        .is_none_or(|coding| {
            coding
                .split(',')
                .all(|coding| coding.trim().eq_ignore_ascii_case("identity"))
        });
    let single = response.headers.content_type.as_deref().is_none_or(|kind| {
        !kind
            .trim()
            .to_ascii_lowercase()
            .starts_with("multipart/byteranges")
    });
    identity && single
}

pub(super) fn probe_facts(
    shown: &str,
    response: &RangeResponse,
) -> Result<ObjectFacts, RemoteError> {
    if !identity_body(response) {
        return Err(unsupported(
            shown,
            "the server encodes the range or answers several parts",
        ));
    }
    let Some((range, total)) = response
        .headers
        .content_range
        .as_deref()
        .and_then(content_range)
    else {
        return Err(unsupported(
            shown,
            "the server's Content-Range is missing or malformed",
        ));
    };
    let Some(total) = total else {
        return Err(unsupported(shown, "the server names no length"));
    };
    if !range.is_some_and(|(first, last)| first == 0 && last < total) {
        return Err(unsupported(
            shown,
            "the server answered another range than asked",
        ));
    }
    let etag = response.headers.etag.clone().filter(|tag| is_strong(tag));
    let last_modified = response.headers.last_modified.clone();
    if etag.is_none() && last_modified.is_none() {
        return Err(unsupported(shown, "the server names no validator"));
    }
    Ok(ObjectFacts {
        total,
        etag,
        last_modified,
    })
}

pub(super) fn check_range(
    shown: &str,
    facts: &ObjectFacts,
    response: &RangeResponse,
    first: u64,
    last: u64,
) -> Result<(), RemoteError> {
    let changed = |why: &str| {
        RemoteError::new(
            RemoteFailure::Changed,
            format!("{shown}: the object changed during the request ({why})"),
        )
        .with_status(206)
    };
    if !identity_body(response) {
        return Err(changed("the range is encoded or in several parts"));
    }
    let Some((Some((start, end)), total)) = response
        .headers
        .content_range
        .as_deref()
        .and_then(content_range)
    else {
        return Err(changed("the Content-Range is missing or malformed"));
    };
    if total != Some(facts.total) {
        return Err(changed("its length differs"));
    }
    if start != first || end != last.min(facts.total.saturating_sub(1)) {
        return Err(changed("the server answered another range than asked"));
    }
    if let Some(tag) = &response.headers.etag {
        let same = facts
            .etag
            .as_deref()
            .is_some_and(|recorded| opaque_tag(recorded) == opaque_tag(tag));
        if !same && facts.etag.is_some() {
            return Err(changed("its ETag differs"));
        }
    }
    if let (Some(modified), Some(recorded)) =
        (&response.headers.last_modified, &facts.last_modified)
        && modified.trim() != recorded.trim()
    {
        return Err(changed("its Last-Modified differs"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_ranges_parse_as_rfc_9110_writes_them() {
        assert_eq!(
            content_range("bytes 0-9/100"),
            Some((Some((0, 9)), Some(100)))
        );
        assert_eq!(content_range("bytes 0-9/*"), Some((Some((0, 9)), None)));
        assert_eq!(content_range("bytes */0"), Some((None, Some(0))));
        assert_eq!(content_range("bytes 9-0/100"), None);
        assert_eq!(content_range("items 0-9/100"), None);
    }

    #[test]
    fn retry_after_reads_seconds_and_http_dates() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(784_111_777);
        assert_eq!(retry_after("120", now), Some(Duration::from_secs(120)));
        assert_eq!(
            retry_after("Sun, 06 Nov 1994 08:49:37 GMT", now),
            Some(Duration::ZERO)
        );
        assert_eq!(
            retry_after("Sun, 06 Nov 1994 08:50:37 GMT", now),
            Some(Duration::from_secs(60))
        );
        assert_eq!(retry_after("soon", now), None);
    }
}
