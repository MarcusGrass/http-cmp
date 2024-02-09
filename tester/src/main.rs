use crate::client::HttpClient;
use crate::scenario::{run};

pub mod client;
mod scenario;
mod statistics;

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let _guard = rt.enter();
    rt.block_on(run_tester(16, 10_000));
}

async fn run_tester(num_concurrent: usize, num_per_task: usize) {
    let client = HttpClient::new();
    run(num_concurrent, num_per_task, client, "http://127.0.0.1:8080").await.unwrap();
}
