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
        read_buf: QueueBuffers,
        #[pin]
        read_fut: Option<Pin<Box<dyn Future<Output=Result<BytesMut, std::io::Error>>>>>,
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
            read_buf: QueueBuffers::new(BytesMut::zeroed(buf_size)),
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

#[derive(Eq, PartialEq)]
enum OutstandingBuffer {
    // Buffer to the left of the currently available is outstanding
    Left,
    // Buffer to the right of the currently available is outstanding
    Right,
    // No buffers outstanding
    None
}

struct QueueBuffers {
    total_size: usize,
    outstanding: OutstandingBuffer,
    // Currently available buffer
    rem_bytes: BytesMut,
    // Currently available byte range
    r1: UnreadRange,
}

impl QueueBuffers {
    fn new(buffer: BytesMut) -> Self {
        Self {
            total_size: buffer.capacity(),
            outstanding: OutstandingBuffer::None,
            rem_bytes: buffer,
            r1: UnreadRange {
                start: 0,
                count: 0,
            },
        }
    }

    #[inline]
    fn cur_buf_capacity(&self) -> usize {
        self.rem_bytes.len() - self.r1.offset()
    }

    fn write_into_avail(&mut self, write: &[u8]) -> Option<usize> {
        let cap = self.cur_buf_capacity();
        let write_bytes = cap.min(write.len());
        if write_bytes == 0 {
            return None;
        }
        let start = self.r1.offset();
        self.rem_bytes.as_mut()[start..start + write_bytes].copy_from_slice(&write[..write_bytes]);
        self.r1.count += write_bytes;
        Some(write_bytes)
    }

    #[inline]
    fn has_outstanding(&self) -> bool {
        self.outstanding == OutstandingBuffer::None && self.r1.count == 0
    }

    fn get_unchecked(&mut self) -> Option<(BytesMut, Option<usize>)> {
        if self.r1.count == 0 {
            return None;
        }
        match self.outstanding {
            OutstandingBuffer::Left |
            OutstandingBuffer::Right => {
                panic!("Invariant broken, get when already split");
            }
            OutstandingBuffer::None => {
                if self.r1.start == 0 {
                    // Buffer starts from the left
                    let rest = self.rem_bytes.split_off(self.r1.offset());
                    let out = std::mem::replace(&mut self.rem_bytes, rest);
                    self.outstanding = OutstandingBuffer::Left;
                    self.r1 = UnreadRange {
                        start: 0,
                        count: 0,
                    };
                    Some((out, None))
                } else {
                    // Buffer starts with a right-offset
                    let out = self.rem_bytes.split_off(self.r1.offset());
                    self.outstanding = OutstandingBuffer::Right;
                    let count = self.r1.count;
                    self.r1 = UnreadRange {
                        start: 0,
                        count: 0,
                    };
                    Some((out, Some(count)))
                }
            }
        }
    }

    fn merge_back(&mut self, mut prev_writing: BytesMut) -> bool {
        match self.outstanding {
            OutstandingBuffer::Left => {
                let tmp = std::mem::take(&mut self.rem_bytes);
                let prev_len = prev_writing.len();
                prev_writing.unsplit(tmp);
                self.rem_bytes = prev_writing;
                if self.r1.count != 0 {
                    self.r1.start += prev_len;
                }
                self.outstanding = OutstandingBuffer::None;
                self.r1.count != 0
            }
            OutstandingBuffer::Right => {
                self.rem_bytes.unsplit(prev_writing);
                self.outstanding = OutstandingBuffer::None;
                self.r1.count != 0
            }
            OutstandingBuffer::None => {
                panic!("Invariant violated, tried to unsplit an unsplit buffer");
            }
        }
    }
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

impl UnreadRange {
    #[inline]
    #[must_use]
    pub fn offset(&self) -> usize {
        self.start + self.count
    }
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
    read_buf: &mut QueueBuffers,
    read_fut: &mut Option<
        Pin<Box<dyn Future<Output = Result<BytesMut, std::io::Error>>>>,
    >,
    cx: &mut Context<'_>,
) -> Poll<Result<bool, Error>> {
    let mut fut = if let Some(rf) = read_fut.take() {
        rf
    } else {
        // If no read fut, there should be a ready buffer
        let Some((bytes, count)) = read_buf.get_unchecked() else {
            return Poll::Ready(Ok(false));
        };
        Box::pin(try_write_to_stream(tcp.clone(), bytes, count))
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
                            continue;
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
    mut bytes: BytesMut,
    count: Option<usize>
) -> Result<BytesMut, std::io::Error> {
    let (r, rest) = if let Some(count) = count {
        let rest = BytesMut::split_off(&mut bytes, count);
        (stream.write(bytes).await, Some(rest))
    } else {
        (stream.write(bytes).await, None)
    };
    let mut bytes = r.1;
    if let Some(rest) = rest {
        bytes.unsplit(rest);
    }
    Ok(bytes)
}
