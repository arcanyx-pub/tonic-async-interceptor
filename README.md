# Tonic Async Interceptor

[![Crates.io](https://img.shields.io/crates/v/tonic-async-interceptor)](https://crates.io/crates/tonic-async-interceptor)
[![Documentation](https://docs.rs/tonic-async-interceptor/badge.svg)](https://docs.rs/tonic-async-interceptor)
[![Crates.io](https://img.shields.io/crates/l/tonic-async-interceptor)](LICENSE)

This crate contains `AsyncInterceptor`, an async variant of Tonic's
[`Interceptor`](https://docs.rs/tonic/latest/tonic/service/trait.Interceptor.html).
Other than accepting an async interceptor function, it works the same as `Interceptor`.

Async interceptor functions are useful for tasks like authentication, where you need to make
asynchronous calls to another service or database within the interceptor.

## Compatibility

The major/minor version corresponds with that of Tonic; so, if you are using Tonic v0.12.x, then you should likewise use Tonic Async Interceptor v0.12.x.

## Usage

### Using with Tonic built-in Server/Router

```rust
async fn my_async_interceptor(req: Request<()>) -> Result<Request<()>, Status> {
    // do things and stuff
    Ok(req)
}

async fn main() {
    use tonic::transport::server;
    use tonic_async_interceptor::async_interceptor;
    let router = server::Server::builder()
        .layer(async_interceptor(my_async_interceptor))
        .add_service(some_service)
        .add_service(another_service);
    // ...
}
```

### Setting a custom extension

Here's an example of an async interceptor which authenticates a user and sets a custom
extension for the underlying service to use.

```rust
// Your custom extension
struct UserContext {
    user_id: String,
}

// Async interceptor fn
async fn authenticate(req: Request<()>) -> Result<Request<()>, Status> {
    // Inspect the gRPC metadata.
    let auth_header_val = match req.metadata().get("x-my-auth-header") {
        Some(val) => val,
        None => return Err(Status::unauthorized("Request missing creds")),
    };
    // Call some async function (`verify_auth`).
    let maybe_user_id: Result<String> =
        verify_auth(auth_header_val).await;
    
    let user_id = match maybe_user_id {
        Ok(id) => id,
        Err(_) => return Err(Status::unauthorized("Invalid creds")),
    };
    
    // Insert an extension, which can be inspected by the service.
    req.extensions_mut().insert(UserContext { user_id });
    
    Ok(req)
}
```

## Why is this a separate crate?

The code in this crate was originally intended to live in the official Tonic crate, alongside
the non-async interceptor code. However, the maintainer
[decided not to merge it](https://github.com/hyperium/tonic/pull/910)
due to lack of time and misalignment with his future vision for Tonic.
