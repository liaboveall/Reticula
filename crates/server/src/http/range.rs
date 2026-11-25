//! Which bytes of a file a request is for.

use axum::http::{HeaderMap, HeaderValue, header};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Wanted {
    /// The whole file.
    All,
    /// From the first offset to before the second.
    Part(u64, u64),
    /// Nothing that can be had.
    Unsatisfiable,
}

/// Reads `Range` and `If-Range` for a file of that length and strong ETag.
///
/// One range is served. Several, and ones that are malformed, are refused
/// rather than answered with the whole file, which can be large.
pub(super) fn wanted(headers: &HeaderMap, etag: &str, length: u64) -> Wanted {
    let mut ranges = headers.get_all(header::RANGE).iter();
    let Some(range) = ranges.next() else {
        return Wanted::All;
    };
    if headers
        .get(header::IF_RANGE)
        .is_some_and(|validator| validator != etag)
    {
        return Wanted::All;
    }
    let Some((unit, set)) = range.to_str().ok().and_then(|value| value.split_once('=')) else {
        return Wanted::Unsatisfiable;
    };
    if !unit.trim().eq_ignore_ascii_case("bytes") {
        return Wanted::All;
    }
    if ranges.next().is_some() {
        return Wanted::Unsatisfiable;
    }
    part(set.trim(), length).map_or(Wanted::Unsatisfiable, |(start, end)| {
        Wanted::Part(start, end)
    })
}

fn part(spec: &str, length: u64) -> Option<(u64, u64)> {
    let (first, last) = spec.split_once('-')?;
    if first.is_empty() {
        let count = number(last)?.min(length);
        return (count > 0).then_some((length - count, length));
    }
    let start = number(first)?;
    let end = if last.is_empty() {
        length
    } else {
        let last = number(last)?;
        if last < start {
            return None;
        }
        last.saturating_add(1).min(length)
    };
    (start < length).then_some((start, end))
}

/// A decimal number; one too large to be told apart from the largest.
fn number(digits: &str) -> Option<u64> {
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(digits.parse().unwrap_or(u64::MAX))
}

/// `Content-Range` of a part, or of a file none of which was served.
pub(super) fn content_range(part: Option<(u64, u64)>, length: u64) -> Option<HeaderValue> {
    let value = match part {
        Some((start, end)) => format!("bytes {start}-{}/{length}", end.saturating_sub(1)),
        None => format!("bytes */{length}"),
    };
    HeaderValue::from_str(&value).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ETAG: &str = "\"abc\"";

    fn read(range: &str, if_range: Option<&str>) -> Wanted {
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, range.parse().unwrap());
        if let Some(validator) = if_range {
            headers.insert(header::IF_RANGE, validator.parse().unwrap());
        }
        wanted(&headers, ETAG, 100)
    }

    #[test]
    fn one_range_in_its_three_forms() {
        for (range, part) in [
            ("bytes=0-0", (0, 1)),
            ("bytes=0-99", (0, 100)),
            ("bytes=99-99", (99, 100)),
            ("bytes=10-19", (10, 20)),
            ("bytes=10-1000", (10, 100)),
            ("bytes=10-99999999999999999999999", (10, 100)),
            ("bytes=10-", (10, 100)),
            ("bytes=-1", (99, 100)),
            ("bytes=-100", (0, 100)),
            ("bytes=-1000", (0, 100)),
            ("BYTES= 10-19 ", (10, 20)),
        ] {
            assert_eq!(read(range, None), Wanted::Part(part.0, part.1), "{range}");
        }
    }

    #[test]
    fn what_cannot_be_served() {
        for range in [
            "bytes=100-",
            "bytes=100-200",
            "bytes=99999999999999999999999-",
            "bytes=-0",
            "bytes=-",
            "bytes=",
            "bytes=5-4",
            "bytes=a-b",
            "bytes=+1-2",
            "bytes=1",
            "bytes=0-1,3-4",
            "bytes",
        ] {
            assert_eq!(read(range, None), Wanted::Unsatisfiable, "{range}");
        }
        let mut twice = HeaderMap::new();
        twice.append(header::RANGE, "bytes=0-1".parse().unwrap());
        twice.append(header::RANGE, "bytes=3-4".parse().unwrap());
        assert_eq!(wanted(&twice, ETAG, 100), Wanted::Unsatisfiable);
    }

    #[test]
    fn without_a_range_of_bytes_it_is_the_whole_file() {
        assert_eq!(wanted(&HeaderMap::new(), ETAG, 100), Wanted::All);
        assert_eq!(read("rows=0-1", None), Wanted::All);
    }

    #[test]
    fn a_range_of_another_file_is_the_whole_one() {
        assert_eq!(read("bytes=0-1", Some(ETAG)), Wanted::Part(0, 2));
        for validator in [
            "\"abd\"",
            "W/\"abc\"",
            "abc",
            "Wed, 21 Oct 2015 07:28:00 GMT",
        ] {
            assert_eq!(
                read("bytes=0-1", Some(validator)),
                Wanted::All,
                "{validator}"
            );
        }
        // Without a range there is nothing for it to apply to.
        let mut headers = HeaderMap::new();
        headers.insert(header::IF_RANGE, "\"abd\"".parse().unwrap());
        assert_eq!(wanted(&headers, ETAG, 100), Wanted::All);
    }

    #[test]
    fn content_ranges() {
        assert_eq!(
            content_range(Some((10, 20)), 100).unwrap(),
            "bytes 10-19/100"
        );
        assert_eq!(content_range(None, 100).unwrap(), "bytes */100");
    }
}
