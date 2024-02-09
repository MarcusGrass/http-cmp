use std::future::Future;
use std::time::{Duration, SystemTime};
use anyhow::{bail, Context};
use hyper::{Method, Request};
use hyper::header::CONTENT_TYPE;
use http_test_util::{byte_body, empty_body, GetCounterResponse, IncrementCounterRequest, IncrementCounterResponse, SwapCounterRequest, SwapCounterResponse};
use crate::client::HttpClient;
use crate::statistics::SingleRunStatistics;

const INDEX_HTML: &[u8] = include_bytes!("../../index.html");

pub(crate) async fn run(num_concurrent: usize, tests_per_task: usize, client: HttpClient, base_uri: &'static str) -> anyhow::Result<()> {
    let mut tasks = Vec::with_capacity(num_concurrent);
    for _ in 0..num_concurrent {
        tasks.push(tokio::spawn(run_sequential(tests_per_task, client.clone(), base_uri)));
    }
    let mut results = Vec::with_capacity(num_concurrent * tests_per_task);
    for t in tasks {
        let res = t.await.context("Failed to join task")??;
        results.extend(res);
    }
    let mut min_get_index_rtt = u128::MAX;
    let mut max_get_index_rtt = u128::MIN;
    let mut total_get_index = 0;

    let mut min_get_count_rtt = u128::MAX;
    let mut max_get_count_rtt = u128::MIN;
    let mut total_get_count = 0;

    let mut min_put_count_rtt = u128::MAX;
    let mut max_put_count_rtt = u128::MIN;
    let mut total_put_count = 0;

    let mut min_post_count_rtt = u128::MAX;
    let mut max_post_count_rtt = u128::MIN;
    let mut total_post_count = 0;

    let mut min_total_rtt = u128::MAX;
    let mut max_total_count_rtt = u128::MIN;
    let mut total_total_count = 0;

    let len = results.len();
    for res in results.iter() {
        update_stats(res.get_index_rtt.as_micros(), &mut min_get_index_rtt, &mut max_get_index_rtt, &mut total_get_index);
        update_stats(res.get_count_rtt.as_micros(), &mut min_get_count_rtt, &mut max_get_count_rtt, &mut total_get_count);
        update_stats(res.put_count_rtt.as_micros(), &mut min_put_count_rtt, &mut max_put_count_rtt, &mut total_put_count);
        update_stats(res.post_count_rtt.as_micros(), &mut min_post_count_rtt, &mut max_post_count_rtt, &mut total_post_count);
        update_stats(res.total().as_micros(), &mut min_total_rtt, &mut max_total_count_rtt, &mut total_total_count);
    }
    let mean_get_index = total_get_index as f64 / len as f64;
    let mean_get_count = total_get_count as f64 / len as f64;
    let mean_put_count = total_put_count as f64 / len as f64;
    let mean_post_count = total_post_count as f64 / len as f64;
    let mean_total_count = total_total_count as f64 / len as f64;
    println!("\
Results:
    get_index_rtt my s  [min, mean, max] = [{}, {:.2}, {}]
    get_count_rtt my s  [min, mean, max] = [{}, {:.2}, {}]
    put_count_rtt my s  [min, mean, max] = [{}, {:.2}, {}]
    post_count_rtt my s [min, mean, max] = [{}, {:.2}, {}]
    total_rtt my s      [min, mean, max] = [{}, {:.2}, {}]
    ", min_get_index_rtt, mean_get_index, max_get_index_rtt,
        min_get_count_rtt, mean_get_count, max_get_count_rtt,
        min_put_count_rtt, mean_put_count, max_put_count_rtt,
        min_post_count_rtt, mean_post_count, max_post_count_rtt,
        min_total_rtt, mean_total_count, max_total_count_rtt
    );
    Ok(())
}

fn update_stats(cur: u128, min: &mut u128, max: &mut u128, mean: &mut u128) {
    if cur < *min {
        *min = cur;
    }
    if cur > *max {
        *max = cur;
    }
    *mean += cur;
}

pub(crate) async fn run_sequential(num_tests: usize, client: HttpClient, base_uri: &'static str) -> anyhow::Result<Vec<SingleRunStatistics>> {
    let mut v = Vec::with_capacity(num_tests);
    for _ in 0..num_tests {
        v.push(run_full(client.clone(), base_uri).await?)
    }
    Ok(v)
}

pub(crate) async fn run_full(mut client: HttpClient, base_uri: &'static str) -> anyhow::Result<SingleRunStatistics> {
    let get = Request::get(base_uri)
        .method(Method::GET)
        .body(empty_body())
        .context("Failed to build get")?;
    let (get_index_rtt, resp)  = run_timed(client.send_recv(get)).await;
    let resp = resp
        .context("Get index failed")?;
    if INDEX_HTML != &resp {
        bail!("Got unexpected index html response");
    }
    let count_uri = format!("{base_uri}/count");
    let get_count = Request::get(&count_uri)
        .method(Method::GET)
        .body(empty_body())
        .context("Failed to build get count")?;
    let (get_count_rtt, resp) = run_timed(client.send_recv(get_count)).await;
    let resp = resp.context("Failed to get count")?;
    let _resp: GetCounterResponse = serde_json::from_slice(&resp).context("Failed to serialize get count")?;

    let put_body = serde_json::to_vec(&IncrementCounterRequest::new(15)).context("Failed to create put request")?;
    let put_count = Request::put(&count_uri)
        .method(Method::PUT)
        .header(CONTENT_TYPE, "application/json")
        .body(byte_body(put_body))
        .context("Failed to build put request")?;
    let (put_count_rtt, resp) = run_timed(client.send_recv(put_count)).await;
    let resp = resp.context("Failed to put count")?;
    let _resp: IncrementCounterResponse = serde_json::from_slice(&resp).unwrap();

    let post_body = serde_json::to_vec(&SwapCounterRequest::new(155)).context("Failed to create post request")?;
    let post_count = Request::put(&count_uri)
        .method(Method::POST)
        .header(CONTENT_TYPE, "application/json")
        .body(byte_body(post_body))
        .context("Failed to build put request")?;
    let (post_count_rtt, resp) = run_timed(client.send_recv(post_count)).await;
    let resp = resp.context("Failed to post count")?;
    let _resp: SwapCounterResponse = serde_json::from_slice(&resp).unwrap();

    Ok(SingleRunStatistics {
        get_index_rtt,
        get_count_rtt,
        put_count_rtt,
        post_count_rtt,
    })
}

#[inline]
async fn run_timed<T, F: Future<Output=T>>(fut: F) -> (Duration, T) {
    let start = SystemTime::now();
    let res = fut.await;
    (start.elapsed().unwrap(), res)
}