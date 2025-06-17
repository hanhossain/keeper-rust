use crate::PKG_NAME;
use axum::extract::{MatchedPath, Request};
use axum::http::{Method, StatusCode, Version};
use axum::response::Response;
use futures_util::future::BoxFuture;
use opentelemetry::metrics::{Histogram, Meter, MeterProvider, UpDownCounter};
use opentelemetry::{KeyValue, global};
use opentelemetry_semantic_conventions::attribute::{
    HTTP_REQUEST_METHOD, HTTP_RESPONSE_STATUS_CODE, HTTP_ROUTE, NETWORK_PROTOCOL_VERSION,
    URL_SCHEME,
};
use opentelemetry_semantic_conventions::metric::{
    HTTP_SERVER_ACTIVE_REQUESTS, HTTP_SERVER_REQUEST_DURATION,
};
use std::task::{Context, Poll};
use std::time::Instant;
use tower::{Layer, Service};

#[derive(Clone)]
struct RequestMetrics {
    request_duration: Histogram<f64>,
    active_requests: UpDownCounter<i64>,
}

impl RequestMetrics {
    fn new(meter: Meter) -> Self {
        let request_duration = meter
            .f64_histogram(HTTP_SERVER_REQUEST_DURATION)
            .with_description("Duration of HTTP server requests.")
            .with_unit("s")
            .with_boundaries(vec![
                0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
            ])
            .build();
        let active_requests = meter
            .i64_up_down_counter(HTTP_SERVER_ACTIVE_REQUESTS)
            .with_description("Number of active HTTP server requests.")
            .with_unit("{request}")
            .build();

        RequestMetrics {
            request_duration,
            active_requests,
        }
    }
}

#[derive(Clone)]
pub struct RequestMetricsLayer {
    metrics: RequestMetrics,
}

impl RequestMetricsLayer {
    pub fn new() -> RequestMetricsLayer {
        let meter = global::meter(PKG_NAME);
        let metrics = RequestMetrics::new(meter);
        RequestMetricsLayer { metrics }
    }

    pub fn new_with_provider<P>(meter_provider: P) -> RequestMetricsLayer
    where
        P: MeterProvider,
    {
        let meter = meter_provider.meter(PKG_NAME);
        let metrics = RequestMetrics::new(meter);
        RequestMetricsLayer { metrics }
    }
}

impl<S> Layer<S> for RequestMetricsLayer {
    type Service = RequestMetricsMiddleware<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequestMetricsMiddleware {
            inner,
            metrics: self.metrics.clone(),
        }
    }
}

#[derive(Clone)]
pub struct RequestMetricsMiddleware<S> {
    inner: S,
    metrics: RequestMetrics,
}

impl<S, B> Service<Request> for RequestMetricsMiddleware<S>
where
    S: Service<Request, Response = Response<B>> + Send + 'static,
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
        let method = request.method();
        let scheme = request.uri().scheme_str().unwrap_or("http");

        let active_requests_attributes = AttributeBuilder::builder()
            .with_method(method)
            .with_scheme(scheme)
            .build();
        self.metrics
            .active_requests
            .add(1, &active_requests_attributes);

        let route = request
            .extensions()
            .get::<MatchedPath>()
            .map(|p| p.as_str());

        let request_duration_attributes = AttributeBuilder::builder()
            .with_method(method)
            .with_scheme(scheme)
            .with_version(request.version())
            .with_route(route);

        let metrics = self.metrics.clone();
        let future = self.inner.call(request);
        Box::pin(async move {
            let response = future.await?;

            let request_duration_attributes = request_duration_attributes
                .with_status(response.status())
                .build();

            metrics.request_duration.record(
                start_time.elapsed().as_secs_f64(),
                &request_duration_attributes,
            );

            metrics.active_requests.add(-1, &active_requests_attributes);
            Ok(response)
        })
    }
}

#[derive(Default)]
struct AttributeBuilder {
    method: Option<KeyValue>,
    scheme: Option<KeyValue>,
    version: Option<KeyValue>,
    route: Option<KeyValue>,
    status: Option<KeyValue>,
}

impl AttributeBuilder {
    fn builder() -> Self {
        Default::default()
    }

    fn with_method(mut self, method: &Method) -> Self {
        self.method = Some(KeyValue::new(HTTP_REQUEST_METHOD, method.to_string()));
        self
    }

    fn with_scheme(mut self, scheme: &str) -> Self {
        self.scheme = Some(KeyValue::new(URL_SCHEME, scheme.to_string()));
        self
    }

    fn with_version(mut self, version: Version) -> Self {
        // TODO: consider trimming HTTP/ from the version
        self.version = Some(KeyValue::new(
            NETWORK_PROTOCOL_VERSION,
            format!("{:?}", version),
        ));
        self
    }

    fn with_route(mut self, route: Option<&str>) -> Self {
        if let Some(route) = route {
            self.route = Some(KeyValue::new(HTTP_ROUTE, route.to_owned()));
        }
        self
    }

    fn with_status(mut self, status: StatusCode) -> Self {
        self.status = Some(KeyValue::new(
            HTTP_RESPONSE_STATUS_CODE,
            status.as_u16() as i64,
        ));
        self
    }

    fn build(self) -> Vec<KeyValue> {
        [
            self.method,
            self.scheme,
            self.version,
            self.route,
            self.status,
        ]
        .into_iter()
        .filter_map(|x| x)
        .collect()
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
    use pretty_assertions::assert_eq;
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
            .layer(RequestMetricsLayer::new_with_provider(provider.clone()));

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

    #[tokio::test]
    async fn active_requests() {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_periodic_exporter(exporter.clone())
            .build();
        let provider2 = provider.clone();

        let app = Router::new()
            .route(
                "/",
                get(|| async move {
                    provider2.force_flush().unwrap();
                }),
            )
            .layer(RequestMetricsLayer::new_with_provider(provider.clone()));

        let _ = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        provider.force_flush().unwrap();
        let resource_metrics = exporter.get_finished_metrics().unwrap();
        dbg!(&resource_metrics);
        assert_eq!(resource_metrics.len(), 2);

        let mut is_active = true;

        for resource_metric in resource_metrics {
            let scope_metrics = resource_metric.scope_metrics().next().unwrap();
            let metrics: HashMap<_, _> = scope_metrics.metrics().map(|m| (m.name(), m)).collect();

            let metric = metrics[HTTP_SERVER_ACTIVE_REQUESTS];
            let metric_data = match metric.data() {
                AggregatedMetrics::I64(MetricData::Sum(x)) => x,
                _ => panic!("wrong metric data type"),
            };

            let data_point = metric_data.data_points().next().unwrap();
            assert_eq!(data_point.value(), if is_active { 1 } else { 0 });

            let attributes: HashSet<_> = data_point.attributes().cloned().collect();
            let expected_attributes = HashSet::from([
                KeyValue::new(HTTP_REQUEST_METHOD, "GET"),
                KeyValue::new(URL_SCHEME, "http"),
            ]);
            assert_eq!(attributes, expected_attributes);
            is_active = false;
        }
    }

    #[tokio::test]
    async fn simple_global() {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_periodic_exporter(exporter.clone())
            .build();
        global::set_meter_provider(provider.clone());

        let app = Router::new()
            .route("/", get(|| async {}))
            .layer(RequestMetricsLayer::new());

        let _ = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        provider.force_flush().unwrap();
        let resource_metrics = exporter.get_finished_metrics().unwrap();
        assert_eq!(resource_metrics.len(), 1);

        let scope_metrics: Vec<_> = resource_metrics[0].scope_metrics().collect();
        assert_eq!(scope_metrics.len(), 1);
        assert_eq!(scope_metrics[0].scope().name(), PKG_NAME);

        let metrics: HashSet<_> = scope_metrics[0].metrics().map(|m| m.name()).collect();
        assert_eq!(
            metrics,
            HashSet::from([HTTP_SERVER_REQUEST_DURATION, HTTP_SERVER_ACTIVE_REQUESTS])
        );
    }

    #[tokio::test]
    async fn two_requests_in_one_export() {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_periodic_exporter(exporter.clone())
            .build();

        let mut app = Router::new()
            .route("/", get(|| async {}))
            .layer(RequestMetricsLayer::new_with_provider(provider.clone()));

        let request = Request::builder().uri("/").body(Body::empty()).unwrap();
        let _ = ServiceExt::<Request>::ready(&mut app)
            .await
            .unwrap()
            .call(request)
            .await
            .unwrap();

        let request = Request::builder().uri("/").body(Body::empty()).unwrap();
        let _ = ServiceExt::<Request>::ready(&mut app)
            .await
            .unwrap()
            .call(request)
            .await
            .unwrap();

        provider.force_flush().unwrap();
        let resource_metrics = exporter.get_finished_metrics().unwrap();
        assert_eq!(resource_metrics.len(), 1);

        let scope_metrics: Vec<_> = resource_metrics[0].scope_metrics().collect();
        assert_eq!(scope_metrics.len(), 1);

        let metrics: HashMap<_, _> = scope_metrics[0].metrics().map(|m| (m.name(), m)).collect();

        let metric = metrics[HTTP_SERVER_REQUEST_DURATION];
        let metric_data = match metric.data() {
            AggregatedMetrics::F64(MetricData::Histogram(x)) => x,
            _ => panic!("wrong metric data type"),
        };
        let data_point = metric_data.data_points().next().unwrap();
        assert_eq!(data_point.count(), 2);
    }
}
