use bytes::BytesMut;
use hyper::rt::Executor;
use pin_project_lite::pin_project;
use std::future::Future;
use std::io::Error;
use std::net::Shutdown;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

#[derive(Copy, Clone)]
pub struct TokioUringExecutor;

impl<Fut> Executor<Fut> for TokioUringExecutor
    where
        Fut: Future + Send + 'static,
        Fut::Output: Send + 'static,
{
    #[inline]
    fn execute(&self, fut: Fut) {
        tokio_uring::spawn(fut);
    }
}

pin_project! {
    pub struct UringTcp {
        inner_tcp: Rc<tokio_uring::net::TcpStream>,
        read_buf: Option<BufferedOutgoingBytes>,
        #[pin]
        read_fut: Option<Pin<Box<dyn Future<Output=Result<BufferedOutgoingBytes, std::io::Error>>>>>,
        write_buf: Option<BufferedBytesMut>,
        #[pin]
        write_fut: Option<Pin<Box<dyn Future<Output=Result<BufferedBytesMut, std::io::Error>>>>>,
    }
}

impl UringTcp {
    #[must_use]
    pub fn new(inner_tcp: tokio_uring::net::TcpStream, buf_size: usize) -> Self {
        Self {
            inner_tcp: Rc::new(inner_tcp),
            read_buf: Some(BufferedOutgoingBytes {
                bytes: BytesMut::zeroed(buf_size),
                unread_range: None,
            }),
            read_fut: None,
            write_buf: Some(BufferedBytesMut {
                bytes: BytesMut::zeroed(buf_size),
                unread_range: None,
            }),
            write_fut: None,
        }
    }
}

#[derive(Debug)]
struct BufferedOutgoingBytes {
    bytes: BytesMut,
    unread_range: Option<UnreadRange>,
}

impl BufferedOutgoingBytes {
    #[inline]
    fn unwritten(&self) -> bool {
        self.unread_range
            .as_ref()
            .map(|r| r.count > 0)
            .unwrap_or_default()
    }
}

struct BufferedBytesMut {
    bytes: BytesMut,
    unread_range: Option<UnreadRange>,
}

#[derive(Copy, Clone, Debug)]
struct UnreadRange {
    start: usize,
    count: usize,
}

impl tokio::io::AsyncRead for UringTcp {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<Result<(), Error>> {
        match (self.write_buf.is_some(), self.write_fut.is_some()) {
            (true, false) => {
                if try_write_into_cursor(self.write_buf.as_mut().unwrap(), buf) {
                    return Poll::Ready(Ok(()));
                }
                let b = self.write_buf.take().unwrap();
                self.write_fut = Some(Box::pin(uring_read_into(self.inner_tcp.clone(), b)));
                let mut slf = self.project();
                let mut fut = slf.write_fut.take().unwrap();
                let pinned = std::pin::pin!(&mut fut);
                match pinned.poll(cx) {
                    Poll::Ready(res) => match res {
                        Ok(mut buffered) => {
                            try_write_into_cursor(&mut buffered, buf);
                            *slf.write_buf = Some(buffered);
                            Poll::Ready(Ok(()))
                        }
                        Err(e) => Poll::Ready(Err(e)),
                    },
                    Poll::Pending => {
                        *slf.write_fut = Some(fut);
                        Poll::Pending
                    }
                }
            }
            (false, true) => {
                let mut slf = self.project();
                let mut fut = slf.write_fut.take().unwrap();
                let pinned = std::pin::pin!(&mut fut);
                match pinned.poll(cx) {
                    Poll::Ready(res) => match res {
                        Ok(mut buffered) => {
                            try_write_into_cursor(&mut buffered, buf);
                            *slf.write_buf = Some(buffered);
                            Poll::Ready(Ok(()))
                        }
                        Err(e) => Poll::Ready(Err(e)),
                    },
                    Poll::Pending => {
                        *slf.write_fut = Some(fut);
                        Poll::Pending
                    }
                }
            }
            (a, b) => Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                anyhow::anyhow!("async read is busy - buf present={a}, fut present={b}"),
            ))),
        }
    }
}

async fn uring_read_into(
    stream: Rc<tokio_uring::net::TcpStream>,
    bytes: BufferedBytesMut,
) -> Result<BufferedBytesMut, std::io::Error> {
    let buffer = bytes.bytes;
    let range = bytes.unread_range;
    let res = stream.read(buffer).await;
    let written = res.0?;
    let unread_range = if let Some(mut range) = range {
        range.count += written;
        range
    } else {
        UnreadRange {
            start: 0,
            count: written,
        }
    };
    Ok(BufferedBytesMut {
        bytes: res.1,
        unread_range: Some(unread_range),
    })
}

// If there's some unread bytes in the buffer space, just dump them immediately
fn try_write_into_cursor(
    buffered_bytes_mut: &mut BufferedBytesMut,
    buf: &mut tokio::io::ReadBuf<'_>,
) -> bool {
    if let Some(mut rng) = buffered_bytes_mut.unread_range {
        unsafe {
            let uninit = buf.unfilled_mut();
            let len = uninit.len();
            let write_up_to = rng.count.min(len);
            let uninit_ptr: *mut u8 = uninit.as_mut_ptr().cast();
            buffered_bytes_mut
                .bytes
                .as_ptr()
                .add(rng.start)
                .copy_to_nonoverlapping(uninit_ptr, write_up_to);
            if rng.count > write_up_to {
                rng.count -= write_up_to;
                rng.start += write_up_to;
                buffered_bytes_mut.unread_range = Some(rng);
            } else {
                buffered_bytes_mut.unread_range = None;
            }
            buf.assume_init(write_up_to);
            buf.advance(write_up_to);
        }
        return true;
    }
    false
}

fn submit_read(
    tcp: &Rc<tokio_uring::net::TcpStream>,
    read_buf: &mut Option<BufferedOutgoingBytes>,
    read_fut: &mut Option<Pin<Box<dyn Future<Output = Result<BufferedOutgoingBytes, std::io::Error>>>>>,
    cx: &mut Context<'_>,
) -> Poll<Result<bool, Error>> {
    if read_fut.is_none() {
        let rf = read_buf.take().unwrap();
        *read_fut = Some(Box::pin(try_write_to_stream(tcp.clone(), rf)));
    }
    let mut fut = read_fut.take().unwrap();
    let pinned = std::pin::pin!(&mut fut);
    match pinned.poll(cx) {
        Poll::Ready(res) => match res {
            Ok(buffered) => {
                let has_remaining = buffered.unwritten();
                *read_buf = Some(buffered);
                Poll::Ready(Ok(has_remaining))
            }
            Err(e) => Poll::Ready(Err(e)),
        },
        Poll::Pending => {
            *read_fut = Some(fut);
            Poll::Pending
        }
    }
}

impl tokio::io::AsyncWrite for UringTcp {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, Error>> {
        match (self.read_buf.is_some(), self.read_fut.is_some()) {
            (true, false) => {
                // Write the bytes into the outgoing buffer and send it into the future
                let mut rf = self.read_buf.take().unwrap();
                let write_into = buf.len().min(rf.bytes.len());
                rf.bytes[..write_into].copy_from_slice(&buf[..write_into]);
                rf.unread_range = Some(UnreadRange {
                    start: 0,
                    count: write_into,
                });
                self.read_buf = Some(rf);
                let mut read_fut = None;
                // If instantly done, keep going. Unlikely to ever happen though
                loop {
                    match submit_read(
                        &self.inner_tcp.clone(),
                        &mut self.read_buf,
                        &mut read_fut,
                        cx,
                    ) {
                        Poll::Ready(res) => {
                            match res {
                                Ok(has_rem) => {
                                    if has_rem {
                                        continue;
                                    }
                                    // Signal all bytes that was dumped into the buffer,
                                    // they still need to be 'flushed' to be sure of write
                                    break Poll::Ready(Ok(write_into));
                                }
                                Err(e) => {
                                    break Poll::Ready(Err(e));
                                }
                            }
                        }
                        Poll::Pending => {
                            *self.project().read_fut = read_fut;
                            break Poll::Ready(Ok(write_into));
                        }
                    }
                }
            }
            (false, true) => {
                let mut read_fut = self.read_fut.take();
                // If instantly done, keep going. Unlikely to ever happen though
                let mut accumulated_accepted: usize = 0;
                loop {
                    match submit_read(
                        &self.inner_tcp.clone(),
                        &mut self.read_buf,
                        &mut read_fut,
                        cx,
                    ) {
                        Poll::Ready(res) => match res {
                            Ok(has_rem) => {
                                if has_rem {
                                    continue;
                                }
                                let mut rf = self.read_buf.take().unwrap();
                                let write_into = buf.len().min(rf.bytes.len());
                                rf.bytes[..write_into].copy_from_slice(&buf[..write_into]);
                                rf.unread_range = Some(UnreadRange {
                                    start: 0,
                                    count: write_into,
                                });
                                self.read_buf = Some(rf);
                                accumulated_accepted += write_into;
                                continue;
                            }
                            Err(e) => {
                                break Poll::Ready(Err(e));
                            }
                        },
                        Poll::Pending => {
                            let mut slf = self.project();
                            *slf.read_fut = read_fut;
                            break if accumulated_accepted > 0 {
                                Poll::Ready(Ok(accumulated_accepted))
                            } else {
                                Poll::Pending
                            };
                        }
                    }
                }
            }
            (a, b) => Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                anyhow::anyhow!("async write is busy - buf present={a}, fut present={b}"),
            ))),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        let mut read_fut = self.read_fut.take();
        match (self.read_buf.is_some(), read_fut.is_some()) {
            (true, false) => loop {
                match submit_read(
                    &self.inner_tcp.clone(),
                    &mut self.read_buf,
                    &mut read_fut,
                    cx,
                ) {
                    Poll::Ready(res) => match res {
                        Ok(has_rem) => {
                            if has_rem {
                                continue;
                            }
                            break Poll::Ready(Ok(()));
                        }
                        Err(e) => {
                            break Poll::Ready(Err(e));
                        }
                    },
                    Poll::Pending => {
                        *self.project().read_fut = read_fut;
                        break Poll::Pending;
                    }
                }
            },
            (false, true) => {
                // If instantly done, keep going. Unlikely to ever happen though
                loop {
                    match submit_read(
                        &self.inner_tcp.clone(),
                        &mut self.read_buf,
                        &mut read_fut,
                        cx,
                    ) {
                        Poll::Ready(res) => match res {
                            Ok(has_rem) => {
                                if has_rem {
                                    continue;
                                }
                                break Poll::Ready(Ok(()));
                            }
                            Err(e) => {
                                break Poll::Ready(Err(e));
                            }
                        },
                        Poll::Pending => {
                            *self.project().read_fut = read_fut;
                            break Poll::Pending;
                        }
                    }
                }
            }
            (a, b) => Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                anyhow::anyhow!("async write flush is busy - buf present={a}, fut present={b}"),
            ))),
        }
    }
    #[inline]
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        let res = self.inner_tcp.shutdown(Shutdown::Both);
        Poll::Ready(res)
    }

    fn is_write_vectored(&self) -> bool {
        // Todo: implement
        false
    }
}

async fn try_write_to_stream(
    stream: Rc<tokio_uring::net::TcpStream>,
    bytes: BufferedOutgoingBytes,
) -> Result<BufferedOutgoingBytes, std::io::Error> {
    if bytes.unread_range.is_none() {
        return Ok(bytes);
    }
    let mut buf = bytes.bytes;
    let mut range = bytes.unread_range.unwrap();
    let mut mid = BytesMut::split_off(&mut buf, range.start);
    let end = mid.split_off(range.count);
    let r = stream.write(mid).await;
    let written = r.0?;
    let mut mid = r.1;
    mid.unsplit(end);
    buf.unsplit(mid);
    let unread_range = if range.count <= written {
        None
    } else {
        range.count -= written;
        range.start += written;
        Some(range)
    };
    Ok(BufferedOutgoingBytes {
        bytes: buf,
        unread_range,
    })
}
