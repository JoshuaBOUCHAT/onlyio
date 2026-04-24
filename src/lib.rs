pub mod buf_pool;
pub mod runtime;
pub mod stream;

pub(crate) mod task;
pub(crate) mod task_slab;
pub(crate) type IOResult<T> = std::io::Result<T>;
mod calls;

use buf_pool::{RwBuffer, WBuffer};
use runtime::GLOBAL_RUNTIME;

use crate::runtime::Runtime;
pub use calls::accept::accept;
pub use calls::accept::accept_stream;
pub use calls::read::read;
pub use calls::read::read_stream;

//Pour l'instant la fonction est blocking et retourn rien
pub fn block_on(future: impl Future<Output = ()>) -> IOResult<()> {
    Runtime::block_on(future)
}

#[cfg(test)]
mod test {
    use std::{
        io::{Cursor, Read, Write},
        net::{TcpListener, TcpStream},
        os::unix::io::AsRawFd,
        thread,
        time::Duration,
    };

    use crate::{
        accept, block_on,
        calls::{accept::accept_stream, read::read_stream},
        read,
        runtime::spawn,
        stream::StreamExt,
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
            s.write_all(b"ping").unwrap();
            let mut buf = [0u8; 8];
            s.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"pingping");
        });

        block_on(async move {
            println!("Sending multishot accept");
            let mut conns = accept_stream(listen_fd);
            println!("Accepted");
            let conn_fd = conns.next().await.unwrap();
            println!("Recieve conn");

            let mut reads = read_stream(conn_fd);
            let mut rwbuf = reads.next().await.unwrap();
            println!("recieve read");
            let len = rwbuf.bytes as usize;
            // Double le contenu en place : [0..len] → [0..len] + [len..2*len]
            rwbuf.write_slice().copy_within(0..len, len);
            // Envoie les 2*len octets, attend la CQE, libère le slot buf_ring
            rwbuf.commit((2 * len) as u32).await;
            println!("Commited");
        })
        .unwrap();

        client.join().unwrap();
    }
    #[test]
    fn stream_test() {
        let tcp_stream = TcpListener::bind("0.0.0.0:6379").expect("enable to connect");
        let fd = tcp_stream.as_raw_fd();

        block_on(async {
            let mut conns = accept_stream(fd);

            let conn_fd = conns.next().await.expect("no more connexion");

            let mut read_stream = read_stream(conn_fd);
            let mut counter = 0;
            while let Some(mut read) = read_stream.next().await {
                println!(
                    "Recieve: {}",
                    std::string::String::from_utf8_lossy(read.read_slice())
                );
                counter += read.bytes;
                let mut cursor = Cursor::new(read.write_slice());

                write!(cursor, "Total number of char recieve is: {}", counter)
                    .expect("can't write back the message");
                let len = cursor.position() as usize;
                read.commit(len as u32).await;
            }
            println!("read stream.next() returned None");
        })
        .expect("Error");
    }

    #[test]
    fn test_two_connexion() {
        let listener = std::net::TcpListener::bind("127.0.0.1:6379").unwrap();
        listener.set_nonblocking(true).unwrap();
        let listen_fd = listener.as_raw_fd();
        let client = thread::spawn(|| {
            thread::sleep(Duration::from_millis(50));
            let mut s = TcpStream::connect("127.0.0.1:6379").unwrap();
            s.write_all(b"ping").unwrap();
            let mut buf = [0u8; 8];
            s.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"pingping");
        });
        let client = thread::spawn(|| {
            thread::sleep(Duration::from_millis(50));
            let mut s = TcpStream::connect("127.0.0.1:6379").unwrap();
            s.write_all(b"pong").unwrap();
            let mut buf = [0u8; 8];
            s.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"pingping");
        });
        block_on(async {
            for _ in 0..2 {
                let conn_fd = accept(listen_fd).await.expect("accept failed");
                spawn(handle_connexion(conn_fd));
            }
        })
        .expect("block on failed");
    }
    async fn handle_connexion(conn_fd: u32) {
        let buffer = read(conn_fd).await.expect("Read failed");
        let len = buffer.bytes;
        assert!(buffer.commit(len).await > 0, "Buffer commit failed");
    }
    #[test]
    fn test_close() {
        let listener = std::net::TcpListener::bind("127.0.0.1:6379").unwrap();
        listener.set_nonblocking(true).unwrap();
        let listen_fd = listener.as_raw_fd();
        let _ = thread::spawn(|| {
            thread::sleep(Duration::from_millis(50));
            let mut s = TcpStream::connect("127.0.0.1:6379").unwrap();
            s.write_all(b"ping").unwrap();
            let mut buf = [0u8; 4];
            s.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"ping");
        });
        block_on(async {
            let conn_fd = accept(listen_fd).await.expect("accept failed");
            spawn(async {
                let buffer = read(conn_fd).await.expect("Read failed");
                let len = buffer.bytes;
                assert!(buffer.commit(len).await > 0, "Buffer commit failed");
            });
        })
        .expect("block on failed");
    }
}
