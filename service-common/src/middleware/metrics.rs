use crate::PKG_NAME;
use axum::extract::{MatchedPath, Request};
use axum::response::Response;
use futures_util::future::BoxFuture;
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Histogram, MeterProvider};
use opentelemetry_semantic_conventions::attribute::{
    HTTP_REQUEST_METHOD, HTTP_RESPONSE_STATUS_CODE, HTTP_ROUTE, NETWORK_PROTOCOL_VERSION,
    URL_SCHEME,
};
use opentelemetry_semantic_conventions::metric::HTTP_SERVER_REQUEST_DURATION;
use std::task::{Context, Poll};
use std::time::Instant;
use tower::{Layer, Service};

#[derive(Clone)]
pub struct RequestMetricsLayer {
    request_duration: Histogram<f64>,
}

impl RequestMetricsLayer {
    pub fn new<P>(meter_provider: P) -> RequestMetricsLayer
    where
        P: MeterProvider,
    {
        let meter = meter_provider.meter(PKG_NAME);
        let request_duration = meter
            .f64_histogram(HTTP_SERVER_REQUEST_DURATION)
            .with_description("Duration of HTTP server requests.")
            .with_unit("s")
            .with_boundaries(vec![
                0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
            ])
            .build();
        RequestMetricsLayer { request_duration }
    }
}

impl<S> Layer<S> for RequestMetricsLayer {
    type Service = RequestMetricsMiddleware<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequestMetricsMiddleware {
            inner,
            request_duration: self.request_duration.clone(),
        }
    }
}

#[derive(Clone)]
pub struct RequestMetricsMiddleware<S> {
    inner: S,
    request_duration: Histogram<f64>,
}

impl<S> Service<Request> for RequestMetricsMiddleware<S>
where
    S: Service<Request, Response = Response> + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        let start_time = Instant::now();

        let request_duration = self.request_duration.clone();
        let method = request.method();
        let route = request
            .extensions()
            .get::<MatchedPath>()
            .map(|p| p.as_str());
        let scheme = request.uri().scheme_str().unwrap_or("http");

        let mut attributes = vec![
            KeyValue::new(HTTP_REQUEST_METHOD, method.to_string()),
            KeyValue::new(URL_SCHEME, scheme.to_string()),
            // TODO: consider trimming HTTP/ from the version
            KeyValue::new(NETWORK_PROTOCOL_VERSION, format!("{:?}", request.version())),
        ];

        if let Some(route) = route {
            attributes.push(KeyValue::new(HTTP_ROUTE, route.to_owned()));
        }

        let future = self.inner.call(request);
        Box::pin(async move {
            let response = future.await?;

            attributes.push(KeyValue::new(
                HTTP_RESPONSE_STATUS_CODE,
                response.status().as_u16() as i64,
            ));

            request_duration.record(start_time.elapsed().as_secs_f64(), &attributes);
            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::routing::get;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
    use std::collections::{HashMap, HashSet};
    use tower::ServiceExt;

    #[tokio::test]
    async fn request_duration() {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_periodic_exporter(exporter.clone())
            .build();

        let app = Router::new()
            .route("/", get(|| async {}))
            .layer(RequestMetricsLayer::new(provider.clone()));

        let _ = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        provider.force_flush().unwrap();
        let resource_metrics = exporter.get_finished_metrics().unwrap();
        assert_eq!(resource_metrics.len(), 1);

        let scope_metrics = resource_metrics[0].scope_metrics().next().unwrap();
        assert_eq!(scope_metrics.scope().name(), PKG_NAME);

        let metrics: HashMap<_, _> = scope_metrics.metrics().map(|m| (m.name(), m)).collect();
        let metric = metrics[HTTP_SERVER_REQUEST_DURATION];

        let metric_data = match metric.data() {
            AggregatedMetrics::F64(MetricData::Histogram(x)) => x,
            _ => panic!("wrong metric data type"),
        };
        let data_point = metric_data.data_points().next().unwrap();
        assert_eq!(data_point.count(), 1);

        let attributes: HashSet<_> = data_point.attributes().cloned().collect();
        let expected_attributes = HashSet::from([
            KeyValue::new(HTTP_REQUEST_METHOD, "GET"),
            KeyValue::new(HTTP_RESPONSE_STATUS_CODE, 200),
            KeyValue::new(HTTP_ROUTE, "/"),
            KeyValue::new(NETWORK_PROTOCOL_VERSION, "HTTP/1.1"),
            KeyValue::new(URL_SCHEME, "http"),
        ]);
        assert_eq!(attributes, expected_attributes);
    }
}
