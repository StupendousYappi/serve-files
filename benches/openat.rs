use criterion::{black_box, criterion_group, criterion_main, Criterion};
use std::ffi::CString;
use std::fs::{write, File};
use std::io::Error;
use std::os::unix::io::{FromRawFd, RawFd};
use std::path::Path;
use tempfile::tempdir;

fn path_buf_open(base: &Path, filename: &str) -> File {
    let path = base.join(filename);
    File::open(path).expect("failed to open file")
}

fn libc_openat(dir_fd: RawFd, filename: &CString) -> File {
    let fd = unsafe {
        libc::openat(
            dir_fd,
            filename.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        panic!("failed to openat: {}", Error::last_os_error());
    }
    unsafe { File::from_raw_fd(fd) }
}

fn bench_open(c: &mut Criterion) {
    let tmp = tempdir().unwrap();
    let filename = "test_file.txt";
    let file_path = tmp.path().join(filename);
    write(&file_path, b"hello world").unwrap();

    let base_path = tmp.path().to_path_buf();

    // Benchmark PathBuf join
    c.bench_function("path_buf_join_open", |b| {
        b.iter(|| {
            let f = path_buf_open(&base_path, filename);
            black_box(f);
        })
    });

    // Benchmark libc openat
    let base_path_cstr = CString::new(base_path.to_str().unwrap()).unwrap();
    let dir_fd = unsafe {
        libc::open(
            base_path_cstr.as_ptr(),
            libc::O_DIRECTORY | libc::O_RDONLY | libc::O_CLOEXEC,
        )
    };
    if dir_fd < 0 {
        panic!("failed to open directory: {}", Error::last_os_error());
    }

    let filename_cstr = CString::new(filename).unwrap();

    c.bench_function("libc_openat", |b| {
        b.iter(|| {
            let f = libc_openat(dir_fd, &filename_cstr);
            black_box(f);
        })
    });

    // Cleanup for libc test
    unsafe { libc::close(dir_fd) };
}

criterion_group!(benches, bench_open);
criterion_main!(benches);
