use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use crate::{
    runtime::{current_cqe_flags, current_result, submit_accept, submit_accept_multishot},
    stream::Stream,
};

// ─── Single-shot ─────────────────────────────────────────────────────────────

pub struct AcceptFuture {
    listen_fd: i32,
    submitted: bool,
}

impl Future for AcceptFuture {
    type Output = u32; // fixed_file_index

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<u32> {
        if !self.submitted {
            submit_accept(self.listen_fd);
            self.submitted = true;
            return Poll::Pending;
        }
        println!("Recieve a conn");
        Poll::Ready(current_result() as u32)
    }
}

pub fn accept(listen_fd: i32) -> AcceptFuture {
    AcceptFuture {
        listen_fd,
        submitted: false,
    }
}

// ─── Multishot stream ─────────────────────────────────────────────────────────
//
// Soumet un seul ACCEPT_MULTISHOT + ACCEPT_DIRECT SQE (MULTIPLE_MASK).
// Chaque connexion entrante génère une CQE avec le fixed_file_index alloué.
//
// Fin du multishot (résultat < 0, ou !F_MORE) :
//   Le dernier fd valide est livré (Some), puis None au poll suivant.

const IORING_CQE_F_MORE: u32 = 1 << 1;

enum AcceptStreamState {
    Init,
    Running,
    Done,
}

pub struct AcceptStream {
    listen_fd: i32,
    state: AcceptStreamState,
}

impl AcceptStream {
    fn new(listen_fd: i32) -> Self {
        Self {
            listen_fd,
            state: AcceptStreamState::Init,
        }
    }
}

impl Stream for AcceptStream {
    type Item = u32; // fixed_file_index

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<u32>> {
        match self.state {
            AcceptStreamState::Init => {
                submit_accept_multishot(self.listen_fd);

                self.state = AcceptStreamState::Running;
                Poll::Pending
            }
            AcceptStreamState::Running => {
                let result = current_result();
                if result < 0 {
                    self.state = AcceptStreamState::Done;
                    return Poll::Ready(None);
                }
                let flags = current_cqe_flags();
                if flags & IORING_CQE_F_MORE == 0 {
                    self.state = AcceptStreamState::Done;
                }
                Poll::Ready(Some(result as u32))
            }
            AcceptStreamState::Done => Poll::Ready(None),
        }
    }
}

pub fn accept_stream(listen_fd: i32) -> AcceptStream {
    AcceptStream::new(listen_fd)
}
