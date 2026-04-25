use std::{
    future::Future,
    hint::unreachable_unchecked,
    pin::Pin,
    task::{Context, Poll},
    u32,
};

use crate::{
    buf_pool::WBuffer,
    runtime::{CURRENT_TASK, GLOBAL_RUNTIME, current_result, submit_multiple_write, submit_write},
};

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

enum WriteMultipleFuture {
    Init {
        fds: Vec<u32>,
        buffer: WBuffer<1>,
        len: u32,
    },
    Pending {
        in_fligth_count: u16,
        fds: Vec<u32>,
        faileds: Vec<WriteResult>,
        len: u32,
        _buffer: WBuffer<1>, //This old the buffer durring the duration off all the Future
    },
    Poisoned,
}

pub enum WriteResult {
    Failed { fd: u32 },
    Partial { fd: u32, len: u32 },
}

impl Future for WriteMultipleFuture {
    type Output = Vec<WriteResult>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        if let Self::Init { .. } = this {
            let Self::Init { fds, buffer, len } = std::mem::replace(this, Self::Poisoned) else {
                unsafe { unreachable_unchecked() };
            };
            submit_multiple_write(fds.as_slice(), buffer.clone(), len);
            *this = WriteMultipleFuture::Pending {
                in_fligth_count: fds.len() as u16,
                fds,
                faileds: Vec::new(),
                len,
                _buffer: buffer,
            };
            return Poll::Pending;
        }
        let Self::Pending {
            in_fligth_count,
            fds,
            faileds,
            len,
            _buffer,
        } = this
        else {
            unsafe { unreachable_unchecked() }
        };

        *in_fligth_count = *in_fligth_count - 1;
        let task_info = CURRENT_TASK.get();

        let gen_result = task_info.response_gen;
        let fd = fds[gen_result as usize];
        assert!(
            fd != u32::MAX,
            "Re using a empty slot in WriteMultipleFuture::Pending there are some duplicate gen"
        );
        fds[gen_result as usize] = u32::MAX;
        if task_info.result < 0 {
            faileds.push(WriteResult::Failed { fd });
        }
        if task_info.result as u32 != *len {
            faileds.push(WriteResult::Partial {
                fd,
                len: task_info.result as u32,
            });
        }

        if *in_fligth_count == 0 {
            GLOBAL_RUNTIME.with_borrow_mut(|rt| rt.advance_syscall_nb(&task_info.tag));
            let mut task_info = CURRENT_TASK.get();
            task_info.set_to_next_task();
            CURRENT_TASK.set(task_info);
            return Poll::Ready(std::mem::take(faileds));
        }
        Poll::Pending
    }
}
pub fn write_multiple(
    fds: Vec<u32>,
    buffer: WBuffer<1>,
    len: u32,
) -> impl Future<Output = Vec<WriteResult>> {
    WriteMultipleFuture::Init { fds, buffer, len }
}
