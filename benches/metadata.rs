// Copyright (c) 2016-2018 The http-serve developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use criterion::{criterion_group, criterion_main, Criterion};
use http::HeaderMap;
use serve_files::served_dir::ServedDir;
use std::fs::{self, File};
use std::path::Path;

fn path_metadata(path: &Path) -> Option<fs::Metadata> {
    fs::metadata(path).ok()
}

fn file_metadata(path: &Path) -> Option<fs::Metadata> {
    File::open(path).and_then(|f| f.metadata()).ok()
}

fn criterion_benchmark(c: &mut Criterion) {
    let tmpdir = tempfile::tempdir().unwrap();
    let exists_path = tmpdir.path().join("exists");
    fs::write(&exists_path, "hello").unwrap();
    let missing_path = tmpdir.path().join("missing");

    let mut group = c.benchmark_group("metadata");

    group.bench_with_input("path_exists", &exists_path.as_path(), |b, path| {
        b.iter(|| path_metadata(path))
    });

    group.bench_with_input("path_missing", &missing_path.as_path(), |b, path| {
        b.iter(|| path_metadata(path))
    });

    group.bench_with_input("file_exists", &exists_path.as_path(), |b, path| {
        b.iter(|| file_metadata(path))
    });

    group.bench_with_input("file_missing", &missing_path.as_path(), |b, path| {
        b.iter(|| file_metadata(path))
    });

    let served_dir = ServedDir::builder(tmpdir.path()).unwrap().build();
    let hdrs = HeaderMap::new();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    group.bench_function("served_dir_exists", |b| {
        b.to_async(&rt).iter(|| async {
            served_dir.get("exists", &hdrs).await.unwrap();
        })
    });

    group.bench_function("served_dir_missing", |b| {
        b.to_async(&rt).iter(|| async {
            served_dir.get("missing", &hdrs).await.unwrap_err();
        })
    });

    group.finish();
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
