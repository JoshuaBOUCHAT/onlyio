use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use io_uring::cqueue;

use crate::{
    buf_pool::RwBuffer,
    runtime::{GLOBAL_RUNTIME, current_cqe_flags, current_result, submit_read, submit_recv_multishot},
    stream::Stream,
};

// ─── Single-shot ─────────────────────────────────────────────────────────────

enum ReadFuture {
    Init(u32),
    Pending(u32),
}

impl Future for ReadFuture {
    type Output = Result<RwBuffer<1>, i32>;

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<RwBuffer<1>, i32>> {
        match *self {
            ReadFuture::Init(fd) => {
                submit_read(fd);
                *self = ReadFuture::Pending(fd);
                Poll::Pending
            }
            ReadFuture::Pending(fd) => {
                let bytes = current_result();
                let flags = current_cqe_flags();
                // buffer_select() retourne Some(buf_id) si le kernel a consommé un slot
                // du buf_ring — même sur EOF/erreur (kernel 5.19+). Il faut le rendre.
                if bytes <= 0 {
                    if let Some(buf_id) = cqueue::buffer_select(flags) {
                        crate::runtime::release_read_buf(buf_id as u32);
                    }
                    return Poll::Ready(Err(bytes));
                }
                let buf_id = cqueue::buffer_select(flags)
                    .expect("READ+BUFFER_SELECT: CQE positive sans buf_id") as u32;
                let rwbuf = GLOBAL_RUNTIME
                    .with_borrow(|rt| rt.buffer.checkout_read(buf_id, fd, bytes as u32));
                Poll::Ready(Ok(rwbuf))
            }
        }
    }
}

pub async fn read(fd: u32) -> Result<RwBuffer<1>, i32> {
    ReadFuture::Init(fd).await
}

// ─── Multishot stream ─────────────────────────────────────────────────────────
//
// Soumet un seul RECV_MULTISHOT SQE (MULTIPLE_MASK).
// Chaque CQE — avec ou sans IORING_CQE_F_MORE — appelle poll_next via le runtime.
//
// Cycle de vie du buf_ring :
//   Some(buf) retourné → RwBuffer tenu par le caller.
//   commit() sur le buf → libère après la CQE du write.
//   drop() sans commit → libère immédiatement (userspace).
//
// Fin du multishot (résultat < 0, ou !F_MORE) :
//   Le dernier item valide est livré (Some), puis None au poll suivant.

enum ReadStreamState {
    Init,
    Running,
    Done,
}

pub struct ReadStream {
    fd: u32,
    state: ReadStreamState,
}

impl ReadStream {
    fn new(fd: u32) -> Self {
        Self {
            fd,
            state: ReadStreamState::Init,
        }
    }
}

impl Stream for ReadStream {
    type Item = RwBuffer<1>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<RwBuffer<1>>> {
        match self.state {
            ReadStreamState::Init => {
                submit_recv_multishot(self.fd);
                self.state = ReadStreamState::Running;
                Poll::Pending
            }
            ReadStreamState::Running => {
                let result = current_result();
                let flags = current_cqe_flags();
                if result < 0 {
                    self.state = ReadStreamState::Done;
                    return Poll::Ready(None);
                }
                let buf_id = cqueue::buffer_select(flags)
                    .expect("RECV_MULTISHOT: CQE positive sans buf_id") as u32;
                let buf = GLOBAL_RUNTIME
                    .with_borrow(|rt| rt.buffer.checkout_read(buf_id, self.fd, result as u32));
                if !cqueue::more(flags) {
                    // Dernière CQE du multishot — prochain poll retourne None
                    self.state = ReadStreamState::Done;
                }
                Poll::Ready(Some(buf))
            }
            ReadStreamState::Done => Poll::Ready(None),
        }
    }
}

pub fn read_stream(fd: u32) -> ReadStream {
    ReadStream::new(fd)
}
