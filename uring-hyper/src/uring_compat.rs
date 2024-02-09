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
        read_buf: OutgoingBuffers,
        #[pin]
        read_fut: Option<Pin<Box<dyn Future<Output=Result<OutgoingWriteBuffer, std::io::Error>>>>>,
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
            read_buf: OutgoingBuffers::new(BytesMut::zeroed(buf_size), BytesMut::zeroed(buf_size)),
            read_fut: None,
            write_buf: Some(BufferedBytesMut {
                bytes: BytesMut::zeroed(buf_size),
                unread_range: None,
            }),
            write_fut: None,
        }
    }
}

struct OutgoingBuffers {
    // Missing if currently writing to stream
    writing: Option<OutgoingWriteBuffer>,
    // Always present but can be full
    buffer: OutgoingWriteBuffer,
}

impl OutgoingBuffers {
    fn new(left: BytesMut, right: BytesMut) -> Self {
        Self {
            writing: Some(OutgoingWriteBuffer {
                bytes: left,
                unread_range: UnreadRange {
                    start: 0,
                    count: 0,
                },
            }),
            buffer: OutgoingWriteBuffer {
                bytes: right,
                unread_range: UnreadRange {
                    start: 0,
                    count: 0,
                },
            },
        }
    }

    fn write_into_avail(&mut self, write: &[u8]) -> Option<usize> {
        // Has space in right, to preserve order, bytes need to be written into there
        if !self.buffer.unwritten() {
            let written = self.buffer.copy_into(write);
            if written == 0 {
                return None;
            }
            return Some(written);
        }
        if let Some(active) = &mut self.writing {
            // Active buffer is not busy and passive buffer has nothing in it, extend
            let written = active.copy_into(write);
            if written == 0 {
                return None;
            }
            return Some(written);
        }
        None
    }

    fn get_unchecked(&mut self) -> OutgoingWriteBuffer {
        self.writing.take().unwrap()
    }

    fn merge_back(&mut self, prev_writing: OutgoingWriteBuffer) -> bool {
        if prev_writing.unwritten() {
            // Did not manage to write the old buffer completely
            self.writing = Some(prev_writing);
            true
        } else {
            // Managed to write the old buffer completely, swap in the waiting one
            let new_writing = std::mem::replace(&mut self.buffer, prev_writing);
            let has_rem = new_writing.unwritten();
            self.writing = Some(new_writing);
            has_rem
        }
    }
}

#[derive(Debug)]
struct QueueBuffer {
    bytes: BytesMut,
    written_offset: usize,
}

#[derive(Debug)]
struct OutgoingWriteBuffer {
    bytes: BytesMut,
    unread_range: UnreadRange,
}

impl OutgoingWriteBuffer {
    #[inline]
    fn unwritten(&self) -> bool {
        self.unread_range.count > 0
    }

    #[inline]
    fn avail_space(&self) -> usize {
        self.bytes.len() - (self.unread_range.start + self.unread_range.count)
    }

    #[inline]
    fn space_end(&self) -> usize {
        self.unread_range.start + self.unread_range.count
    }

    fn copy_into(&mut self, source: &[u8]) -> usize {
        let end = self.space_end();
        if end < self.bytes.len() - 1 {
            let space = self.bytes.len() - end;
            let to_write = space.min(source.len());
            self.bytes.as_mut()[end..end + to_write].copy_from_slice(&source[..to_write]);
            self.unread_range.count += to_write;
            to_write
        } else {
            0
        }
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

fn poll_outgoing(
    tcp: &Rc<tokio_uring::net::TcpStream>,
    read_buf: &mut OutgoingBuffers,
    read_fut: &mut Option<
        Pin<Box<dyn Future<Output = Result<OutgoingWriteBuffer, std::io::Error>>>>,
    >,
    cx: &mut Context<'_>,
) -> Poll<Result<bool, Error>> {
    let mut fut = if let Some(rf) = read_fut.take() {
        rf
    } else {
        // If no read fut, there should be a ready buffer
        let buf = read_buf.get_unchecked();
        Box::pin(try_write_to_stream(tcp.clone(), buf))
    };
    let pinned = std::pin::pin!(&mut fut);
    match pinned.poll(cx) {
        Poll::Ready(res) => match res {
            Ok(buffered) => {
                let has_remaining = read_buf.merge_back(buffered);
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
        let mut accumulated_accepted: usize = self.read_buf.write_into_avail(buf).unwrap_or_default();
        let mut fut = self.read_fut.take();
        loop {
            match poll_outgoing(
                &self.inner_tcp.clone(),
                &mut self.read_buf,
                &mut fut,
                cx,
            ) {
                Poll::Ready(res) => match res {
                    Ok(_has_rem) => {
                        // Done, try start a new one
                        accumulated_accepted += self.read_buf.write_into_avail(&buf[accumulated_accepted..]).unwrap_or_default();
                        continue;
                    }
                    Err(e) => {
                        break Poll::Ready(Err(e));
                    }
                },
                Poll::Pending => {
                    let mut slf = self.project();
                    *slf.read_fut = fut;
                    break if accumulated_accepted > 0 {
                        Poll::Ready(Ok(accumulated_accepted))
                    } else {
                        Poll::Pending
                    };
                }
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        let mut fut = self.read_fut.take();
        loop {
            match poll_outgoing(
                &self.inner_tcp.clone(),
                &mut self.read_buf,
                &mut fut,
                cx,
            ) {
                Poll::Ready(res) => match res {
                    Ok(has_rem) => {
                        if has_rem {
                            continue
                        }
                        break Poll::Ready(Ok(()));
                    }
                    Err(e) => {
                        break Poll::Ready(Err(e));
                    }
                },
                Poll::Pending => {
                    let mut slf = self.project();
                    *slf.read_fut = fut;
                    break Poll::Pending;
                }
            }
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
    bytes: OutgoingWriteBuffer,
) -> Result<OutgoingWriteBuffer, std::io::Error> {
    if !bytes.unwritten() {
        return Ok(bytes);
    }
    let mut buf = bytes.bytes;
    let mut range = bytes.unread_range;
    let mut mid = BytesMut::split_off(&mut buf, range.start);
    let end = mid.split_off(range.count);
    let r = stream.write(mid).await;
    let written = r.0?;
    let mut mid = r.1;
    mid.unsplit(end);
    buf.unsplit(mid);
    let unread_range = if range.count <= written {
        UnreadRange {
            start: 0,
            count: 0,
        }
    } else {
        range.count -= written;
        range.start += written;
        range
    };
    Ok(OutgoingWriteBuffer {
        bytes: buf,
        unread_range,
    })
}
