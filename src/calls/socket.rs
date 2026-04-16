use std::{
    future::Future,
    mem,
    pin::Pin,
    task::{Context, Poll},
};

use crate::runtime::{current_result, submit_connect, submit_socket};

/// Étapes du socket TCP client avec keepalive.
enum SocketFuture {
    /// Soumet SOCKET pour créer le fd dans la fixed files table.
    CreateSocket { addr: libc::sockaddr_in },
    /// Socket créé (fixed_file_index = result), on lance CONNECT.
    Connecting { fd: u32, addr: libc::sockaddr_in },
    Pending,
}

impl Future for SocketFuture {
    type Output = u32; // fixed_file_index du socket connecté

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<u32> {
        loop {
            match *self {
                SocketFuture::CreateSocket { addr } => {
                    submit_socket();
                    *self = SocketFuture::Connecting { fd: 0, addr }; // fd mis à jour à la CQE
                    return Poll::Pending;
                }
                SocketFuture::Connecting { ref mut fd, addr } => {
                    // Premier passage : result = fixed_file_index du socket créé.
                    if *fd == 0 {
                        let slot = current_result() as u32;
                        // Keepalive : setsockopt sur le fd réel n'est pas possible sur fixed files.
                        // Utiliser IORING_OP_URING_CMD ou un fd temporaire si nécessaire.
                        // Pour l'instant on connecte directement.
                        *fd = slot;
                        let sockaddr = &addr as *const libc::sockaddr_in as *const libc::sockaddr;
                        submit_connect(slot, sockaddr, mem::size_of::<libc::sockaddr_in>() as u32);
                        return Poll::Pending;
                    }
                    // Deuxième passage : CQE de CONNECT.
                    let connected_fd = *fd;
                    *self = SocketFuture::Pending;
                    return Poll::Ready(connected_fd);
                }
                SocketFuture::Pending => unreachable!(),
            }
        }
    }
}

/// Ouvre une connexion TCP vers `ip:port` et retourne le fixed_file_index.
pub async fn connect(ip: [u8; 4], port: u16) -> u32 {
    let addr = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: port.to_be(),
        sin_addr: libc::in_addr {
            s_addr: u32::from_ne_bytes(ip),
        },
        sin_zero: [0; 8],
    };
    SocketFuture::CreateSocket { addr }.await
}
