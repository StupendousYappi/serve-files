# UNRELASED

- Improved consistency of error logging between direct use and middleware use of `ServedDir`
- Performance optimizations
- Simplified `serve` function API
- Renamed `ServedDirBuilder::new` to `ServedDir::builder`
- Added optional support for in-memory caching of file content

# 0.3.0

- Improved examples, including adding more CLI options and adding an example
with use of raw `ServedDir` APis with hyper.
- Security fixes related to path traversal on Windows.
- Replaced unmaintained `winapi` dependency with `windows-sys`
- Optimized and hardened CI configuration
- Increased test coverage
- Eliminated `memchr` dependency

# 0.2.1

First public release (doc improvements for yanked 0.2.0 release)