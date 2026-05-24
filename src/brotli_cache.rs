use std::{
    collections::HashSet,
    env,
    fmt::Debug,
    fs::File,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::Arc,
    sync::LazyLock,
};

use brotli::enc::backward_references::BrotliEncoderMode;
use brotli::enc::BrotliEncoderParams;
use log::{debug, warn};
use sieve_cache::{Weigh, WeightedShardedSieveCache};
use std::io::Write;

use crate::compression::{ContentEncoding, MatchedFile};
use crate::SerdirError;

type CacheKey = crate::FileInfo;

impl Weigh for crate::FileInfo {
    fn weigh(&self) -> usize {
        0
    }
}

impl Weigh for MatchedFile {
    fn weigh(&self) -> usize {
        self.file_info.len() as usize
    }
}

const BUF_SIZE: usize = 8192;

static DEFAULT_TEXT_TYPES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        "html", "htm", "xhtml", "css", "js", "json", "xml", "svg", "txt", "csv", "tsv", "md",
    ])
});

/// A service that creates and reuses Brotli-compressed versions of files, returning
/// a Resource for the compressed file if it's supported, or a Resource for the
/// original file if it's not.
pub(crate) struct BrotliCache {
    tempdir: PathBuf,
    cache: WeightedShardedSieveCache<CacheKey, MatchedFile>,
    params: BrotliEncoderParams,
    supported_extensions: HashSet<&'static str>,
    max_file_size: u64,
}

impl From<crate::compression::CachedCompression> for BrotliCache {
    fn from(value: crate::compression::CachedCompression) -> Self {
        let tempdir = env::temp_dir();
        let cache = WeightedShardedSieveCache::new(
            value.capacity as usize,
            value.max_total_weight as usize,
        )
        .expect("brotli cache capacity and max total weight cannot be zero");
        let params = BrotliEncoderParams {
            quality: i32::from(value.compression_level),
            ..Default::default()
        };
        let supported_extensions = value
            .supported_extensions
            .clone()
            .unwrap_or_else(|| DEFAULT_TEXT_TYPES.clone());

        BrotliCache {
            tempdir,
            cache,
            params,
            supported_extensions,
            max_file_size: value.max_file_size,
        }
    }
}

impl BrotliCache {
    fn detect_brotli_mode(&self, extension: &str) -> BrotliEncoderMode {
        match self.supported_extensions.contains(extension) {
            true => BrotliEncoderMode::BROTLI_MODE_TEXT,
            false => BrotliEncoderMode::BROTLI_MODE_GENERIC,
        }
    }

    /// Returns a Resource for the compressed file if it's supported, or a Resource for the
    /// original file if it's not.
    ///
    /// If a cached Resource already exists for the given file, it will be reused. Otherwise,
    /// the file will be compressed and a new Resource will be created and cached.
    ///
    /// The underlying tempfile for the compressed file is unlinked on the
    /// filesystem and inaccessible externally. It will be deleted when the last
    /// `Resource` referencing it is dropped (such as when the BrotliCache is
    /// dropped), or when the application is terminated (deletion is handled by
    /// the operating system and should be performed even after hard crashes of
    /// the application).
    pub(crate) async fn get(&self, path: &Path) -> Result<MatchedFile, SerdirError> {
        let extension: &str = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        // We don't bother compressing file types that are already compressed, and instead
        // just return a Resource for the original file.
        if !self.supported_extensions.contains(extension) {
            return Self::wrap_orig(path, extension);
        }

        //let file_info = tokio::task::block_in_place(|| crate::FileInfo::for_path(path))?;
        let file_info = crate::FileInfo::for_path(path)?;

        // Skip compression if the file is too large.
        if file_info.len() > self.max_file_size {
            return Self::wrap_orig(path, extension);
        }

        // If we have a cache entry for the file, return it.
        let matched: Option<MatchedFile> = self.cache.get(&file_info);
        if let Some(f) = matched {
            return Ok(f);
        }

        let mut params = self.params.clone();
        params.mode = self.detect_brotli_mode(extension);

        if let Ok(len) = usize::try_from(file_info.len()) {
            params.size_hint = len;
        }

        let path_buf = path.to_path_buf();
        let tempdir = self.tempdir.clone();
        let brotli_file = match tokio::task::spawn_blocking(move || {
            Self::compress_internal(&tempdir, &path_buf, params)
        })
        .await
        .map_err(|e| {
            SerdirError::compression_error("Join error".to_string(), std::io::Error::other(e))
        })? {
            Ok(v) => v,
            Err(e) if e.kind() == ErrorKind::StorageFull => {
                warn!(
                    "Disk full while compressing {}; purging Brotli cache",
                    path.display()
                );
                self.prune_cache();
                return Self::wrap_orig(path, extension);
            }
            Err(e) => {
                let msg = format!("Brotli compression failed for {}", path.display());
                return Err(SerdirError::compression_error(msg, e));
            }
        };

        let brotli_metadata = brotli_file.metadata()?;
        debug!(
            "Successfully compressed file: path={}, original_size={}, compressed_size={}",
            path.display(),
            file_info.len(),
            brotli_metadata.len()
        );

        // The cached brotli tempfile literally doesn't have a filename, so we can't
        // calculate a path hash for it. Instead, we reverse the bytes of the original
        // file's path hash to create a pseudo path hash that is deterministic in the same
        // ways as the original path hash, and just as unlikely to collide with any other
        // value.
        let pseudo_hash = file_info.get_hash().swap_bytes();

        let brotli_file_info = crate::FileInfo {
            path_hash: pseudo_hash,
            len: brotli_metadata.len(),
            // arguably it would be more accurate and produce a higher browser
            // cache hit rate to use the same modification time as the original
            // file, but given that we use strong ETag values (i.e. different
            // for the original and compressed versions), I think it's more
            // consistent for the modification times to potentially differ too.
            mtime: brotli_metadata.modified()?,
        };

        let matched = MatchedFile {
            file: Arc::new(brotli_file),
            file_info: brotli_file_info,
            content_encoding: ContentEncoding::Brotli,
            extension: extension.to_string(),
        };
        self.cache.insert(file_info, matched.clone());
        Ok(matched)
    }

    /// Evicts the largest half of cached Brotli files, allowing that disk space to be reclaimed.
    fn prune_cache(&self) {
        let mut entries = self.cache.entries();
        entries.sort_unstable_by_key(|(_, matched)| matched.file_info.len());

        let keep_count = entries.len() / 2;
        let keep_keys: HashSet<CacheKey> = entries
            .into_iter()
            .take(keep_count)
            .map(|(key, _)| key)
            .collect();

        self.cache.retain(|key, _| keep_keys.contains(key));
    }

    /// Returns a Resource for the uncompressed file
    ///
    /// Used when skipping compression.
    fn wrap_orig(path: &Path, extension: &str) -> Result<MatchedFile, SerdirError> {
        let file = File::open(path)?;
        let file_info = crate::FileInfo::open_file(path, &file)?;
        let extension = extension.to_string();
        Ok(MatchedFile {
            file: Arc::new(file),
            file_info,
            content_encoding: ContentEncoding::Identity,
            extension,
        })
    }

    fn compress_internal(
        tempdir: &Path,
        path: &Path,
        params: BrotliEncoderParams,
    ) -> Result<File, crate::IOError> {
        let brotli_file: File = tempfile::tempfile_in(tempdir)?;
        let mut compressor = brotli::CompressorWriter::with_params(brotli_file, BUF_SIZE, &params);

        let mut file = File::open(path)?;
        std::io::copy(&mut file, &mut compressor)?;
        compressor.flush()?;
        let brotli_file = compressor.into_inner();
        Ok(brotli_file)
    }
}

impl Debug for BrotliCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrotliCache")
            .field("cache_size", &self.cache.capacity())
            .field("compression_level", &self.params.quality)
            .field("tempdir", &self.tempdir)
            .field("supported_extensions", &self.supported_extensions)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashSet, io::Read};

    static CAT_PHOTO_BYTES: &[u8] = include_bytes!("test-resources/cat.jpg");
    static BOOK_BYTES: &[u8] = include_bytes!("test-resources/wonderland.txt");

    /// Reads the contents of a file, optionally decompressing it if it's a brotli file.
    fn read_bytes(f: &File, decompress: bool) -> Vec<u8> {
        use std::io::{Seek, SeekFrom};
        let mut f = f;
        f.seek(SeekFrom::Start(0))
            .expect("Failed to seek to beginning");
        let mut raw_bytes = Vec::new();
        if decompress {
            let mut input = brotli::Decompressor::new(f, 4096);
            let _ = input
                .read_to_end(&mut raw_bytes)
                .expect("Failed to decompress file");
        } else {
            let mut f = f;
            f.read_to_end(&mut raw_bytes).expect("Failed to read file");
        }
        raw_bytes
    }

    #[test]
    fn test_brotli_cache_initialization_defaults() {
        let settings = crate::compression::CachedCompression::default();
        let cache = BrotliCache::from(settings);
        assert_eq!(
            cache.params.quality,
            i32::from(crate::compression::BrotliLevel::L5)
        );
        assert_eq!(cache.cache.capacity(), 1024);
        assert_eq!(cache.max_file_size, 10 * 1024 * 1024);
    }

    #[test]
    fn test_brotli_cache_initialization_custom() {
        let mut extensions = HashSet::new();
        extensions.insert("html");

        let settings = crate::compression::CachedCompression::new()
            .capacity(64)
            .compression_level(crate::compression::BrotliLevel::L5)
            .supported_extensions(Some(extensions.clone()));

        let cache = BrotliCache::from(settings);

        assert_eq!(cache.cache.capacity(), 64);
        assert_eq!(cache.params.quality, 5);
        assert_eq!(cache.supported_extensions, extensions);
    }

    #[test]
    fn test_brotli_cache_build() {
        let cache = BrotliCache::from(
            crate::compression::CachedCompression::new()
                .capacity(16)
                .compression_level(crate::compression::BrotliLevel::L1),
        );

        assert_eq!(cache.params.quality, 1);
    }

    #[tokio::test]
    async fn test_simple_compression() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.html");
        let content = "<html><body><h1>Hello World</h1></body></html>";
        {
            let mut f = File::create(&path).unwrap();
            f.write_all(content.as_bytes()).unwrap();
        }

        let cache = BrotliCache::from(
            crate::compression::CachedCompression::new()
                .compression_level(crate::compression::BrotliLevel::L1),
        );

        let matched = cache
            .get(&path)
            .await
            .expect("Failed to get file from cache");

        assert!(matches!(matched.content_encoding, ContentEncoding::Brotli));

        // Verify FileInfo
        let orig_info = crate::FileInfo::for_path(&path).unwrap();
        assert_ne!(matched.file_info.get_hash(), orig_info.get_hash());
        let delta_nanos = matched
            .file_info
            .mtime()
            .duration_since(orig_info.mtime())
            .unwrap()
            .as_nanos();
        // between 0 and 10ms
        assert!(delta_nanos < 10_000_000);

        // Verify decompressed content
        let decompressed = read_bytes(&matched.file, true);
        assert_eq!(decompressed, content.as_bytes());

        // Verify second call returns cached version
        let matched2 = cache
            .get(&path)
            .await
            .expect("Failed to get file from cache second time");
        assert_eq!(matched2.file_info.mtime(), matched.file_info.mtime());
    }

    #[tokio::test]
    async fn test_compression_levels() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wonderland.txt");
        {
            let mut f = File::create(&path).unwrap();
            f.write_all(BOOK_BYTES).unwrap();
        }

        let cache0 = BrotliCache::from(
            crate::compression::CachedCompression::new()
                .compression_level(crate::compression::BrotliLevel::L0),
        );
        let cache5 = BrotliCache::from(
            crate::compression::CachedCompression::new()
                .compression_level(crate::compression::BrotliLevel::L5),
        );

        let matched0 = cache0
            .get(&path)
            .await
            .expect("Failed to get file from cache0");
        let matched5 = cache5
            .get(&path)
            .await
            .expect("Failed to get file from cache5");

        assert!(matches!(matched0.content_encoding, ContentEncoding::Brotli));
        assert!(matches!(matched5.content_encoding, ContentEncoding::Brotli));

        // Verify decompressed content for both
        let decompressed0 = read_bytes(&matched0.file, true);
        assert_eq!(decompressed0, BOOK_BYTES);

        let decompressed5 = read_bytes(&matched5.file, true);
        assert_eq!(decompressed5, BOOK_BYTES);

        // Verify that level 5 is smaller than level 0
        let size0 = matched0.file.metadata().unwrap().len();
        let size5 = matched5.file.metadata().unwrap().len();
        // the first output should be larger than the second
        assert!(size0 > size5);
        assert_eq!(83898, size0);
        assert_eq!(59317, size5);
    }

    #[tokio::test]
    async fn test_skip_compression() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jpg");
        {
            let mut f = File::create(&path).unwrap();
            f.write_all(CAT_PHOTO_BYTES).unwrap();
        }

        // Initialize with default text types, which should NOT include jpg
        let cache = BrotliCache::from(crate::compression::CachedCompression::default());

        let matched = cache
            .get(&path)
            .await
            .expect("Failed to get file from cache");

        // JPG should skip compression
        assert!(matches!(
            matched.content_encoding,
            ContentEncoding::Identity
        ));

        // Verify FileInfo
        let orig_info = crate::FileInfo::for_path(&path).unwrap();
        assert_eq!(matched.file_info.len(), orig_info.len());
        assert_eq!(matched.file_info.mtime(), orig_info.mtime());

        // Verify content
        let bytes = read_bytes(&matched.file, false);
        assert_eq!(bytes, CAT_PHOTO_BYTES);
    }

    #[tokio::test]
    async fn test_max_file_size() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("too_big.txt");
        let content = "This is a test file that is larger than 10 bytes.";
        {
            let mut f = File::create(&path).unwrap();
            f.write_all(content.as_bytes()).unwrap();
        }

        // Cache with max_file_size of 10 bytes
        let cache =
            BrotliCache::from(crate::compression::CachedCompression::new().max_file_size(10));

        let matched = cache
            .get(&path)
            .await
            .expect("Failed to get file from cache");

        // Should skip compression because size (49) > max_file_size (10)
        assert!(matches!(
            matched.content_encoding,
            ContentEncoding::Identity
        ));

        // Now try a smaller file
        let path_small = dir.path().join("small.txt");
        let content_small = "small"; // 5 bytes
        {
            let mut f = File::create(&path_small).unwrap();
            f.write_all(content_small.as_bytes()).unwrap();
        }

        let matched_small = cache
            .get(&path_small)
            .await
            .expect("Failed to get small file from cache");

        // Should NOT skip compression
        assert!(matches!(
            matched_small.content_encoding,
            ContentEncoding::Brotli
        ));
    }

    #[tokio::test]
    async fn test_prune_cache_keeps_smallest_half() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let cache = BrotliCache::from(crate::compression::CachedCompression::new().capacity(16));

        for (name, size) in [
            ("a.txt", 64usize),
            ("b.txt", 256usize),
            ("c.txt", 1024usize),
            ("d.txt", 4096usize),
        ] {
            let path = dir.path().join(name);
            let mut f = File::create(&path).unwrap();
            f.write_all(&vec![b'x'; size]).unwrap();
            let _ = cache.get(&path).await.unwrap();
        }

        let mut before = cache.cache.entries();
        before.sort_unstable_by_key(|(_, matched)| matched.file_info.len());
        let expected_sizes: Vec<u64> = before
            .iter()
            .take(before.len() / 2)
            .map(|(_, matched)| matched.file_info.len())
            .collect();
        let max_expected_size = *expected_sizes.last().unwrap();

        cache.prune_cache();

        let remaining = cache.cache.entries();
        assert_eq!(remaining.len(), expected_sizes.len());
        assert!(remaining
            .iter()
            .all(|(_, matched)| matched.file_info.len() <= max_expected_size));
    }
}
