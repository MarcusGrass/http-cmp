mod uring_compat;
mod hyper_tokio_compat;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::str::FromStr;
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Body;
use hyper::{Method, Request, Response, StatusCode};
use hyper::service::service_fn;
use mimalloc::MiMalloc;
use http_test_util::{byte_body, empty_body, IncrementCounterRequest, SharedCounter, SwapCounterRequest};
use http_test_util::drain::DrainBodyFuture;
use crate::hyper_tokio_compat::TokioUringIo;
use crate::uring_compat::UringTcp;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

const INDEX_HTML: &[u8] = include_bytes!("../../index.html");

fn main() {
    let mut ub = tokio_uring::uring_builder();
    ub
        .setup_single_issuer();
    tokio_uring::builder()
        .uring_builder(&ub)
        .entries(256)
        .start(run_app());
}

async fn my_service<B: Body>(shared_counter: SharedCounter, incoming: Request<B>) -> Result<Response<Full<Bytes>>, Infallible> {
    let path = incoming.uri().path();
    match path {
        "" | "/" => {
            if incoming.method() == Method::GET {
                Ok(Response::new(byte_body(INDEX_HTML)))
            } else {
                Ok(Response::builder().status(StatusCode::METHOD_NOT_ALLOWED).body(empty_body()).unwrap())
            }
        }
        "/count" => {
            if incoming.method() == Method::GET {
                let payload = serde_json::to_vec(&shared_counter.get()).unwrap();
                return Ok(Response::builder()
                    .status(StatusCode::OK)
                    .body(byte_body(payload)).unwrap())
            }
            if incoming.method() == Method::PUT {
                let body = incoming.into_body();
                let body = DrainBodyFuture::new_trusted_length(body, 512)
                    .await.unwrap();
                let req_raw: IncrementCounterRequest = serde_json::from_slice(&body).unwrap();
                let resp = shared_counter.increment(req_raw.new);
                let resp_raw = serde_json::to_vec(&resp).unwrap();
                return Ok(Response::builder().status(StatusCode::OK).body(byte_body(resp_raw)).unwrap());
            }
            if incoming.method() == Method::POST {
                let body = incoming.into_body();
                let body = DrainBodyFuture::new_trusted_length(body, 512)
                    .await.unwrap();
                let req_raw: SwapCounterRequest = serde_json::from_slice(&body).unwrap();
                let resp = shared_counter.swap(req_raw.new);
                let resp_raw = serde_json::to_vec(&resp).unwrap();
                return Ok(Response::builder().status(StatusCode::OK).body(byte_body(resp_raw)).unwrap());
            }
            Ok(Response::builder().status(StatusCode::METHOD_NOT_ALLOWED).body(empty_body()).unwrap())
        }
        _ => {
            Ok(Response::builder().status(StatusCode::NOT_FOUND).body(Full::new(Bytes::new())).unwrap())
        }
    }
}


async fn run_app() {
    let addr = "127.0.0.1:8080";
    let sock = SocketAddr::from_str(addr).unwrap();
    let sock = tokio_uring::net::TcpListener::bind(sock).unwrap();
    let shared_count = SharedCounter::new();
    loop {
        let (tcp, _peer) = sock.accept().await.unwrap();
        let uring_tcp = TokioUringIo::new(UringTcp::new(tcp, 1024 * 256));
        let sc = shared_count.clone();
        tokio_uring::spawn(hyper::server::conn::http1::Builder::new()
            .serve_connection(uring_tcp, service_fn(move |req| {
                my_service(sc.clone(), req)
            })));

    }

}