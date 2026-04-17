use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use crate::runtime::{current_result, release_read_buf, submit_commit_write};

/// Future produite par `RwBuffer::commit()`.
///
/// - Init    : soumet le WriteFixed (user_data de la task courante).
/// - Pending : attend la CQE du write, libère ensuite le buffer dans le buf_ring
///             (écriture userspace, zéro syscall), puis retourne le résultat.
pub(crate) enum CommitFuture<const N: usize> {
    Init {
        ptr: *mut u8,
        buf_idx: u32,
        fd_idx: u32,
        len: u32,
    },
    /// buf_idx conservé pour la release buf_ring sur réception de la CQE.
    Pending { buf_idx: u32 },
}

impl<const N: usize> Future for CommitFuture<N> {
    type Output = i32; // octets écrits, ou errno négatif

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<i32> {
        match *self {
            CommitFuture::Init { ptr, buf_idx, fd_idx, len } => {
                submit_commit_write(ptr, fd_idx, len, buf_idx as u16);
                *self = CommitFuture::Pending { buf_idx };
                Poll::Pending
            }
            CommitFuture::Pending { buf_idx } => {
                // Write confirmé : rendre le slot au buf_ring (userspace, pas de syscall).
                release_read_buf(buf_idx);
                Poll::Ready(current_result())
            }
        }
    }
}

/// Construit un `CommitFuture` depuis les champs bruts d'un `RwBuffer`.
/// Appelé exclusivement par `RwBuffer::commit()`.
#[inline]
pub(crate) fn make_commit_future<const N: usize>(
    ptr: *mut u8,
    buf_idx: u32,
    fd_idx: u32,
    len: u32,
) -> CommitFuture<N> {
    CommitFuture::Init { ptr, buf_idx, fd_idx, len }
}
