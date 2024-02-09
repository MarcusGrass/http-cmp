use anyhow::{Context, Result};
use bytes::Bytes;
use http_body_util::Full;
use http_test_util::drain::DrainBodyFuture;
use hyper::header::CONTENT_LENGTH;
use hyper::Request;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

#[derive(Clone)]
pub struct HttpClient {
    client: Client<HttpConnector, Full<Bytes>>,
}

impl HttpClient {
    #[must_use]
    pub fn new() -> Self {
        let client = Client::builder(TokioExecutor::new()).build(HttpConnector::new());
        Self { client }
    }

    pub async fn send_recv(&mut self, request: Request<Full<Bytes>>) -> Result<Vec<u8>> {
        let resp = self
            .client
            .request(request)
            .await
            .context("Failed to send request")?;
        let content_length: usize = resp
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|hv| hv.to_str().ok())
            .and_then(|hv| hv.parse().ok())
            .unwrap_or(1024);
        let bytes: Vec<u8> =
            DrainBodyFuture::new_trusted_length(resp.into_body(), content_length).await?;
        Ok(bytes)
    }
}
