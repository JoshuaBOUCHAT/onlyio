use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use crate::runtime::{current_result, submit_accept};

/// Future multishot : chaque poll après une CQE retourne le fixed_file_index
/// de la nouvelle connexion. La future reste vivante (Poll::Pending) après chaque accept.
/// Retourner Poll::Ready arrête le multishot.
pub struct AcceptFuture {
    listen_fd: i32,
    submitted: bool,
}

impl AcceptFuture {
    fn new(listen_fd: i32) -> Self {
        Self { listen_fd, submitted: false }
    }
}

impl Future for AcceptFuture {
    type Output = u32; // fixed_file_index de la connexion acceptée

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<u32> {
        if !self.submitted {
            submit_accept(self.listen_fd);
            self.submitted = true;
            return Poll::Pending;
        }
        // Chaque CQE multishot repoll la future ici.
        // result = fixed_file_index alloué par le kernel, ou errno négatif.
        Poll::Ready(current_result() as u32)
    }
}

/// Lance le multishot accept sur `listen_fd`.
/// À utiliser dans une boucle : la future se repoll sur chaque connexion entrante.
pub fn accept(listen_fd: i32) -> AcceptFuture {
    AcceptFuture::new(listen_fd)
}
