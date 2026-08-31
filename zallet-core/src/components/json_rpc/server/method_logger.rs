//! Call logging for the JSON-RPC server.

use futures::future::BoxFuture;
use jsonrpsee::{
    MethodResponse, server::middleware::rpc::RpcServiceT, tracing::debug, types::Request,
};

/// JSON-RPC middleware that logs each call by method name only.
///
/// This deliberately never logs call parameters or responses: they can carry
/// secrets (wallet passphrases, exported or imported keys) and private
/// transaction data, which must not end up in log output at any verbosity.
#[derive(Clone)]
pub struct MethodLoggerMiddleware<S> {
    service: S,
}

impl<S> MethodLoggerMiddleware<S> {
    /// Create a new `MethodLoggerMiddleware` with the given `service`.
    pub fn new(service: S) -> Self {
        Self { service }
    }
}

impl<'a, S> RpcServiceT<'a> for MethodLoggerMiddleware<S>
where
    S: RpcServiceT<'a> + Send + Sync + Clone + 'static,
{
    type Future = BoxFuture<'a, MethodResponse>;

    fn call(&self, request: Request<'a>) -> Self::Future {
        let service = self.service.clone();
        Box::pin(async move {
            let method = request.method_name().to_owned();
            let response = service.call(request).await;
            debug!(method, is_error = response.is_error(), "rpc call");
            response
        })
    }
}
