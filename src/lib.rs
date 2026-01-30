// Copyright (c) 2016-2021 The http-serve developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Helpers for serving HTTP GET and HEAD responses asynchronously with the
//! [http](http://crates.io/crates/http) crate and [tokio](https://crates.io/crates/tokio).
//! Works well with [hyper](https://crates.io/crates/hyper) 1.x.
//!
//! This crate supplies a way to respond to HTTP GET and HEAD requests:
//!
//! *   the `serve` function can be used to serve an `Entity`, a trait representing reusable,
//!     byte-rangeable HTTP entities. `Entity` must be able to produce exactly the same data on
//!     every call, know its size in advance, and be able to produce portions of the data on demand.
//!
//! It supplies a static file `Entity` implementation and a (currently Unix-only)
//! helper for serving a full directory tree from the local filesystem, including
//! automatically looking for `.gz`-suffixed files when the client advertises
//! `Accept-Encoding: gzip`.
//!
//! # Why the weird type bounds? Why not use `hyper::Body` and `BoxError`?
//!
//! These bounds are compatible with `bytes::Bytes` and `BoxError`, and most callers will use
//! those types.
//!
//! There are times when it's desirable to have more flexible ownership provided by a
//! type such as `reffers::ARefs<'static, [u8]>`. One is `mmap`-based file serving:
//! `bytes::Bytes` would require copying the data in each chunk. An implementation with `ARefs`
//! could instead `mmap` and `mlock` the data on another thread and provide chunks which `munmap`
//! when dropped. In these cases, the caller can supply an alternate implementation of the
//! `http_body::Body` trait which uses a different `Data` type than `bytes::Bytes`.

#![deny(missing_docs, clippy::print_stderr, clippy::print_stdout)]
#![cfg_attr(docsrs, feature(doc_cfg))]

use bytes::Buf;
use futures_core::Stream;
use http::header::{self, HeaderMap, HeaderValue};
use std::ops::Range;
use std::pin::Pin;
use std::str::FromStr;
use std::time::SystemTime;
#[cfg(test)]
use {
    bytes::{Bytes, BytesMut},
    futures_util::StreamExt,
};

/// Returns a HeaderValue for the given formatted data.
/// Caller must make two guarantees:
///    * The data fits within `max_len` (or the write will panic).
///    * The data are ASCII (or HeaderValue's safety will be violated).
macro_rules! unsafe_fmt_ascii_val {
    ($max_len:expr, $fmt:expr, $($arg:tt)+) => {{
        let mut buf = bytes::BytesMut::with_capacity($max_len);
        use std::fmt::Write;
        write!(buf, $fmt, $($arg)*).expect("fmt_val fits within provided max len");
        unsafe {
            http::header::HeaderValue::from_maybe_shared_unchecked(buf.freeze())
        }
    }}
}

fn as_u64(len: usize) -> u64 {
    const {
        assert!(std::mem::size_of::<usize>() <= std::mem::size_of::<u64>());
    };
    len as u64
}

/// A type-erased error.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

mod body;

pub mod dir;
pub mod served_dir;

mod etag;
mod file;
mod platform;
mod range;
mod serving;

pub use crate::body::Body;
pub use crate::file::FileEntity;
pub use crate::serving::serve;

/// A reusable, read-only, byte-rangeable HTTP entity for GET and HEAD serving.
/// Must return exactly the same data on every call.
pub trait Entity: 'static + Send + Sync {
    /// The type of errors produced in [`Self::get_range`] chunks and in the final stream.
    ///
    /// [`BoxError`] is a good choice for most implementations.
    ///
    /// This must be convertable from [`BoxError`] to allow `serve_files` to
    /// inject errors.
    ///
    /// Note that errors returned directly from the body to `hyper` just drop
    /// the stream abruptly without being logged. Callers might use an
    /// intermediary service for better observability.
    type Error: 'static + From<BoxError>;

    /// The type of a data chunk.
    ///
    /// Commonly `bytes::Bytes` but may be something more exotic.
    type Data: 'static + Buf + From<Vec<u8>> + From<&'static [u8]>;

    /// Returns the length of the entity's body in bytes.
    fn len(&self) -> u64;

    /// Returns true iff the entity's body has length 0.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Gets the body bytes indicated by `range`.
    ///
    /// The stream must return exactly `range.end - range.start` bytes or fail early with an `Err`.
    #[allow(clippy::type_complexity)]
    fn get_range(
        &self,
        range: Range<u64>,
    ) -> Pin<Box<dyn Stream<Item = Result<Self::Data, Self::Error>> + Send + Sync>>;

    /// Adds entity headers such as `Content-Type` to the supplied `HeaderMap`.
    /// In particular, these headers are the "other representation header fields" described by [RFC
    /// 7233 section 4.1](https://tools.ietf.org/html/rfc7233#section-4.1); they should exclude
    /// `Content-Range`, `Date`, `Cache-Control`, `ETag`, `Expires`, `Content-Location`, and `Vary`.
    ///
    /// This function will be called only when that section says that headers such as
    /// `Content-Type` should be included in the response.
    fn add_headers(&self, _: &mut HeaderMap);

    /// Returns an etag for this entity, if available.
    /// Implementations are encouraged to provide a strong etag. [RFC 7232 section
    /// 2.1](https://tools.ietf.org/html/rfc7232#section-2.1) notes that only strong etags
    /// are usable for sub-range retrieval.
    fn etag(&self) -> Option<HeaderValue>;

    /// Returns the last modified time of this entity, if available.
    /// Note that `serve` may serve an earlier `Last-Modified:` date than the one returned here if
    /// this time is in the future, as required by [RFC 7232 section
    /// 2.2.1](https://tools.ietf.org/html/rfc7232#section-2.2.1).
    fn last_modified(&self) -> Option<SystemTime>;

    /// Reads the entire body of this entity into a `Bytes`.
    ///
    /// This is a convenience method for testing and debugging.
    #[cfg(test)]
    async fn read_body(&self) -> Result<Bytes, Self::Error>
    where
        Self: Sized,
    {
        let chunks = self.get_range(0..self.len()).collect::<Vec<_>>().await;
        let mut bytes = BytesMut::new();
        for chunk in chunks {
            match chunk {
                Ok(chunk) => bytes.extend_from_slice(chunk.chunk()),
                Err(err) => return Err(err),
            }
        }
        Ok(bytes.freeze())
    }
}

/// Parses an RFC 7231 section 5.3.1 `qvalue` into an integer in [0, 1000].
/// ```text
/// qvalue = ( "0" [ "." 0*3DIGIT ] )
///        / ( "1" [ "." 0*3("0") ] )
/// ```
fn parse_qvalue(s: &str) -> Result<u16, ()> {
    match s {
        "1" | "1." | "1.0" | "1.00" | "1.000" => return Ok(1000),
        "0" | "0." => return Ok(0),
        s if !s.starts_with("0.") => return Err(()),
        _ => {}
    };
    let v = &s[2..];
    let factor = match v.len() {
        1 /* 0.x */ => 100,
        2 /* 0.xx */ => 10,
        3 /* 0.xxx */ => 1,
        _ => return Err(()),
    };
    let v = u16::from_str(v).map_err(|_| ())?;
    let q = v * factor;
    Ok(q)
}

/// The compression styles supported by the client of a request.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum CompressionSupport {
    /// Use gzip compression.
    Gzip,
    /// Use brotli compression
    Brotli,
    /// Use brotli if available, otherwise gzip
    Both,
    /// Do not use compression.
    None,
}

impl CompressionSupport {
    /// Returns true if Brotli compression is supported.
    pub fn brotli(&self) -> bool {
        matches!(self, CompressionSupport::Brotli | CompressionSupport::Both)
    }

    /// Returns true if Gzip compression is supported.
    pub fn gzip(&self) -> bool {
        matches!(self, CompressionSupport::Gzip | CompressionSupport::Both)
    }
}

/// Returns the preferred compression to use when responding to the given request, if any.
///
/// Use via `detect_compression_support(req.headers())`.
///
/// Follows the rules of [RFC 7231 section
/// 5.3.4](https://tools.ietf.org/html/rfc7231#section-5.3.4).
///
/// Note that if both gzip and brotli are supported, brotli will be preferred by the server.
pub fn detect_compression_support(headers: &HeaderMap) -> CompressionSupport {
    let v = match headers.get(header::ACCEPT_ENCODING) {
        None => return CompressionSupport::None,
        Some(v) if v.is_empty() => return CompressionSupport::None,
        Some(v) => v,
    };
    let (mut gzip_q, mut br_q, mut identity_q, mut star_q) = (None, None, None, None);
    let parts = match v.to_str() {
        Ok(s) => s.split(','),
        Err(_) => return CompressionSupport::None,
    };
    for qi in parts {
        // Parse.
        let coding;
        let quality;
        match qi.split_once(';') {
            None => {
                coding = qi.trim();
                quality = 1000;
            }
            Some((c, q)) => {
                coding = c.trim();
                let Some(q) = q
                    .trim()
                    .strip_prefix("q=")
                    .and_then(|q| parse_qvalue(q).ok())
                else {
                    return CompressionSupport::None; // unparseable.
                };
                quality = q;
            }
        };

        if coding == "gzip" {
            gzip_q = Some(quality);
        } else if coding == "br" {
            br_q = Some(quality);
        } else if coding == "identity" {
            identity_q = Some(quality);
        } else if coding == "*" {
            star_q = Some(quality);
        }
    }

    let gzip_q = gzip_q.or(star_q).unwrap_or(0);
    let br_q = br_q.or(star_q).unwrap_or(0);

    // "If the representation has no content-coding, then it is
    // acceptable by default unless specifically excluded by the
    // Accept-Encoding field stating either "identity;q=0" or "*;q=0"
    // without a more specific entry for "identity"."
    let identity_q = identity_q.or(star_q).unwrap_or(0);

    // The server will always have identity coding available, so if it's
    // higher priority than another coding, there's no need to enable
    // the other coding. According to the RFC, it's possible for a client
    // to support other codings while not supporting identity coding, but
    // that seems very unlikely in practice and we don't support that.
    let use_gzip = gzip_q > 0 && gzip_q >= identity_q;
    let use_br = br_q > 0 && br_q >= identity_q;

    match (use_gzip, use_br) {
        (true, false) => CompressionSupport::Gzip,
        (false, true) => CompressionSupport::Brotli,
        (true, true) => CompressionSupport::Both,
        (false, false) => CompressionSupport::None,
    }
}

// streaming_body and related types removed.

#[cfg(test)]
mod tests {
    use http::header::HeaderValue;
    use http::{self, header};
    use pretty_assertions::assert_matches;

    fn ae_hdrs(value: &'static str) -> http::HeaderMap {
        let mut h = http::HeaderMap::new();
        h.insert(header::ACCEPT_ENCODING, HeaderValue::from_static(value));
        h
    }

    #[test]
    fn parse_qvalue() {
        use super::parse_qvalue;
        assert_eq!(parse_qvalue("0"), Ok(0));
        assert_eq!(parse_qvalue("0."), Ok(0));
        assert_eq!(parse_qvalue("0.0"), Ok(0));
        assert_eq!(parse_qvalue("0.00"), Ok(0));
        assert_eq!(parse_qvalue("0.000"), Ok(0));
        assert_eq!(parse_qvalue("0.0000"), Err(()));
        assert_eq!(parse_qvalue("0.2"), Ok(200));
        assert_eq!(parse_qvalue("0.23"), Ok(230));
        assert_eq!(parse_qvalue("0.234"), Ok(234));
        assert_eq!(parse_qvalue("1"), Ok(1000));
        assert_eq!(parse_qvalue("1."), Ok(1000));
        assert_eq!(parse_qvalue("1.0"), Ok(1000));
        assert_eq!(parse_qvalue("1.1"), Err(()));
        assert_eq!(parse_qvalue("1.00"), Ok(1000));
        assert_eq!(parse_qvalue("1.000"), Ok(1000));
        assert_eq!(parse_qvalue("1.001"), Err(()));
        assert_eq!(parse_qvalue("1.0000"), Err(()));
        assert_eq!(parse_qvalue("2"), Err(()));
    }

    #[test]
    fn detect_compression_support() {
        use super::CompressionSupport;
        // "A request without an Accept-Encoding header field implies that the
        // user agent has no preferences regarding content-codings. Although
        // this allows the server to use any content-coding in a response, it
        // does not imply that the user agent will be able to correctly process
        // all encodings." Identity seems safer; don't compress.
        assert!(matches!(
            super::detect_compression_support(&header::HeaderMap::new()),
            CompressionSupport::None
        ));

        // "If the representation's content-coding is one of the
        // content-codings listed in the Accept-Encoding field, then it is
        // acceptable unless it is accompanied by a qvalue of 0.  (As
        // defined in Section 5.3.1, a qvalue of 0 means "not acceptable".)"
        assert_matches!(
            super::detect_compression_support(&ae_hdrs("gzip")),
            CompressionSupport::Gzip
        );
        assert_matches!(
            super::detect_compression_support(&ae_hdrs("gzip;q=0.001")),
            CompressionSupport::Gzip
        );
        assert_matches!(
            super::detect_compression_support(&ae_hdrs("br;q=0.001")),
            CompressionSupport::Brotli
        );
        assert_matches!(
            super::detect_compression_support(&ae_hdrs("br, gzip")),
            CompressionSupport::Both
        );

        assert_matches!(
            super::detect_compression_support(&ae_hdrs("gzip;q=0")),
            CompressionSupport::None
        );

        // "An Accept-Encoding header field with a combined field-value that is
        // empty implies that the user agent does not want any content-coding in
        // response."
        assert_matches!(
            super::detect_compression_support(&ae_hdrs("")),
            CompressionSupport::None
        );

        // The asterisk "*" symbol matches any available content-coding not
        // explicitly listed. identity is a content-coding.
        // If * is q=1000, then identity_q=1000, and neither gzip nor br is
        // strictly greater than 1000.
        assert_matches!(
            super::detect_compression_support(&ae_hdrs("*")),
            CompressionSupport::Both
        );
        assert_matches!(
            super::detect_compression_support(&ae_hdrs("gzip;q=0, *")),
            CompressionSupport::Brotli
        );
        assert_matches!(
            super::detect_compression_support(&ae_hdrs("identity;q=0, *")),
            CompressionSupport::Both
        );

        // "If multiple content-codings are acceptable, then the acceptable
        // content-coding with the highest non-zero qvalue is preferred."
        assert_matches!(
            super::detect_compression_support(&ae_hdrs("identity;q=0.5, gzip;q=1.0")),
            CompressionSupport::Gzip
        );
        assert_matches!(
            super::detect_compression_support(&ae_hdrs("identity;q=1.0, gzip;q=0.5")),
            CompressionSupport::None
        );
        assert_matches!(
            super::detect_compression_support(&ae_hdrs("br;q=1.0, gzip;q=0.5, identity;q=0.1")),
            CompressionSupport::Both
        );
        assert_matches!(
            super::detect_compression_support(&ae_hdrs("br;q=0.5, gzip;q=1.0, identity;q=0.1")),
            CompressionSupport::Both
        );

        // "If an Accept-Encoding header field is present in a request
        // and none of the available representations for the response have a
        // content-coding that is listed as acceptable, the origin server SHOULD
        // send a response without any content-coding."
        assert_matches!(
            super::detect_compression_support(&ae_hdrs("*;q=0")),
            CompressionSupport::None
        );
        assert_matches!(
            super::detect_compression_support(&ae_hdrs("gzip;q=0.002")), // q=2
            CompressionSupport::Gzip
        );
    }
}
