#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let value = match http::header::HeaderValue::from_bytes(data) {
        Ok(v) => v,
        Err(_) => return,
    };
    let mut map = http::HeaderMap::new();
    map.insert(http::header::ACCEPT_ENCODING, value);
    let _ = serve_files::detect_compression_support(&map);
});
