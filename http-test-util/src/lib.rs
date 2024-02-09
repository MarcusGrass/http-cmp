pub mod drain;

use std::sync::{Arc};
use std::sync::atomic::{AtomicUsize, Ordering};
use bytes::Bytes;
use http_body_util::Full;

#[inline]
pub fn empty_body() -> Full<Bytes> {
    Full::new(Bytes::new())
}

#[inline]
pub fn byte_body<B: Into<Bytes>>(bytes: B) -> Full<Bytes> {
    Full::new(bytes.into())
}


#[derive(Clone)]
pub struct SharedCounter {
    count: Arc<AtomicUsize>,
}

impl SharedCounter {
    #[must_use]
    pub fn new() -> Self {
        Self {
            count: Arc::new(AtomicUsize::new(0)),
        }
    }

    #[inline]
    #[must_use]
    pub fn increment(&self, num: usize) -> IncrementCounterResponse {
        let prev = self.count.fetch_add(num, Ordering::AcqRel);
        IncrementCounterResponse {
            prev,
        }
    }

    #[inline]
    #[must_use]
    pub fn swap(&self, cur: usize) -> SwapCounterResponse {
        let prev = self.count.swap(cur, Ordering::AcqRel);
        SwapCounterResponse {
            prev,
            cur,
        }
    }

    #[inline]
    #[must_use]
    pub fn get(&self) -> GetCounterResponse {
        GetCounterResponse {
            cur: self.count.load(Ordering::Acquire),
        }
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct IncrementCounterRequest {
    pub new: usize
}

impl IncrementCounterRequest {
    #[must_use]
    pub fn new(new: usize) -> Self {
        Self { new }
    }
}


#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct IncrementCounterResponse {
    prev: usize,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct SwapCounterRequest {
    pub new: usize
}

impl SwapCounterRequest {
    #[must_use]
    pub fn new(new: usize) -> Self {
        Self { new }
    }
}


#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct SwapCounterResponse {
    prev: usize,
    cur: usize,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct GetCounterResponse {
    cur: usize,
}
