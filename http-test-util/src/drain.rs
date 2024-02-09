use bytes::Buf;
use hyper::body::Body;
use pin_project_lite::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

pin_project! {
    pub struct DrainBodyFuture<B: Body> {
        #[pin]
        body: B,
        #[pin]
        buf: Vec<u8>
    }
}

impl<B> DrainBodyFuture<B>
where
    B: Body,
{
    #[inline]
    #[must_use]
    pub fn new_trusted_length(body: B, content_length: usize) -> Self {
        Self {
            body,
            buf: Vec::with_capacity(content_length),
        }
    }
}

impl<B> Future for DrainBodyFuture<B>
where
    B: Body,
{
    type Output = Result<Vec<u8>, anyhow::Error>;

    #[allow(clippy::similar_names)]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut slf = self.project();
        match slf.body.as_mut().poll_frame(cx) {
            Poll::Ready(data) => {
                let Some(next_res) = data else {
                    return Poll::Ready(Ok(std::mem::take(&mut slf.buf)));
                };
                let next_frame = match next_res {
                    Ok(frame) => frame,
                    Err(_e) => {
                        return Poll::Ready(Err(anyhow::anyhow!("Failed to poll next frame")));
                    }
                };
                let data = match next_frame.into_data() {
                    Ok(d) => d,
                    Err(_e) => {
                        return Poll::Ready(Err(anyhow::anyhow!(
                            "Failed to get data from frame, not a data frame"
                        )));
                    }
                };
                let chunk = data.chunk();
                slf.buf.extend_from_slice(chunk);
                if slf.body.is_end_stream() {
                    Poll::Ready(Ok(std::mem::take(&mut slf.buf)))
                } else {
                    Poll::Pending
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
