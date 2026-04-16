use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use crate::{
    buf_pool::RwBuffer,
    runtime::{GLOBAL_RUNTIME, current_cqe_flags, current_result, submit_read},
};

enum ReadFuture {
    Init(u32),    // fd_idx, pas encore soumis
    Pending(u32), // fd_idx, SQE en vol
}

impl Future for ReadFuture {
    type Output = RwBuffer<1>;

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<RwBuffer<1>> {
        match *self {
            ReadFuture::Init(fd) => {
                submit_read(fd);
                *self = ReadFuture::Pending(fd);
                Poll::Pending
            }
            ReadFuture::Pending(fd) => {
                let bytes = current_result();
                // buf_id fourni par le kernel dans les flags (IORING_CQE_BUFFER_SHIFT = 16)
                let buf_id = (current_cqe_flags() >> 16) as u32;
                let rwbuf = GLOBAL_RUNTIME
                    .with_borrow(|rt| rt.buffer.checkout_read(buf_id, fd, bytes as u32, 0));
                Poll::Ready(rwbuf)
            }
        }
    }
}

pub async fn read(fd: u32) -> RwBuffer<1> {
    ReadFuture::Init(fd).await
}
