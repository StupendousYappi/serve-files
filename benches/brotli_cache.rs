use criterion::{criterion_group, criterion_main, Criterion};
use http::HeaderMap;
use serdir::compression::BrotliLevel;
use serdir::ServedDir;
use std::fs::File;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

fn criterion_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("brotli_cache_group"); // changed name

    let num_files = 1000;
    let temp_dir = tempfile::tempdir().unwrap();
    let mut paths = Vec::with_capacity(num_files);

    for i in 0..num_files {
        let name = format!("file_{}.html", i);
        let tmppath = temp_dir.path().join(&name);
        let mut tmpfile = File::create(&tmppath).unwrap();
        tmpfile
            .write_all(b"<html><body><h1>Hello World</h1></body></html>")
            .unwrap();
        tmpfile.flush().unwrap();
        paths.push(format!("/{}", name));
    }

    let served_dir = ServedDir::builder(temp_dir.path())
        .unwrap()
        .cached_compression(BrotliLevel::L0) // Enables CachedCompression
        .build();

    let served_dir = Arc::new(served_dir);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();

    let mut hdrs = HeaderMap::new();
    hdrs.insert(http::header::ACCEPT_ENCODING, "br".parse().unwrap());

    group.bench_function("brotli_cache_get_many_files", |b| {
        b.to_async(&rt).iter(|| async {
            let mut tasks = Vec::with_capacity(paths.len());
            for path in &paths {
                let sd = served_dir.clone();
                let p = path.clone();
                let hdrs = hdrs.clone();
                tasks.push(tokio::spawn(async move {
                    let _ = sd.get(&p, &hdrs).await;
                }));
            }
            for task in tasks {
                let _ = task.await;
            }
        });
    });

    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(5));
    targets = criterion_benchmark
}
criterion_main!(benches);
