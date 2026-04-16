pub mod buf_pool;
pub mod runtime;

pub(crate) mod task;
pub(crate) mod task_slab;
pub(crate) type IOResult<T> = std::io::Result<T>;
mod calls;

use buf_pool::{RwBuffer, WBuffer};
use runtime::GLOBAL_RUNTIME;

use crate::runtime::Runtime;

//Pour l'instant la fonction est blocking et retourn rien
pub fn block_on(future: impl Future<Output = ()>) -> IOResult<()> {
    Runtime::block_on(future)
}

#[cfg(test)]
mod test {
    use std::{
        io::{Read, Write},
        net::TcpStream,
        os::unix::io::AsRawFd,
        thread,
        time::Duration,
    };

    use crate::{
        block_on,
        calls::{accept::accept, read::read},
    };

    #[test]
    fn test_runtime() {
        let hello = async {
            println!("Hello !");
        };
        block_on(hello).expect("Should not have any io error");
    }

    /// Ouvre un serveur TCP sur 6379, accepte une connexion en multishot,
    /// lit un message et le renvoie en double (contenu × 2).
    #[test]
    fn test_echo_double_6379() {
        let listener = std::net::TcpListener::bind("127.0.0.1:6379").unwrap();
        listener.set_nonblocking(true).unwrap();
        let listen_fd = listener.as_raw_fd(); // listener reste vivant jusqu'à la fin du test

        // Client : envoie "ping", attend "pingping"
        let client = thread::spawn(|| {
            thread::sleep(Duration::from_millis(50));
            let mut s = TcpStream::connect("127.0.0.1:6379").unwrap();
            println!("Client conn open");
            s.write_all(b"ping").unwrap();
            println!("Client send mesage");
            let mut buf = [0u8; 8];
            s.read_exact(&mut buf).unwrap();
            println!("Client recieve message back");
            assert_eq!(&buf, b"pingping");
        });
        println!("starting the server");
        block_on(async move {
            // Accepte la première connexion (multishot — one-shot ici)
            let conn_fd = accept(listen_fd).await;
            println!("Now accepting");

            // Lit le message entrant (BUFFER_SELECT, kernel choisit le buffer)
            let mut rwbuf = read(conn_fd).await;
            println!("Read");
            let len = rwbuf.bytes as usize;

            // Double le contenu en place : [0..len] → [0..len] + [len..2*len]
            rwbuf.write_slice().copy_within(0..len, len);
            println!("Commiting");

            // Envoie les 2*len octets et re-queue un read sur la connexion
            rwbuf.commit((2 * len) as u32);
            println!("Commited");
        })
        .unwrap();

        client.join().unwrap();
    }
}
