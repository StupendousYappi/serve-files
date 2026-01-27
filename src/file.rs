// Copyright (c) 2020 The http-serve developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use crate::platform::{self, FileExt, FileInfo};
use bytes::Buf;
use fixed_cache::{static_cache, Cache};
use futures_core::Stream;
use futures_util::stream;
use http::header::{HeaderMap, HeaderValue};
use std::error::Error as StdError;
use std::io;
use std::ops::Range;
use std::pin::Pin;
use std::sync::Arc;
use std::time::SystemTime;

use crate::Entity;

// This stream breaks apart the file into chunks of at most CHUNK_SIZE. This size is
// a tradeoff between memory usage and thread handoffs.
static CHUNK_SIZE: u64 = 65_536;

/// HTTP entity created from a [`std::fs::File`] which reads the file chunk-by-chunk within
/// a [`tokio::task::block_in_place`] closure.
///
/// `ChunkedReadFile` is cheap to clone and reuse for many requests.
///
/// Expects to be served from a tokio threadpool.
///
/// ```
/// # use bytes::Bytes;
/// # use http::{Request, Response, header::{self, HeaderMap, HeaderValue}};
/// type BoxError = Box<dyn std::error::Error + Send + Sync>;
/// async fn serve_dictionary(req: Request<hyper::body::Incoming>) -> Result<Response<http_serve::Body>, BoxError> {
///     let f = tokio::task::block_in_place::<_, Result<_, BoxError>>(
///         move || {
///             let f = std::fs::File::open("/usr/share/dict/words")?;
///             let mut headers = http::header::HeaderMap::new();
///             headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
///             Ok(http_serve::ChunkedReadFile::new(f, headers)?)
///         },
///     )?;
///     Ok(http_serve::serve(f, &req))
/// }
/// ```
#[derive(Clone)]
pub struct ChunkedReadFile<
    D: 'static + Send + Buf + From<Vec<u8>> + From<&'static [u8]>,
    E: 'static + Send + Into<Box<dyn StdError + Send + Sync>> + From<Box<dyn StdError + Send + Sync>>,
> {
    inner: Arc<ChunkedReadFileInner>,
    phantom: std::marker::PhantomData<(D, E)>,
}

struct ChunkedReadFileInner {
    len: u64,
    mtime: SystemTime,
    f: std::fs::File,
    headers: HeaderMap,
    etag: ETag,
}

impl<D, E> ChunkedReadFile<D, E>
where
    D: 'static + Send + Sync + Buf + From<Vec<u8>> + From<&'static [u8]>,
    E: 'static
        + Send
        + Sync
        + Into<Box<dyn StdError + Send + Sync>>
        + From<Box<dyn StdError + Send + Sync>>,
{
    /// Creates a new ChunkedReadFile.
    ///
    /// `read(2)` calls will be wrapped in [`tokio::task::block_in_place`] calls so that they don't
    /// block the tokio reactor thread on local disk I/O. Note that [`std::fs::File::open`] and
    /// this constructor (specifically, its call to `fstat(2)`) may also block, so they typically
    /// should be wrapped in [`tokio::task::block_in_place`] as well.
    pub fn new(file: std::fs::File, headers: HeaderMap) -> Result<Self, io::Error> {
        let m = file.metadata()?;
        ChunkedReadFile::new_with_metadata(file, &m, headers)
    }

    /// Creates a new ChunkedReadFile, with presupplied metadata.
    ///
    /// This is an optimization for the case where the caller has already called `fstat(2)`.
    /// Note that on Windows, this still may perform a blocking file operation, so it should
    /// still be wrapped in [`tokio::task::block_in_place`].
    pub fn new_with_metadata(
        file: ::std::fs::File,
        metadata: &::std::fs::Metadata,
        headers: HeaderMap,
    ) -> Result<Self, io::Error> {
        // `file` might represent a directory. If so, it's better to realize that now (while
        // we can still send a proper HTTP error) rather than during `get_range` (when all we can
        // do is drop the HTTP connection).
        if !metadata.is_file() {
            return Err(io::Error::new(io::ErrorKind::Other, "expected a file"));
        }

        let info = platform::file_info(&file, metadata)?;
        let etag: ETag = ETAG_CACHE.get_or_try_insert_with(info, |_info| ETag::from_file(&file))?;

        Ok(ChunkedReadFile {
            inner: Arc::new(ChunkedReadFileInner {
                len: info.len,
                mtime: info.mtime,
                headers,
                f: file,
                etag,
            }),
            phantom: std::marker::PhantomData,
        })
    }
}

impl<D, E> Entity for ChunkedReadFile<D, E>
where
    D: 'static + Send + Sync + Buf + From<Vec<u8>> + From<&'static [u8]>,
    E: 'static
        + Send
        + Sync
        + Into<Box<dyn StdError + Send + Sync>>
        + From<Box<dyn StdError + Send + Sync>>,
{
    type Data = D;
    type Error = E;

    fn len(&self) -> u64 {
        self.inner.len
    }

    fn get_range(
        &self,
        range: Range<u64>,
    ) -> Pin<Box<dyn Stream<Item = Result<Self::Data, Self::Error>> + Send + Sync>> {
        let stream = stream::unfold(
            (range, Arc::clone(&self.inner)),
            move |(left, inner)| async {
                if left.start == left.end {
                    return None;
                }
                let chunk_size = std::cmp::min(CHUNK_SIZE, left.end - left.start) as usize;
                Some(tokio::task::block_in_place(move || {
                    match inner.f.read_range(chunk_size, left.start) {
                        Err(e) => (
                            Err(Box::<dyn StdError + Send + Sync + 'static>::from(e).into()),
                            (left, inner),
                        ),
                        Ok(v) => {
                            let bytes_read = v.len();
                            (
                                Ok(v.into()),
                                (left.start + bytes_read as u64..left.end, inner),
                            )
                        }
                    }
                }))
            },
        );
        let _: &dyn Stream<Item = Result<Self::Data, Self::Error>> = &stream;
        Box::pin(stream)
    }

    fn add_headers(&self, h: &mut HeaderMap) {
        h.extend(
            self.inner
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone())),
        );
    }

    fn etag(&self) -> Option<HeaderValue> {
        Some(self.inner.etag.into())
    }

    fn last_modified(&self) -> Option<SystemTime> {
        Some(self.inner.mtime)
    }
}

#[derive(Hash, Eq, PartialEq, Clone, Copy)]
struct ETag(u64);

impl ETag {
    fn from_file(file: &std::fs::File) -> Result<Self, io::Error> {
        tokio::task::block_in_place(move || {
            let hash = rapidhash::v3::rapidhash_v3_file(file)?;
            Ok(ETag(hash))
        })
    }

    fn to_bytes(self) -> [u8; 16] {
        let mut buf = [0u8; 16];
        let hex_chars = b"0123456789abcdef";
        let val = self.0;
        for i in 0..16 {
            let nibble = (val >> ((15 - i) * 4)) & 0xf;
            buf[i] = hex_chars[nibble as usize];
        }
        buf
    }
}

impl From<ETag> for HeaderValue {
    fn from(etag: ETag) -> Self {
        let mut buf = [0u8; 18];
        buf[0] = b'"';
        buf[17] = b'"';
        let bytes = etag.to_bytes();
        buf[1..17].copy_from_slice(&bytes);
        HeaderValue::from_bytes(&buf).expect("failed to serialize etag")
    }
}

type BuildHasher = std::hash::BuildHasherDefault<rapidhash::fast::RapidHasher<'static>>;

const CACHE_SIZE: usize = 512;

static ETAG_CACHE: Cache<FileInfo, ETag, BuildHasher> =
    static_cache!(FileInfo, ETag, CACHE_SIZE, BuildHasher::new());

#[cfg(test)]
mod tests {
    use super::ChunkedReadFile;
    use super::Entity;
    use bytes::Bytes;
    use futures_core::Stream;
    use futures_util::stream::TryStreamExt;
    use http::header::HeaderMap;
    use std::fs::File;
    use std::io::{Seek, SeekFrom, Write};
    use std::pin::Pin;
    use std::time::Duration;
    use std::time::SystemTime;

    type BoxError = Box<dyn std::error::Error + Sync + Send>;
    type CRF = ChunkedReadFile<Bytes, BoxError>;

    async fn to_bytes(
        s: Pin<Box<dyn Stream<Item = Result<Bytes, BoxError>> + Send>>,
    ) -> Result<Bytes, BoxError> {
        let concat = Pin::from(s)
            .try_fold(Vec::new(), |mut acc, item| async move {
                acc.extend(&item[..]);
                Ok(acc)
            })
            .await?;
        Ok(concat.into())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn basic() {
        tokio::spawn(async move {
            let tmp = tempfile::tempdir().unwrap();
            let p = tmp.path().join("f");
            let mut f = File::create(&p).unwrap();
            f.write_all(b"asdf").unwrap();

            let crf1 = CRF::new(File::open(&p).unwrap(), HeaderMap::new()).unwrap();
            assert_eq!(4, crf1.len());

            // Test returning part/all of the stream.
            assert_eq!(
                &to_bytes(crf1.get_range(0..4)).await.unwrap().as_ref(),
                b"asdf"
            );
            assert_eq!(
                &to_bytes(crf1.get_range(1..3)).await.unwrap().as_ref(),
                b"sd"
            );

            // A ChunkedReadFile constructed from a modified file should have a different etag.
            f.write_all(b"jkl;").unwrap();
            let crf2 = CRF::new(File::open(&p).unwrap(), HeaderMap::new()).unwrap();
            assert_eq!(8, crf2.len());
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn etag() {
        tokio::spawn(async move {
            let tmp = tempfile::tempdir().unwrap();
            let p = tmp.path().join("f");
            let mut f = File::create(&p).unwrap();

            f.write_all(b"first value").unwrap();
            let crf1 = CRF::new(File::open(&p).unwrap(), HeaderMap::new()).unwrap();
            let etag1 = crf1.etag().expect("etag1 was None");
            assert_eq!(r#""928c5c44c1689e3f""#, etag1.to_str().unwrap());

            f.seek(SeekFrom::Start(0)).unwrap();
            f.set_len(0).unwrap();

            f.write_all(b"another value").unwrap();
            let crf2 = CRF::new(File::open(&p).unwrap(), HeaderMap::new()).unwrap();
            let etag2 = crf2.etag().expect("etag2 was None");
            assert_eq!(r#""d712812bea51c2cf""#, etag2.to_str().unwrap());

            assert_eq!(
                Some(etag1),
                crf1.etag(),
                "CRF etag changed after file modification (should be immutable)"
            );
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn last_modified() {
        tokio::spawn(async move {
            let tmp = tempfile::tempdir().unwrap();
            let p = tmp.path().join("f");
            let mut f = File::create(&p).unwrap();
            f.write_all(b"blahblah").unwrap();

            let crf1 = CRF::new(File::open(&p).unwrap(), HeaderMap::new()).unwrap();
            let expected = f.metadata().unwrap().modified().ok();
            assert_eq!(expected, crf1.last_modified());

            let t = SystemTime::UNIX_EPOCH + Duration::from_hours(50);
            f.set_modified(t).unwrap();

            let crf2 = CRF::new(File::open(&p).unwrap(), HeaderMap::new()).unwrap();
            assert_eq!(Some(t), crf2.last_modified());

            assert_eq!(
                expected,
                crf1.last_modified(),
                "CRF last_modified value changed after file modification (should be immutable)"
            );
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn truncate_race() {
        tokio::spawn(async move {
            let tmp = tempfile::tempdir().unwrap();
            let p = tmp.path().join("f");
            let mut f = File::create(&p).unwrap();
            f.write_all(b"asdf").unwrap();

            let crf = CRF::new(File::open(&p).unwrap(), HeaderMap::new()).unwrap();
            assert_eq!(4, crf.len());
            f.set_len(3).unwrap();

            // Test that
            let e = to_bytes(crf.get_range(0..4)).await.unwrap_err();
            let e = e.downcast::<std::io::Error>().unwrap();
            assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof);
        })
        .await
        .unwrap();
    }
}
