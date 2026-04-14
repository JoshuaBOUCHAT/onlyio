pub mod buf_pool;
pub mod runtime;
pub(crate) mod sqe_queue;
pub(crate) mod task;
pub(crate) mod task_slab;
pub(crate) type IOResult<T> = std::io::Result<T>;

use buf_pool::{RwBuffer, WBuffer};
use runtime::GLOBAL_RUNTIME;

// ============================================================================
// API publique
// ============================================================================
//
// Côté utilisateur (RadixOx), l'usage ressemble à :
//
//   // Lecture : le runtime livre les bytes bruts, RadixOx gère les partiels.
//   let buf: RwBuffer<1> = onlyio::read(fd_idx).await;
//   let data = buf.read_slice();          // &[u8] — bytes reçus
//
//   // Réponse in-place sur le même slot (zero-alloc) :
//   buf.write_slice().copy_from_slice(&response);
//   buf.commit(response.len() as u32);   // user_data géré en interne
//
//   // Ou : allouer un WBuffer séparé (fan-out pub/sub) :
//   let mut wbuf = onlyio::alloc_buffer();
//   wbuf.copy_from_slice(&response);
//   let w2 = wbuf.clone();               // O(1) — même slot, rc++
//   wbuf.submit(fd_a, len);
//   w2.submit(fd_b, len);                // zéro copie vers N subscribers

/// Alloue un buffer write depuis le pool. Synchrone, O(1).
///
/// Panics si le write pool est épuisé (à rendre async plus tard).
pub fn alloc_buffer() -> WBuffer<1> {
    GLOBAL_RUNTIME.with(|rt| rt.borrow_mut().buffer.alloc_write())
}

/// Lecture async sur un fd fixe — retourne un `RwBuffer` quand le kernel
/// a écrit des données dedans.
///
/// Soumet un READ_FIXED et suspend la task jusqu'à la CQE.
/// Le runtime appelle `BufPool::checkout_read()` sur la CQE et wake la task.
///
/// TODO: implémenter le waker + op slot quand le système de tasks est en place.
pub async fn read(_fd_idx: u32) -> RwBuffer<1> {
    todo!("read async — nécessite waker + op slot")
}
