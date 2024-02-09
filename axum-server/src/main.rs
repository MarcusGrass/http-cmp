use axum::extract::State;
use axum::routing::{get, post, put};
use axum::Json;
use http_test_util::{
    GetCounterResponse, IncrementCounterRequest, IncrementCounterResponse, SharedCounter,
    SwapCounterRequest, SwapCounterResponse,
};

const INDEX_HTML: &[u8] = include_bytes!("../../index.html");

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let _g = rt.enter();
    rt.block_on(run_server());
}

async fn run_server() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8080")
        .await
        .unwrap();
    let state = SharedCounter::new();
    let router = axum::Router::new()
        .route("/", get(get_index))
        .route("/count", get(get_count))
        .route("/count", put(put_count))
        .route("/count", post(post_count))
        .with_state(state);

    axum::serve(listener, router).await.unwrap()
}

#[inline]
async fn get_index() -> &'static [u8] {
    INDEX_HTML
}

#[inline]
async fn get_count(State(counter): State<SharedCounter>) -> Json<GetCounterResponse> {
    Json(counter.get())
}

#[inline]
async fn put_count(
    State(counter): State<SharedCounter>,
    Json(incr): Json<IncrementCounterRequest>,
) -> Json<IncrementCounterResponse> {
    Json(counter.increment(incr.new))
}

#[inline]
async fn post_count(
    State(counter): State<SharedCounter>,
    Json(incr): Json<SwapCounterRequest>,
) -> Json<SwapCounterResponse> {
    Json(counter.swap(incr.new))
}
