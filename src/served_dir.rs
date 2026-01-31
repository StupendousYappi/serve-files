//! Serves files from a local directory.

use std::{
    collections::HashMap,
    error::Error,
    fmt::Display,
    fs::File,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use bytes::Bytes;
use http::{header, HeaderMap, HeaderValue};
use std::io::Error as IOError;

use crate::{BoxError, FileEntity};

/// Returns `FileEntity` values for file paths within a directory.
pub struct ServedDir {
    /// Provides servable `FileEntity` values for file paths within a directory.
    auto_compress: bool,
    dirpath: PathBuf,
    strip_prefix: Option<String>,
    known_extensions: Option<HashMap<String, HeaderValue>>,
    default_content_type: HeaderValue,
    common_headers: HeaderMap,
}

static OCTET_STREAM: HeaderValue = HeaderValue::from_static("application/octet-stream");

impl ServedDir {
    /// Returns a builder for `ServedDir`.
    pub fn builder(path: impl Into<PathBuf>) -> ServedDirBuilder {
        ServedDirBuilder::new(path.into())
    }

    /// Returns a `FileEntity` for the given path and request headers.
    ///
    /// The `Accept-Encoding` request header is used to determine whether to serve the file
    /// gzipped if possible. If gzip encoding is requested, and `auto_gzip` is enabled, this
    /// method will look for a file with the same name but a `.gz` extension. If found, it will
    /// serve that file instead of the primary file. If gzip encoding is not requested,
    /// `auto_gzip` is disabled, or the file is not found, the primary file will be served.
    ///
    /// This method will return an error with kind `ErrorKind::NotFound` if the file is not found.
    /// It will return an error with kind `ErrorKind::InvalidInput` if the path is invalid.
    pub async fn get(
        &self,
        path: &str,
        req_hdrs: &HeaderMap,
    ) -> Result<FileEntity<Bytes, BoxError>, IOError> {
        let path = match self.strip_prefix.as_deref() {
            Some(prefix) => path.strip_prefix(prefix).ok_or_else(|| {
                IOError::new(ErrorKind::NotFound, "invalid path, prefix not found")
            })?,
            None => path,
        };

        if let Err(e) = self.validate_path(path) {
            return Err(IOError::new(ErrorKind::InvalidInput, e));
        }

        let full_path = self.dirpath.join(path);
        let preferred = if self.auto_compress {
            crate::detect_compression_support(req_hdrs)
        } else {
            crate::CompressionSupport::None
        };
        let auto_compress = self.auto_compress;
        let content_type: HeaderValue = self.get_content_type(&full_path);

        let node: Node = tokio::task::spawn_blocking(move || -> Result<Node, IOError> {
            let mut node = Self::find_file(&full_path, preferred)?
                .ok_or_else(|| IOError::new(ErrorKind::NotFound, "file not found"))?;
            node.auto_compress = auto_compress;
            Ok(node)
        })
        .await
        .unwrap_or_else(|e: tokio::task::JoinError| Err(e.into()))?;

        let mut headers = self.common_headers.clone();
        headers.insert(http::header::CONTENT_TYPE, content_type);
        let e = node.into_file_entity(headers)?;
        Ok(e)
    }

    fn get_content_type(&self, path: &Path) -> HeaderValue {
        let extension = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or_default();

        self.known_extensions
            .as_ref()
            .and_then(|exts| exts.get(extension).cloned())
            .or_else(|| Self::guess_content_type(extension))
            .unwrap_or_else(|| self.default_content_type.clone())
    }

    #[cfg(feature = "mime_guess")]
    fn guess_content_type(ext: &str) -> Option<HeaderValue> {
        mime_guess::from_ext(ext)
            .first_raw()
            .map(|s| HeaderValue::from_str(s).unwrap())
    }

    #[cfg(not(feature = "mime_guess"))]
    fn guess_content_type(ext: &str) -> Option<HeaderValue> {
        let guess = match ext {
            "html" => Some("text/html"),
            "htm" => Some("text/html"),
            "hxt" => Some("text/html"),
            "css" => Some("text/css"),
            "js" => Some("application/javascript"),
            "es" => Some("application/javascript"),
            "ecma" => Some("application/javascript"),
            "jsm" => Some("application/javascript"),
            "jsx" => Some("application/javascript"),
            "png" => Some("image/png"),
            "apng" => Some("image/apng"),
            "avif" => Some("image/avif"),
            "gif" => Some("image/gif"),
            "ico" => Some("image/x-icon"),
            "jpeg" => Some("image/jpeg"),
            "jfif" => Some("image/jpeg"),
            "pjpeg" => Some("image/jpeg"),
            "pjp" => Some("image/jpeg"),
            "jpg" => Some("image/jpeg"),
            "svg" => Some("image/svg+xml"),
            "tiff" => Some("image/tiff"),
            "webp" => Some("image/webp"),
            "bmp" => Some("image/bmp"),
            "pdf" => Some("application/pdf"),
            "zip" => Some("application/zip"),
            "gz" => Some("application/gzip"),
            "tar" => Some("application/tar"),
            "bz" => Some("application/x-bzip"),
            "bz2" => Some("application/x-bzip2"),
            "xz" => Some("application/x-xz"),
            "csv" => Some("text/csv"),
            "txt" => Some("text/plain"),
            "text" => Some("text/plain"),
            "log" => Some("text/plain"),
            "md" => Some("text/markdown"),
            "markdown" => Some("text/x-markdown"),
            "mkd" => Some("text/x-markdown"),
            "mp4" => Some("video/mp4"),
            "webm" => Some("video/webm"),
            "mpeg" => Some("video/mpeg"),
            "mpg" => Some("video/mpeg"),
            "mpg4" => Some("video/mp4"),
            "xml" => Some("application/xml"),
            "json" => Some("application/json"),
            "yaml" => Some("application/yaml"),
            "yml" => Some("application/yaml"),
            "toml" => Some("application/toml"),
            "ini" => Some("application/ini"),
            "ics" => Some("text/calendar"),
            "doc" => Some("application/msword"),
            "docx" => {
                Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document")
            }
            "xls" => Some("application/vnd.ms-excel"),
            "xlsx" => Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
            "ppt" => Some("application/vnd.ms-powerpoint"),
            "pptx" => {
                Some("application/vnd.openxmlformats-officedocument.presentationml.presentation")
            }
            _ => None,
        };
        guess.map(|s| HeaderValue::from_str(s).unwrap())
    }

    fn find_file(
        path: &Path,
        supported: crate::CompressionSupport,
    ) -> Result<Option<Node>, IOError> {
        let try_path = |p: &Path, encoding: ContentEncoding| -> Result<Option<Node>, IOError> {
            match File::open(p) {
                Ok(file) => {
                    let metadata = file.metadata()?;
                    // TODO: simplify this, I think we want to fail with NotAFile earlier, and can trust that path is a file
                    // if we get here
                    if metadata.is_file()
                        || (matches!(encoding, ContentEncoding::Identity) && metadata.is_dir())
                    {
                        return Ok(Some(Node {
                            path: p.to_path_buf(),
                            metadata,
                            auto_compress: false, // will be set by caller
                            content_encoding: encoding,
                        }));
                    }
                }
                Err(ref e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            Ok(None)
        };

        if supported.brotli() {
            let mut br_path = path.to_path_buf();
            if let Some(name) = path.file_name() {
                let mut new_name = name.to_os_string();
                new_name.push(".br");
                br_path.set_file_name(new_name);
                if let Some(node) = try_path(&br_path, ContentEncoding::Brotli)? {
                    return Ok(Some(node));
                }
            }
        }

        if supported.gzip() {
            let mut gz_path = path.to_path_buf();
            if let Some(name) = path.file_name() {
                let mut new_name = name.to_os_string();
                new_name.push(".gz");
                gz_path.set_file_name(new_name);
                if let Some(node) = try_path(&gz_path, ContentEncoding::Gzip)? {
                    return Ok(Some(node));
                }
            }
        }

        try_path(path, ContentEncoding::Identity)
    }

    /// Ensures path is safe: no NUL bytes, not absolute, no `..` segments.
    fn validate_path(&self, path: &str) -> Result<(), &'static str> {
        if memchr::memchr(0, path.as_bytes()).is_some() {
            return Err("path contains NUL byte");
        }
        if path.as_bytes().first() == Some(&b'/') {
            return Err("path is absolute");
        }
        let mut left = path.as_bytes();
        loop {
            let next = memchr::memchr(b'/', left);
            let seg = &left[0..next.unwrap_or(left.len())];
            if seg == b".." {
                return Err("path contains .. segment");
            }
            match next {
                None => break,
                Some(n) => left = &left[n + 1..],
            };
        }
        Ok(())
    }
}

/// A builder for [`ServedDir`].
pub struct ServedDirBuilder {
    dirpath: PathBuf,
    auto_compress: bool,
    strip_prefix: Option<String>,
    known_extensions: Option<HashMap<String, HeaderValue>>,
    default_content_type: HeaderValue,
    common_headers: HeaderMap,
}

impl ServedDirBuilder {
    fn new(dirpath: PathBuf) -> Self {
        Self {
            dirpath,
            auto_compress: true,
            strip_prefix: None,
            known_extensions: None,
            default_content_type: OCTET_STREAM.clone(),
            common_headers: HeaderMap::new(),
        }
    }

    /// Sets whether to automatically serve compressed versions of files.
    /// Defaults to `true`.
    pub fn auto_compress(mut self, auto_compress: bool) -> Self {
        self.auto_compress = auto_compress;
        self
    }

    /// Sets a prefix to strip from the request path.
    pub fn strip_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.strip_prefix = Some(prefix.into());
        self
    }

    /// Sets a map of file extensions to content types.
    pub fn known_extensions(mut self, extensions: HashMap<String, HeaderValue>) -> Self {
        self.known_extensions = Some(extensions);
        self
    }

    /// Sets the default content type to use when the extension is unknown.
    /// Defaults to `application/octet-stream`.
    pub fn default_content_type(mut self, content_type: HeaderValue) -> Self {
        self.default_content_type = content_type;
        self
    }

    /// Adds a common header to be added to all successful responses.
    ///
    /// The header will added to the `FileEntity` if the `ServedDir` returns one, but
    /// will not be recoded anywhere in an error response.
    pub fn common_header(mut self, name: header::HeaderName, value: HeaderValue) -> Self {
        self.common_headers.insert(name, value);
        self
    }

    /// Builds the [`ServedDir`].
    pub fn build(self) -> ServedDir {
        ServedDir {
            dirpath: self.dirpath,
            auto_compress: self.auto_compress,
            strip_prefix: self.strip_prefix,
            known_extensions: self.known_extensions,
            default_content_type: self.default_content_type,
            common_headers: self.common_headers,
        }
    }
}

enum ContentEncoding {
    Gzip,
    Brotli,
    Identity,
}

impl ContentEncoding {
    /// Returns the encoding this file is assumed to have applied to the caller's request.
    /// E.g., if automatic gzip compression is enabled and `index.html.gz` was found when the
    /// caller requested `index.html`, this will return `Some("gzip")`. If the caller requests
    /// `index.html.gz`, this will return `None` because the gzip encoding is built in to the
    /// caller's request.
    fn get_header_value(&self) -> Option<HeaderValue> {
        match self {
            ContentEncoding::Gzip => Some(HeaderValue::from_static("gzip")),
            ContentEncoding::Brotli => Some(HeaderValue::from_static("br")),
            ContentEncoding::Identity => None,
        }
    }
}

/// Error returned when creating a `FileEntity` for a path that is not a file.
#[derive(Debug)]
pub struct NotAFile(pub PathBuf);

impl Display for NotAFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Path is not a file: {}", self.0.display())
    }
}

impl Error for NotAFile {}

/// An opened path (aka inode on Unix) as returned by `FsDir::open`.
///
/// This is not necessarily a plain file; it could also be a directory, for example.
///
/// The caller can inspect it as desired. If it is a directory, the caller might pass the result of
/// `into_file()` to `nix::dir::Dir::from`. If it is a plain file, the caller might create an
/// `serve_files::Entity` with `into_file_entity()`.
struct Node {
    path: PathBuf,
    metadata: std::fs::Metadata,
    auto_compress: bool,
    content_encoding: ContentEncoding,
}

impl Node {
    /// Converts this node to a `std::fs::File`.
    fn into_file(self) -> Result<std::fs::File, IOError> {
        File::open(&self.path)
    }

    /// Converts this node (which must represent a plain file) into a `FileEntity`.
    /// The caller is expected to supply all headers. The function `add_encoding_headers`
    /// may be useful.
    fn into_file_entity<D, E>(
        self,
        mut headers: HeaderMap,
    ) -> Result<crate::file::FileEntity<D, E>, IOError>
    where
        D: 'static + Send + Sync + bytes::Buf + From<Vec<u8>> + From<&'static [u8]>,
        E: 'static
            + Send
            + Sync
            + Into<Box<dyn std::error::Error + Send + Sync>>
            + From<Box<dyn std::error::Error + Send + Sync>>,
    {
        if !self.metadata.is_file() {
            return Err(IOError::new(ErrorKind::Other, NotAFile(self.path)));
        }

        // Add `Content-Encoding` and `Vary` headers for the encoding to `hdrs`.
        if let Some(val) = self.content_encoding.get_header_value() {
            headers.insert(header::CONTENT_ENCODING, val);
        }
        if self.auto_compress {
            headers.insert(header::VARY, HeaderValue::from_static("accept-encoding"));
        }

        let file = File::open(&self.path)?;
        crate::file::FileEntity::new_with_metadata(file, &self.metadata, headers)
    }

    /// Returns the (already fetched) metadata for this node.
    fn metadata(&self) -> &std::fs::Metadata {
        &self.metadata
    }

    /// Returns true iff the content varies with the request's `Accept-Encoding` header value.
    fn encoding_varies(&self) -> bool {
        self.auto_compress
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Entity;
    use tempfile::TempDir;

    struct TestContext {
        tmp: TempDir,
        builder: ServedDirBuilder,
    }

    impl TestContext {
        fn write_file(&self, name: &str, contents: &str) {
            std::fs::write(self.tmp.path().join(name), contents.as_bytes())
                .expect("failed to write test file")
        }

        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let path = tmp.path().to_path_buf();
            Self {
                tmp,
                builder: ServedDir::builder(path),
            }
        }
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn test_served_dir_get() {
        let context = TestContext::new();
        context.write_file("one.txt", "one");
        context.write_file("two.json", "more content");
        let served_dir = context.builder.build();
        let hdrs = HeaderMap::new();

        let e1 = served_dir.get("one.txt", &hdrs).await.unwrap();
        assert_eq!(e1.len(), 3);
        assert_eq!(
            e1.header(&http::header::CONTENT_TYPE).unwrap(),
            "text/plain"
        );

        let e2 = served_dir.get("two.json", &hdrs).await.unwrap();
        assert_eq!(e2.len(), 12);
        assert_eq!(
            e2.header(&http::header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_served_dir_not_found() {
        let context = TestContext::new();
        let served_dir = context.builder.build();
        let hdrs = HeaderMap::new();

        let err = served_dir.get("non-existent.txt", &hdrs).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_served_dir_auto_compress() {
        let context = TestContext::new();
        context.write_file("test.txt", "plain text");
        context.write_file("test.txt.gz", "fake gzip content");
        context.write_file("test.txt.br", "fake brotli content");

        // 1. auto_compress enabled (default), Accept-Encoding: gzip
        let served_dir = context.builder.build();
        let mut hdrs = HeaderMap::new();
        hdrs.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("gzip, deflate"),
        );

        let e = served_dir.get("test.txt", &hdrs).await.unwrap();
        assert_eq!(e.header(&header::CONTENT_ENCODING).unwrap(), "gzip");
        assert_eq!(e.read_body().await.unwrap(), "fake gzip content");

        // 2. auto_compress enabled (default), Accept-Encoding: br
        let mut hdrs = HeaderMap::new();
        hdrs.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("br"));
        let e = served_dir.get("test.txt", &hdrs).await.unwrap();
        assert_eq!(e.header(&header::CONTENT_ENCODING).unwrap(), "br");
        assert_eq!(e.read_body().await.unwrap(), "fake brotli content");

        // 3. auto_compress enabled (default), Accept-Encoding: br, gzip (prioritizes br)
        let mut hdrs = HeaderMap::new();
        hdrs.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("br, gzip"),
        );
        let e = served_dir.get("test.txt", &hdrs).await.unwrap();
        assert_eq!(e.header(&header::CONTENT_ENCODING).unwrap(), "br");
        assert_eq!(e.read_body().await.unwrap(), "fake brotli content");

        // 4. auto_compress enabled (default), no Accept-Encoding
        let hdrs = HeaderMap::new();
        let e = served_dir.get("test.txt", &hdrs).await.unwrap();
        assert!(e.header(&header::CONTENT_ENCODING).is_none());
        assert_eq!(e.read_body().await.unwrap(), "plain text");

        // 5. auto_compress disabled
        let served_dir = ServedDir::builder(context.tmp.path().to_path_buf())
            .auto_compress(false)
            .build();
        let mut hdrs = HeaderMap::new();
        hdrs.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("br, gzip"),
        );

        let e = served_dir.get("test.txt", &hdrs).await.unwrap();
        assert!(e.header(&header::CONTENT_ENCODING).is_none());
        assert_eq!(e.read_body().await.unwrap(), "plain text");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_served_dir_forbidden_paths() {
        let context = TestContext::new();
        let served_dir = context.builder.build();
        let hdrs = HeaderMap::new();

        // 1. Contains ".."
        let err = served_dir
            .get("include/../etc/passwd", &hdrs)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert!(err.to_string().contains(".."));

        // 2. Contains null byte
        let err = served_dir.get("test\0file.txt", &hdrs).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert!(err.to_string().contains("NUL byte"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_served_dir_strip_prefix() {
        let context = TestContext::new();
        context.write_file("real.txt", "real content");

        let served_dir = context.builder.strip_prefix("/static/").build();
        let hdrs = HeaderMap::new();

        // Should work with the prefix
        let e = served_dir.get("/static/real.txt", &hdrs).await.unwrap();
        assert_eq!(e.read_body().await.unwrap(), "real content");

        // Should fail without the prefix
        let err = served_dir.get("real.txt", &hdrs).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_served_dir_content_types() {
        let context = TestContext::new();
        context.write_file("index.html", "html");
        context.write_file("style.css", "css");
        context.write_file("script.js", "js");
        context.write_file("image.webp", "webp");
        context.write_file("unknown.foo", "foo");

        let served_dir = context.builder.build();
        let hdrs = HeaderMap::new();

        let e = served_dir.get("index.html", &hdrs).await.unwrap();
        assert_eq!(e.header(&header::CONTENT_TYPE).unwrap(), "text/html");

        let e = served_dir.get("style.css", &hdrs).await.unwrap();
        assert_eq!(e.header(&header::CONTENT_TYPE).unwrap(), "text/css");

        let e = served_dir.get("script.js", &hdrs).await.unwrap();
        assert_eq!(
            e.header(&header::CONTENT_TYPE).unwrap(),
            "application/javascript"
        );

        let e = served_dir.get("image.webp", &hdrs).await.unwrap();
        assert_eq!(e.header(&header::CONTENT_TYPE).unwrap(), "image/webp");

        let e = served_dir.get("unknown.foo", &hdrs).await.unwrap();
        assert_eq!(
            e.header(&header::CONTENT_TYPE).unwrap(),
            "application/octet-stream"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_served_dir_common_headers() {
        let context = TestContext::new();
        context.write_file("one.txt", "one");

        let served_dir = context
            .builder
            .common_header(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=3600"),
            )
            .common_header(
                header::ACCESS_CONTROL_ALLOW_ORIGIN,
                HeaderValue::from_static("*"),
            )
            .build();
        let hdrs = HeaderMap::new();

        let e = served_dir.get("one.txt", &hdrs).await.unwrap();
        assert_eq!(
            e.header(&header::CACHE_CONTROL).unwrap(),
            "public, max-age=3600"
        );
        assert_eq!(e.header(&header::ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(), "*");
        assert_eq!(e.header(&header::CONTENT_TYPE).unwrap(), "text/plain");
    }
}
