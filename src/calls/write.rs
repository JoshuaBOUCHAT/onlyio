use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use crate::runtime::{current_result, submit_write};

enum WriteFuture {
    Init { fd: u32, buf_id: u16, len: u32 },
    Pending,
}

impl Future for WriteFuture {
    type Output = i32; // octets écrits, ou errno négatif

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<i32> {
        match *self {
            WriteFuture::Init { fd, buf_id, len } => {
                submit_write(fd, buf_id, len);
                *self = WriteFuture::Pending;
                Poll::Pending
            }
            WriteFuture::Pending => Poll::Ready(current_result()),
        }
    }
}

pub async fn write(fd: u32, buf_id: u16, len: u32) -> i32 {
    WriteFuture::Init { fd, buf_id, len }.await
}
