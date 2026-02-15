use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use http::{Request, Response};
use metrics::{counter, histogram};

/// tower layer that records per-RPC metrics for every gRPC call.
///
/// records:
/// - `fides_grpc_requests_total{method, status}` — counter
/// - `fides_grpc_request_duration_seconds{method}` — histogram
#[derive(Debug, Clone)]
pub struct GrpcMetricsLayer;

impl<S> tower_layer::Layer<S> for GrpcMetricsLayer {
    type Service = GrpcMetricsService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        GrpcMetricsService { inner }
    }
}

#[derive(Debug, Clone)]
pub struct GrpcMetricsService<S> {
    inner: S,
}

impl<S, ReqBody, ResBody> tower_service::Service<Request<ReqBody>> for GrpcMetricsService<S>
where
    S: tower_service::Service<Request<ReqBody>, Response = Response<ResBody>>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    ReqBody: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        // tower clone pattern: swap ready service out, keep fresh clone for next poll_ready
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);

        let method = extract_method_name(req.uri().path());
        let start = Instant::now();

        Box::pin(async move {
            let result = inner.call(req).await;
            let duration = start.elapsed().as_secs_f64();

            let status = match &result {
                Ok(response) => grpc_status_from_response(response),
                Err(_) => "error",
            };

            counter!("fides_grpc_requests_total", "method" => method, "status" => status)
                .increment(1);
            histogram!("fides_grpc_request_duration_seconds", "method" => method).record(duration);

            result
        })
    }
}

/// extract RPC method name from gRPC URI path.
///
/// maps known RPCs to static strings (bounded prometheus cardinality).
/// unknown paths fall back to "unknown" rather than creating unbounded label sets.
fn extract_method_name(path: &str) -> &'static str {
    // gRPC path format: /package.Service/Method
    let method = path.rsplit('/').next().unwrap_or("");
    match method {
        "CreateAccount" => "CreateAccount",
        "GetAccount" => "GetAccount",
        "GetBalance" => "GetBalance",
        "Authorize" => "Authorize",
        "Capture" => "Capture",
        "Void" => "Void",
        "GetEntries" => "GetEntries",
        _ => "unknown",
    }
}

/// extract gRPC status from response headers.
///
/// for error responses, tonic uses trailers-only format where grpc-status is in
/// the HTTP headers. for success responses, grpc-status is in trailers (unavailable
/// at this layer). absent header = "ok".
fn grpc_status_from_response<B>(response: &Response<B>) -> &'static str {
    match response.headers().get("grpc-status") {
        None => "ok",
        Some(value) => grpc_code_to_label(value.as_bytes()),
    }
}

/// map gRPC status code bytes to a static label string
fn grpc_code_to_label(code: &[u8]) -> &'static str {
    match code {
        b"0" => "ok",
        b"1" => "cancelled",
        b"2" => "unknown",
        b"3" => "invalid_argument",
        b"4" => "deadline_exceeded",
        b"5" => "not_found",
        b"6" => "already_exists",
        b"7" => "permission_denied",
        b"8" => "resource_exhausted",
        b"9" => "failed_precondition",
        b"10" => "aborted",
        b"11" => "out_of_range",
        b"12" => "unimplemented",
        b"13" => "internal",
        b"14" => "unavailable",
        b"15" => "data_loss",
        b"16" => "unauthenticated",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_known_methods() {
        assert_eq!(
            extract_method_name("/fides.ledger.v1.LedgerService/Authorize"),
            "Authorize"
        );
        assert_eq!(
            extract_method_name("/fides.ledger.v1.LedgerService/Capture"),
            "Capture"
        );
        assert_eq!(
            extract_method_name("/fides.ledger.v1.LedgerService/Void"),
            "Void"
        );
        assert_eq!(
            extract_method_name("/fides.ledger.v1.LedgerService/CreateAccount"),
            "CreateAccount"
        );
        assert_eq!(
            extract_method_name("/fides.ledger.v1.LedgerService/GetAccount"),
            "GetAccount"
        );
        assert_eq!(
            extract_method_name("/fides.ledger.v1.LedgerService/GetBalance"),
            "GetBalance"
        );
        assert_eq!(
            extract_method_name("/fides.ledger.v1.LedgerService/GetEntries"),
            "GetEntries"
        );
    }

    #[test]
    fn extract_unknown_method() {
        assert_eq!(extract_method_name("/some.other.Service/FooBar"), "unknown");
    }

    #[test]
    fn extract_empty_path() {
        assert_eq!(extract_method_name(""), "unknown");
        assert_eq!(extract_method_name("/"), "unknown");
    }

    #[test]
    fn grpc_code_labels() {
        // codes used by ServiceError
        assert_eq!(grpc_code_to_label(b"0"), "ok");
        assert_eq!(grpc_code_to_label(b"3"), "invalid_argument");
        assert_eq!(grpc_code_to_label(b"5"), "not_found");
        assert_eq!(grpc_code_to_label(b"6"), "already_exists");
        assert_eq!(grpc_code_to_label(b"9"), "failed_precondition");
        assert_eq!(grpc_code_to_label(b"10"), "aborted");
        assert_eq!(grpc_code_to_label(b"13"), "internal");
    }

    #[test]
    fn grpc_code_unmapped() {
        assert_eq!(grpc_code_to_label(b"99"), "unknown");
        assert_eq!(grpc_code_to_label(b""), "unknown");
    }

    #[test]
    fn grpc_status_absent_header_is_ok() {
        let response = Response::builder().body(()).unwrap();
        assert_eq!(grpc_status_from_response(&response), "ok");
    }

    #[test]
    fn grpc_status_present_header() {
        let response = Response::builder()
            .header("grpc-status", "5")
            .body(())
            .unwrap();
        assert_eq!(grpc_status_from_response(&response), "not_found");
    }
}
