use crate::PKG_NAME;
use axum::http::Extensions;
use opentelemetry::metrics::{Histogram, Meter, MeterProvider};
use opentelemetry::{KeyValue, global};
use opentelemetry_semantic_conventions::attribute::{
    HTTP_REQUEST_METHOD, HTTP_RESPONSE_STATUS_CODE, SERVER_ADDRESS, SERVER_PORT,
};
use opentelemetry_semantic_conventions::metric::HTTP_CLIENT_REQUEST_DURATION;
use opentelemetry_semantic_conventions::trace::ERROR_TYPE;
use reqwest_middleware::reqwest::{Request, Response};
use reqwest_middleware::{Middleware, Next};
use std::time::Instant;

struct ReqwestMetrics {
    request_duration: Histogram<f64>,
}

impl ReqwestMetrics {
    fn new(meter: Meter) -> Self {
        let request_duration = meter
            .f64_histogram(HTTP_CLIENT_REQUEST_DURATION)
            .with_description("Duration of HTTP client requests.")
            .with_unit("s")
            .with_boundaries(vec![
                0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
            ])
            .build();
        Self { request_duration }
    }
}

pub struct ReqwestMetricsMiddleware {
    metrics: ReqwestMetrics,
}

impl ReqwestMetricsMiddleware {
    pub fn new() -> Self {
        let meter = global::meter(PKG_NAME);
        let metrics = ReqwestMetrics::new(meter);
        Self { metrics }
    }

    pub fn new_with_provider<P>(meter_provider: P) -> Self
    where
        P: MeterProvider,
    {
        let meter = meter_provider.meter(PKG_NAME);
        let metrics = ReqwestMetrics::new(meter);
        Self { metrics }
    }
}

#[async_trait::async_trait]
impl Middleware for ReqwestMetricsMiddleware {
    async fn handle(
        &self,
        req: Request,
        extensions: &mut Extensions,
        next: Next<'_>,
    ) -> reqwest_middleware::Result<Response> {
        let start_time = Instant::now();

        let method = req.method().to_string();
        let host = req.url().host_str().unwrap().to_string();
        let port = req.url().port();

        let mut attributes = vec![
            KeyValue::new(HTTP_REQUEST_METHOD, method),
            KeyValue::new(SERVER_ADDRESS, host),
        ];

        if let Some(port) = port {
            attributes.push(KeyValue::new(SERVER_PORT, port as i64));
        }

        let res = next.run(req, extensions).await;

        match &res {
            Ok(response) => {
                let status = response.status();
                attributes.push(KeyValue::new(
                    HTTP_RESPONSE_STATUS_CODE,
                    status.as_u16() as i64,
                ));

                if status.is_client_error() || status.is_server_error() {
                    attributes.push(KeyValue::new(ERROR_TYPE, status.as_str().to_string()));
                }
            }
            Err(error) => {
                attributes.push(KeyValue::new(ERROR_TYPE, error.to_string()));
            }
        }

        self.metrics
            .request_duration
            .record(start_time.elapsed().as_secs_f64(), &attributes);

        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
    use reqwest_middleware::ClientBuilder;
    use reqwest_middleware::reqwest::Client;
    use reqwest_tracing::HTTP_RESPONSE_STATUS_CODE;
    use std::collections::{HashMap, HashSet};
    use std::error::Error;

    #[tokio::test]
    async fn request_duration() {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_periodic_exporter(exporter.clone())
            .build();

        let mut server = mockito::Server::new_async().await;
        let server_mock = server
            .mock("GET", "/hello")
            .with_body("world")
            .create_async()
            .await;

        let client = ClientBuilder::new(Client::new())
            .with(ReqwestMetricsMiddleware::new_with_provider(
                provider.clone(),
            ))
            .build();

        let _ = client
            .get(format!("{}/hello", server.url()))
            .send()
            .await
            .unwrap();

        server_mock.assert_async().await;

        provider.force_flush().unwrap();
        let resource_metrics = exporter.get_finished_metrics().unwrap();
        assert_eq!(resource_metrics.len(), 1);

        let scope_metrics = resource_metrics[0].scope_metrics().next().unwrap();
        assert_eq!(scope_metrics.scope().name(), PKG_NAME);

        let metrics: HashMap<_, _> = scope_metrics.metrics().map(|m| (m.name(), m)).collect();
        let metric = metrics[HTTP_CLIENT_REQUEST_DURATION];

        let AggregatedMetrics::F64(MetricData::Histogram(metric_data)) = metric.data() else {
            panic!("wrong metric data type");
        };
        let data_point = metric_data.data_points().next().unwrap();
        assert_eq!(data_point.count(), 1);

        let attributes: HashSet<_> = data_point.attributes().cloned().collect();
        let expected_attributes = HashSet::from([
            KeyValue::new(HTTP_REQUEST_METHOD, "GET"),
            KeyValue::new(HTTP_RESPONSE_STATUS_CODE, 200),
            KeyValue::new(SERVER_ADDRESS, "127.0.0.1"),
            KeyValue::new(SERVER_PORT, server.socket_address().port() as i64),
        ]);
        assert_eq!(attributes, expected_attributes);
    }

    #[tokio::test]
    async fn request_duration_status_400() {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_periodic_exporter(exporter.clone())
            .build();

        let mut server = mockito::Server::new_async().await;
        let server_mock = server
            .mock("GET", "/hello")
            .with_status(400)
            .with_body("world")
            .create_async()
            .await;

        let client = ClientBuilder::new(Client::new())
            .with(ReqwestMetricsMiddleware::new_with_provider(
                provider.clone(),
            ))
            .build();

        let _ = client
            .get(format!("{}/hello", server.url()))
            .send()
            .await
            .unwrap();

        server_mock.assert_async().await;

        provider.force_flush().unwrap();
        let resource_metrics = exporter.get_finished_metrics().unwrap();
        assert_eq!(resource_metrics.len(), 1);

        let scope_metrics = resource_metrics[0].scope_metrics().next().unwrap();
        assert_eq!(scope_metrics.scope().name(), PKG_NAME);

        let metrics: HashMap<_, _> = scope_metrics.metrics().map(|m| (m.name(), m)).collect();
        let metric = metrics[HTTP_CLIENT_REQUEST_DURATION];

        let AggregatedMetrics::F64(MetricData::Histogram(metric_data)) = metric.data() else {
            panic!("wrong metric data type");
        };
        let data_point = metric_data.data_points().next().unwrap();
        assert_eq!(data_point.count(), 1);

        let attributes: HashSet<_> = data_point.attributes().cloned().collect();
        let expected_attributes = HashSet::from([
            KeyValue::new(HTTP_REQUEST_METHOD, "GET"),
            KeyValue::new(HTTP_RESPONSE_STATUS_CODE, 400),
            KeyValue::new(SERVER_ADDRESS, "127.0.0.1"),
            KeyValue::new(SERVER_PORT, server.socket_address().port() as i64),
            KeyValue::new(ERROR_TYPE, "400"),
        ]);
        assert_eq!(attributes, expected_attributes);
    }

    #[tokio::test]
    async fn request_duration_status_500() {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_periodic_exporter(exporter.clone())
            .build();

        let mut server = mockito::Server::new_async().await;
        let server_mock = server
            .mock("GET", "/hello")
            .with_status(500)
            .with_body("world")
            .create_async()
            .await;

        let client = ClientBuilder::new(Client::new())
            .with(ReqwestMetricsMiddleware::new_with_provider(
                provider.clone(),
            ))
            .build();

        let _ = client
            .get(format!("{}/hello", server.url()))
            .send()
            .await
            .unwrap();

        server_mock.assert_async().await;

        provider.force_flush().unwrap();
        let resource_metrics = exporter.get_finished_metrics().unwrap();
        assert_eq!(resource_metrics.len(), 1);

        let scope_metrics = resource_metrics[0].scope_metrics().next().unwrap();
        assert_eq!(scope_metrics.scope().name(), PKG_NAME);

        let metrics: HashMap<_, _> = scope_metrics.metrics().map(|m| (m.name(), m)).collect();
        let metric = metrics[HTTP_CLIENT_REQUEST_DURATION];

        let AggregatedMetrics::F64(MetricData::Histogram(metric_data)) = metric.data() else {
            panic!("wrong metric data type");
        };
        let data_point = metric_data.data_points().next().unwrap();
        assert_eq!(data_point.count(), 1);

        let attributes: HashSet<_> = data_point.attributes().cloned().collect();
        let expected_attributes = HashSet::from([
            KeyValue::new(HTTP_REQUEST_METHOD, "GET"),
            KeyValue::new(HTTP_RESPONSE_STATUS_CODE, 500),
            KeyValue::new(SERVER_ADDRESS, "127.0.0.1"),
            KeyValue::new(SERVER_PORT, server.socket_address().port() as i64),
            KeyValue::new(ERROR_TYPE, "500"),
        ]);
        assert_eq!(attributes, expected_attributes);
    }

    #[tokio::test]
    async fn request_duration_middleware_failure() {
        #[derive(Debug)]
        struct FailingMiddlewareError;

        impl std::fmt::Display for FailingMiddlewareError {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("FailingMiddlewareErrorDisplay")
            }
        }

        impl Error for FailingMiddlewareError {}

        struct FailingMiddleware;

        #[async_trait::async_trait]
        impl Middleware for FailingMiddleware {
            async fn handle(
                &self,
                _req: Request,
                _extensions: &mut Extensions,
                _next: Next<'_>,
            ) -> reqwest_middleware::Result<Response> {
                Err(reqwest_middleware::Error::middleware(
                    FailingMiddlewareError,
                ))
            }
        }

        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_periodic_exporter(exporter.clone())
            .build();

        let server = mockito::Server::new_async().await;

        let client = ClientBuilder::new(Client::new())
            .with(ReqwestMetricsMiddleware::new_with_provider(
                provider.clone(),
            ))
            .with(FailingMiddleware)
            .build();

        let _ = client
            .get(format!("{}/hello", server.url()))
            .send()
            .await
            .unwrap_err();

        provider.force_flush().unwrap();
        let resource_metrics = exporter.get_finished_metrics().unwrap();
        assert_eq!(resource_metrics.len(), 1);

        let scope_metrics = resource_metrics[0].scope_metrics().next().unwrap();
        assert_eq!(scope_metrics.scope().name(), PKG_NAME);

        let metrics: HashMap<_, _> = scope_metrics.metrics().map(|m| (m.name(), m)).collect();
        let metric = metrics[HTTP_CLIENT_REQUEST_DURATION];

        let AggregatedMetrics::F64(MetricData::Histogram(metric_data)) = metric.data() else {
            panic!("wrong metric data type");
        };
        let data_point = metric_data.data_points().next().unwrap();
        assert_eq!(data_point.count(), 1);

        let attributes: HashSet<_> = data_point.attributes().cloned().collect();
        let expected_attributes = HashSet::from([
            KeyValue::new(HTTP_REQUEST_METHOD, "GET"),
            KeyValue::new(SERVER_ADDRESS, "127.0.0.1"),
            KeyValue::new(SERVER_PORT, server.socket_address().port() as i64),
            KeyValue::new(ERROR_TYPE, "FailingMiddlewareErrorDisplay"),
        ]);
        assert_eq!(attributes, expected_attributes);
    }
}
