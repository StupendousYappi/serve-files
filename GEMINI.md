This is a Rust crate that helps users serve static files over HTTP.

The original version of this crate provided two main APIs:

1. `serve` - a function that serves implementations of the `Entity` trait, such as files, that know their own size.
   This function supports conditional requests (If-Modified-Since, If-None-Match) and range requests.
2. `streaming_body` - a function that can add a body to an otherwise complete HTTP response, allowing the body
    to be generated dynamically and streamed

The original version of this crate is available at

https://github.com/scottlamb/http-serve 

The original version of this crate focused on compatibility with the `hyper` web server, which is a low-level API. I'm forking
the crate, removing the `streaming_body` functionality, and adding integration with the `tower` network service ecosystem.

## The `serve` function

The `serve` function serves HTTP `GET` and `HEAD` requests for a given byte-ranged `Entity`. It handles conditional GETs
and subrange requests. Its function signature is:

```rust
pub fn serve<Ent: Entity, B: http_body::Body + From<Box<dyn Stream<Item = Result<Ent::Data, Ent::Error>> + Send>>, BI>(
    entity: Ent,
    req: &http:Request<BI>,
) -> http:Response<B>
```

The definition of the `Entity` trait is:

```rust
pub trait Entity:
    'static
    + Send
    + Sync {
    type Error: 'static + Send + Sync;
    type Data: 'static + Send + Sync + Buf + From<Vec<u8>> + From<&'static [u8]>;

    // Required methods
    fn len(&self) -> u64;
    fn get_range(
        &self,
        range: Range<u64>,
    ) -> Box<dyn Stream<Item = Result<Self::Data, Self::Error>> + Send + Sync>;
    fn add_headers(&self, _: &mut HeaderMap);
    fn etag(&self) -> Option<HeaderValue>;
    fn last_modified(&self) -> Option<SystemTime>;

    // Provided method
    fn is_empty(&self) -> bool { ... }
}
```

Responses returned by `serve` will add an `ETAG` header to the response if the input `Entity` provides one.

## Project structure

The project is organized as a library crate with the following modules:

- `lib.rs` - crate-level documentation and re-exports
- `etag.rs` - ETag parsing and comparison
- `range.rs` - range header parsing and validation
- `chunker.rs` - chunked encoding utilities
- `body.rs` - HTTP response body implementation
- `file.rs` - file entity implementation

## Testing

The project comes with a basic suite of unit tests that can be run with `cargo test`.

You can check for correct syntax using `cargo check`.

## Documentation

When making changes to the code, don't change the crate-level documentation in `src/lib.rs`. I'll update it myself
once all the code changes are complete.