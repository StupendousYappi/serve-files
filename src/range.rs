// Copyright (c) 2016-2018 The http-serve developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use http::header::HeaderValue;
use std::cmp;
use std::ops::Range;
use std::str::FromStr;

/// Represents a `Range:` header which has been parsed and resolved to a particular entity length.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ResolvedRanges {
    /// No `Range:` header was supplied.
    None,

    /// A `Range:` header was supplied, but none of the ranges were possible to satisfy with the
    /// given entity length.
    NotSatisfiable,

    /// A `Range:` header was supplied with one satisfiable range.
    /// Non-satisfiable ranges have been dropped. Ranges are converted from the HTTP closed
    /// interval style to the the std::ops::Range half-open interval style (start inclusive, end
    /// exclusive).
    Single(Range<u64>),

    /// A `Range:` header was supplied with multiple satisfiable ranges. Non-satisfiable ranges
    /// have been dropped.
    Multiple(Vec<Range<u64>>),
}

/// Parses the byte-range-set in the range header as described in [RFC 7233 section
/// 2.1](https://tools.ietf.org/html/rfc7233#section-2.1).
pub(crate) fn parse(range: Option<&HeaderValue>, len: u64) -> ResolvedRanges {
    let range = match range.and_then(|v| v.to_str().ok()) {
        None => return ResolvedRanges::None,
        Some(r) => r,
    };

    // byte-ranges-specifier = bytes-unit "=" byte-range-set
    let Some(bytes) = range.strip_prefix("bytes=") else {
        return ResolvedRanges::None;
    };

    // byte-range-set  = 1#( byte-range-spec / suffix-byte-range-spec )
    let mut ranges: Vec<Range<u64>> = Vec::with_capacity(2);
    for r in bytes.split(',') {
        // Trim OWS = *( SP / HTAB )
        let r = r.trim_start_matches([' ', '\t']);

        // Parse one of the following.
        // byte-range-spec = first-byte-pos "-" [ last-byte-pos ]
        // suffix-byte-range-spec = "-" suffix-length
        let hyphen = match r.find('-') {
            None => return ResolvedRanges::None, // unparseable.
            Some(h) => h,
        };
        if hyphen == 0 {
            // It's a suffix-byte-range-spec.
            let last = match u64::from_str(&r[1..]) {
                Err(_) => return ResolvedRanges::None, // unparseable
                Ok(l) => l,
            };
            if last >= len {
                continue; // this range is not satisfiable; skip.
            }
            ranges.push((len - last)..len);
        } else {
            let first = match u64::from_str(&r[0..hyphen]) {
                Err(_) => return ResolvedRanges::None, // unparseable
                Ok(f) => f,
            };
            let end = if r.len() > hyphen + 1 {
                cmp::min(
                    match u64::from_str(&r[hyphen + 1..]) {
                        Err(_) => return ResolvedRanges::None, // unparseable
                        Ok(l) => l,
                    } + 1,
                    len,
                )
            } else {
                len // no end specified; use EOF.
            };
            if first >= end {
                continue; // this range is not satisfiable; skip.
            }
            ranges.push(first..end);
        }
    }
    match ranges.len() {
        0 => ResolvedRanges::NotSatisfiable,
        1 => ResolvedRanges::Single(ranges.remove(0)),
        _ => ResolvedRanges::Multiple(ranges),
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, ResolvedRanges};
    use http::header::HeaderValue;

    /// Tests the specific examples enumerated in [RFC 2616 section
    /// 14.35.1](https://tools.ietf.org/html/rfc2616#section-14.35.1).
    #[test]
    fn test_resolve_ranges_rfc() {
        assert_eq!(
            ResolvedRanges::Single(0..500),
            parse(Some(&HeaderValue::from_static("bytes=0-499")), 10000)
        );

        assert_eq!(
            ResolvedRanges::Single(500..1000),
            parse(Some(&HeaderValue::from_static("bytes=500-999")), 10000)
        );

        assert_eq!(
            ResolvedRanges::Single(9500..10000),
            parse(Some(&HeaderValue::from_static("bytes=-500")), 10000)
        );

        assert_eq!(
            ResolvedRanges::Single(9500..10000),
            parse(Some(&HeaderValue::from_static("bytes=9500-")), 10000)
        );

        assert_eq!(
            ResolvedRanges::Multiple(vec![0..1, 9999..10000]),
            parse(Some(&HeaderValue::from_static("bytes=0-0,-1")), 10000)
        );

        // Non-canonical ranges. Possibly the point of these is that the adjacent and overlapping
        // ranges are supposed to be coalesced into one? I'm not going to do that for now.

        assert_eq!(
            ResolvedRanges::Multiple(vec![500..601, 601..1000]),
            parse(
                Some(&HeaderValue::from_static("bytes=500-600, 601-999")),
                10000
            )
        );

        assert_eq!(
            ResolvedRanges::Multiple(vec![500..701, 601..1000]),
            parse(
                Some(&HeaderValue::from_static("bytes=500-700, 601-999")),
                10000
            )
        );
    }

    #[test]
    fn test_resolve_ranges_satisfiability() {
        assert_eq!(
            ResolvedRanges::NotSatisfiable,
            parse(Some(&HeaderValue::from_static("bytes=10000-")), 10000)
        );

        assert_eq!(
            ResolvedRanges::Single(0..500),
            parse(Some(&HeaderValue::from_static("bytes=0-499,10000-")), 10000)
        );

        assert_eq!(
            ResolvedRanges::NotSatisfiable,
            parse(Some(&HeaderValue::from_static("bytes=-1")), 0)
        );
        assert_eq!(
            ResolvedRanges::NotSatisfiable,
            parse(Some(&HeaderValue::from_static("bytes=0-0")), 0)
        );
        assert_eq!(
            ResolvedRanges::NotSatisfiable,
            parse(Some(&HeaderValue::from_static("bytes=0-")), 0)
        );

        assert_eq!(
            ResolvedRanges::Single(0..1),
            parse(Some(&HeaderValue::from_static("bytes=0-0")), 1)
        );

        assert_eq!(
            ResolvedRanges::Single(0..500),
            parse(Some(&HeaderValue::from_static("bytes=0-10000")), 500)
        );
    }

    #[test]
    fn test_resolve_ranges_absent_or_invalid() {
        assert_eq!(ResolvedRanges::None, parse(None, 10000));
    }

    #[test]
    fn test_nonascii() {
        assert_eq!(
            ResolvedRanges::None,
            parse(Some(&HeaderValue::from_bytes(b"\xff").unwrap()), 10000)
        );
    }
}
