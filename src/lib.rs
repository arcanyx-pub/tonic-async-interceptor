//! gRPC interceptors which are a kind of middleware.
//!
//! See [`Interceptor`] for more details.

use bytes::Bytes;
use http::{Extensions, Method, Uri, Version};
use http_body::Body;
use http_body_util::BodyExt;
use pin_project::pin_project;
use std::{
    fmt,
    future::Future,
    mem,
    pin::Pin,
    task::{Context, Poll},
};
use tonic::metadata::MetadataMap;
use tonic::{body::BoxBody, Request, Status};
use tower_layer::Layer;
use tower_service::Service;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// An async gRPC interceptor.
///
/// This interceptor is an `async` variant of Tonic's built-in [`Interceptor`].
///
/// gRPC interceptors are similar to middleware but have less flexibility. An interceptor allows
/// you to do two main things, one is to add/remove/check items in the `MetadataMap` of each
/// request. Two, cancel a request with a `Status`.
///
/// Any function that satisfies the bound `async FnMut(Request<()>) -> Result<Request<()>, Status>`
/// can be used as an `AsyncInterceptor`.
///
/// An interceptor can be used on both the server and client side through the `tonic-build` crate's
/// generated structs.
///
/// See the [interceptor example][example] for more details.
///
/// If you need more powerful middleware, [tower] is the recommended approach. You can find
/// examples of how to use tower with tonic [here][tower-example].
///
/// Additionally, interceptors is not the recommended way to add logging to your service. For that
/// a [tower] middleware is more appropriate since it can also act on the response. For example
/// tower-http's [`Trace`](https://docs.rs/tower-http/latest/tower_http/trace/index.html)
/// middleware supports gRPC out of the box.
///
/// [tower]: https://crates.io/crates/tower
/// [example]: https://github.com/hyperium/tonic/tree/master/examples/src/interceptor
/// [tower-example]: https://github.com/hyperium/tonic/tree/master/examples/src/tower
///
/// Async version of `Interceptor`.
pub trait AsyncInterceptor {
    /// The Future returned by the interceptor.
    type Future: Future<Output = Result<Request<()>, Status>>;
    /// Intercept a request before it is sent, optionally cancelling it.
    fn call(&mut self, request: Request<()>) -> Self::Future;
}

impl<F, U> AsyncInterceptor for F
where
    F: FnMut(Request<()>) -> U,
    U: Future<Output = Result<Request<()>, Status>>,
{
    type Future = U;

    fn call(&mut self, request: Request<()>) -> Self::Future {
        self(request)
    }
}

/// Create a new async interceptor layer.
///
/// See [`AsyncInterceptor`] and [`Interceptor`] for more details.
pub fn async_interceptor<F>(f: F) -> AsyncInterceptorLayer<F>
where
    F: AsyncInterceptor,
{
    AsyncInterceptorLayer { f }
}

/// A gRPC async interceptor that can be used as a [`Layer`],
/// created by calling [`async_interceptor`].
///
/// See [`AsyncInterceptor`] for more details.
#[derive(Debug, Clone, Copy)]
pub struct AsyncInterceptorLayer<F> {
    f: F,
}

impl<S, F> Layer<S> for AsyncInterceptorLayer<F>
where
    S: Clone,
    F: AsyncInterceptor + Clone,
{
    type Service = AsyncInterceptedService<S, F>;

    fn layer(&self, service: S) -> Self::Service {
        AsyncInterceptedService::new(service, self.f.clone())
    }
}

/// Convert a [`http_body::Body`] into a [`BoxBody`].
fn boxed<B>(body: B) -> BoxBody
where
    B: Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Error>,
{
    body.map_err(|e| Status::from_error(e.into()))
        .boxed_unsync()
}

// Components and attributes of a request, without metadata or extensions.
#[derive(Debug)]
struct DecomposedRequest<ReqBody> {
    uri: Uri,
    method: Method,
    http_version: Version,
    msg: ReqBody,
}

// Note that tonic::Request::into_parts is not public, so we do it this way.
fn request_into_parts<Msg>(mut req: Request<Msg>) -> (MetadataMap, Extensions, Msg) {
    // We use mem::take because Tonic doesn't not provide public access to these fields.
    let metadata = mem::take(req.metadata_mut());
    let extensions = mem::take(req.extensions_mut());
    (metadata, extensions, req.into_inner())
}

// Note that tonic::Request::from_parts is not public, so we do it this way.
fn request_from_parts<Msg>(
    msg: Msg,
    metadata: MetadataMap,
    extensions: Extensions,
) -> Request<Msg> {
    let mut req = Request::new(msg);
    *req.metadata_mut() = metadata;
    *req.extensions_mut() = extensions;
    req
}

// Note that tonic::Request::into_http is not public, so we do it this way.
fn request_into_http<Msg>(
    msg: Msg,
    uri: http::Uri,
    method: http::Method,
    version: http::Version,
    metadata: MetadataMap,
    extensions: Extensions,
) -> http::Request<Msg> {
    let mut request = http::Request::new(msg);
    *request.version_mut() = version;
    *request.method_mut() = method;
    *request.uri_mut() = uri;
    *request.headers_mut() = metadata.into_headers();
    *request.extensions_mut() = extensions;

    request
}

/// Decompose the request into its contents and properties, and create a new request without a body.
///
/// It is bad practice to modify the body (i.e. Message) of the request via an interceptor.
/// To avoid exposing the body of the request to the interceptor function, we first remove it
/// here, allow the interceptor to modify the metadata and extensions, and then recreate the
/// HTTP request with the original message body with the `recompose` function. Also note that Tonic
/// requests do not preserve the URI, HTTP version, and HTTP method of the HTTP request, so we
/// extract them here and then add them back in `recompose`.
fn decompose<ReqBody>(req: http::Request<ReqBody>) -> (DecomposedRequest<ReqBody>, Request<()>) {
    let uri = req.uri().clone();
    let method = req.method().clone();
    let http_version = req.version();
    let req = Request::from_http(req);
    let (metadata, extensions, msg) = request_into_parts(req);

    let dreq = DecomposedRequest {
        uri,
        method,
        http_version,
        msg,
    };
    let req_without_body = request_from_parts((), metadata, extensions);

    (dreq, req_without_body)
}

/// Combine the modified metadata and extensions with the original message body and attributes.
fn recompose<ReqBody>(
    dreq: DecomposedRequest<ReqBody>,
    modified_req: Request<()>,
) -> http::Request<ReqBody> {
    let (metadata, extensions, _) = request_into_parts(modified_req);

    request_into_http(
        dreq.msg,
        dreq.uri,
        dreq.method,
        dreq.http_version,
        metadata,
        extensions,
    )
}

/// A service wrapped in an async interceptor middleware.
///
/// See [`AsyncInterceptor`] for more details.
#[derive(Clone, Copy)]
pub struct AsyncInterceptedService<S, F> {
    inner: S,
    f: F,
}

impl<S, F> AsyncInterceptedService<S, F> {
    /// Create a new `AsyncInterceptedService` that wraps `S` and intercepts each request with the
    /// function `F`.
    pub fn new(service: S, f: F) -> Self {
        Self { inner: service, f }
    }
}

impl<S, F> fmt::Debug for AsyncInterceptedService<S, F>
where
    S: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AsyncInterceptedService")
            .field("inner", &self.inner)
            .field("f", &format_args!("{}", std::any::type_name::<F>()))
            .finish()
    }
}

impl<S, F, ReqBody, ResBody> Service<http::Request<ReqBody>> for AsyncInterceptedService<S, F>
where
    F: AsyncInterceptor + Clone,
    S: Service<http::Request<ReqBody>, Response = http::Response<ResBody>> + Clone,
    S::Error: Into<Error>,
    ReqBody: Default,
    ResBody: Default + Body<Data = Bytes> + Send + 'static,
    ResBody::Error: Into<Error>,
{
    type Response = http::Response<BoxBody>;
    type Error = S::Error;
    type Future = AsyncResponseFuture<S, F::Future, ReqBody>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<ReqBody>) -> Self::Future {
        // This is necessary because tonic internally uses `tower::buffer::Buffer`.
        // See https://github.com/tower-rs/tower/issues/547#issuecomment-767629149
        // for details on why this is necessary
        let clone = self.inner.clone();
        let inner = std::mem::replace(&mut self.inner, clone);

        AsyncResponseFuture::new(req, &mut self.f, inner)
    }
}

// required to use `AsyncInterceptedService` with `Router`
impl<S, F> tonic::server::NamedService for AsyncInterceptedService<S, F>
where
    S: tonic::server::NamedService,
{
    const NAME: &'static str = S::NAME;
}

/// Response future for [`InterceptedService`].
#[pin_project]
#[derive(Debug)]
pub struct ResponseFuture<F> {
    #[pin]
    kind: Kind<F>,
}

impl<F> ResponseFuture<F> {
    fn future(future: F) -> Self {
        Self {
            kind: Kind::Future(future),
        }
    }

    fn status(status: Status) -> Self {
        Self {
            kind: Kind::Status(Some(status)),
        }
    }
}

#[pin_project(project = KindProj)]
#[derive(Debug)]
enum Kind<F> {
    Future(#[pin] F),
    Status(Option<Status>),
}

impl<F, E, B> Future for ResponseFuture<F>
where
    F: Future<Output = Result<http::Response<B>, E>>,
    E: Into<Error>,
    B: Default + Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Error>,
{
    type Output = Result<http::Response<BoxBody>, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project().kind.project() {
            KindProj::Future(future) => future
                .poll(cx)
                .map(|result| result.map(|res| res.map(boxed))),
            KindProj::Status(status) => {
                let response = status
                    .take()
                    .unwrap()
                    .into_http()
                    .map(|_| B::default())
                    .map(boxed);
                Poll::Ready(Ok(response))
            }
        }
    }
}

#[pin_project(project = PinnedOptionProj)]
#[derive(Debug)]
enum PinnedOption<F> {
    Some(#[pin] F),
    None,
}

/// Response future for [`AsyncInterceptedService`].
///
/// Handles the call to the async interceptor, then calls the inner service and wraps the result in
/// [`ResponseFuture`].
#[pin_project(project = AsyncResponseFutureProj)]
#[derive(Debug)]
pub struct AsyncResponseFuture<S, I, ReqBody>
where
    S: Service<http::Request<ReqBody>>,
    S::Error: Into<Error>,
    I: Future<Output = Result<Request<()>, Status>>,
{
    #[pin]
    interceptor_fut: PinnedOption<I>,
    #[pin]
    inner_fut: PinnedOption<ResponseFuture<S::Future>>,
    inner: S,
    dreq: DecomposedRequest<ReqBody>,
}

impl<S, I, ReqBody> AsyncResponseFuture<S, I, ReqBody>
where
    S: Service<http::Request<ReqBody>>,
    S::Error: Into<Error>,
    I: Future<Output = Result<Request<()>, Status>>,
    ReqBody: Default,
{
    fn new<A: AsyncInterceptor<Future = I>>(
        req: http::Request<ReqBody>,
        interceptor: &mut A,
        inner: S,
    ) -> Self {
        let (dreq, req_without_body) = decompose(req);
        let interceptor_fut = interceptor.call(req_without_body);

        AsyncResponseFuture {
            interceptor_fut: PinnedOption::Some(interceptor_fut),
            inner_fut: PinnedOption::None,
            inner,
            dreq,
        }
    }

    /// Calls the inner service with the intercepted request (which has been modified by the
    /// async interceptor func).
    fn create_inner_fut(
        this: &mut AsyncResponseFutureProj<'_, S, I, ReqBody>,
        intercepted_req: Result<Request<()>, Status>,
    ) -> ResponseFuture<S::Future> {
        match intercepted_req {
            Ok(req) => {
                // We can't move the message body out of the pin projection. So, to
                // avoid copying it, we swap its memory with an empty body and then can
                // move it into the recomposed request.
                let msg = mem::take(&mut this.dreq.msg);
                let movable_dreq = DecomposedRequest {
                    uri: this.dreq.uri.clone(),
                    method: this.dreq.method.clone(),
                    http_version: this.dreq.http_version,
                    msg,
                };
                let modified_req_with_body = recompose(movable_dreq, req);

                ResponseFuture::future(this.inner.call(modified_req_with_body))
            }
            Err(status) => ResponseFuture::status(status),
        }
    }
}

impl<S, I, ReqBody, ResBody> Future for AsyncResponseFuture<S, I, ReqBody>
where
    S: Service<http::Request<ReqBody>, Response = http::Response<ResBody>>,
    I: Future<Output = Result<Request<()>, Status>>,
    S::Error: Into<Error>,
    ReqBody: Default,
    ResBody: Default + Body<Data = Bytes> + Send + 'static,
    ResBody::Error: Into<Error>,
{
    type Output = Result<http::Response<BoxBody>, S::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();

        // The struct was initialized (via `new`) with interceptor func future, which we poll here.
        if let PinnedOptionProj::Some(f) = this.interceptor_fut.as_mut().project() {
            match f.poll(cx) {
                Poll::Ready(intercepted_req) => {
                    let inner_fut = AsyncResponseFuture::<S, I, ReqBody>::create_inner_fut(
                        &mut this,
                        intercepted_req,
                    );
                    // Set the inner service future and clear the interceptor future.
                    this.inner_fut.set(PinnedOption::Some(inner_fut));
                    this.interceptor_fut.set(PinnedOption::None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
        // At this point, inner_fut should always be Some.
        let inner_fut = match this.inner_fut.project() {
            PinnedOptionProj::None => panic!(),
            PinnedOptionProj::Some(f) => f,
        };

        inner_fut.poll(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::StatusCode;
    use http_body_util::Empty;
    use std::future;
    use tower::ServiceExt;

    #[tokio::test]
    async fn propagates_added_extensions() {
        #[derive(Clone)]
        struct TestExtension {
            data: String,
        }
        let test_extension_data = "abc";

        let layer = async_interceptor(|mut req: Request<()>| {
            req.extensions_mut().insert(TestExtension {
                data: test_extension_data.to_owned(),
            });

            future::ready(Ok(req))
        });

        let svc = layer.layer(tower::service_fn(
            |http_req: http::Request<Empty<Bytes>>| async {
                let req = Request::from_http(http_req);
                let maybe_extension = req.extensions().get::<TestExtension>();
                assert!(maybe_extension.is_some());
                assert_eq!(maybe_extension.unwrap().data, test_extension_data);

                Ok::<_, Status>(http::Response::new(Empty::new()))
            },
        ));

        let request = http::Request::builder().body(Empty::new()).unwrap();
        let http_response = svc.oneshot(request).await.unwrap();

        assert_eq!(http_response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn propagates_added_metadata() {
        let test_metadata_key = "test_key";
        let test_metadata_val = "abc";

        let layer = async_interceptor(|mut req: Request<()>| {
            req.metadata_mut()
                .insert(test_metadata_key, test_metadata_val.parse().unwrap());

            future::ready(Ok(req))
        });

        let svc = layer.layer(tower::service_fn(
            |http_req: http::Request<Empty<Bytes>>| async {
                let req = Request::from_http(http_req);
                let maybe_metadata = req.metadata().get(test_metadata_key);
                assert!(maybe_metadata.is_some());
                assert_eq!(maybe_metadata.unwrap(), test_metadata_val);

                Ok::<_, Status>(http::Response::new(Empty::new()))
            },
        ));

        let request = http::Request::builder().body(Empty::new()).unwrap();
        let http_response = svc.oneshot(request).await.unwrap();

        assert_eq!(http_response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn doesnt_remove_headers_from_request() {
        let layer = async_interceptor(|request: Request<()>| {
            assert_eq!(
                request
                    .metadata()
                    .get("user-agent")
                    .expect("missing in interceptor"),
                "test-tonic"
            );
            future::ready(Ok(request))
        });

        let svc = layer.layer(tower::service_fn(
            |request: http::Request<Empty<Bytes>>| async move {
                assert_eq!(
                    request
                        .headers()
                        .get("user-agent")
                        .expect("missing in leaf service"),
                    "test-tonic"
                );

                Ok::<_, Status>(http::Response::new(Empty::new()))
            },
        ));

        let request = http::Request::builder()
            .header("user-agent", "test-tonic")
            .body(Empty::new())
            .unwrap();

        svc.oneshot(request).await.unwrap();
    }

    #[tokio::test]
    async fn handles_intercepted_status_as_response() {
        let message = "Blocked by the interceptor";
        let expected = Status::permission_denied(message).into_http();

        let layer = async_interceptor(|_: Request<()>| {
            future::ready(Err(Status::permission_denied(message)))
        });

        let svc = layer.layer(tower::service_fn(|_: http::Request<Empty<Bytes>>| async {
            Ok::<_, Status>(http::Response::new(Empty::new()))
        }));

        let request = http::Request::builder().body(Empty::new()).unwrap();
        let response = svc.oneshot(request).await.unwrap();

        assert_eq!(expected.status(), response.status());
        assert_eq!(expected.version(), response.version());
        assert_eq!(expected.headers(), response.headers());
    }

    #[tokio::test]
    async fn doesnt_change_http_method() {
        let layer = async_interceptor(|request: Request<()>| future::ready(Ok(request)));

        let svc = layer.layer(tower::service_fn(
            |request: http::Request<Empty<Bytes>>| async move {
                assert_eq!(request.method(), http::Method::OPTIONS);

                Ok::<_, Status>(http::Response::new(Empty::new()))
            },
        ));

        let request = http::Request::builder()
            .method(http::Method::OPTIONS)
            .body(Empty::new())
            .unwrap();

        svc.oneshot(request).await.unwrap();
    }
}
