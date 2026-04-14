//! Accumulateur de SQEs thread-local.
//!
//! Les buffers (RwBuffer, WBuffer) poussent leurs SQEs ici pendant le poll
//! des tasks. La boucle d'événements draine la queue et les soumet tous en
//! un seul `io_uring_enter` — c'est le batching SQE décrit dans CLAUDE.md.
//!
//! Séparé de GLOBAL_RUNTIME pour éviter le double-borrow RefCell quand un
//! Drop ou un commit se produit pendant le poll.

use std::cell::RefCell;

use io_uring::squeue;

thread_local! {
    static PENDING: RefCell<Vec<squeue::Entry>> = const { RefCell::new(Vec::new()) };
}

/// Empile un SQE dans la queue. O(1) amorti.
#[inline]
pub(crate) fn push(entry: squeue::Entry) {
    PENDING.with(|q| q.borrow_mut().push(entry));
}

/// Transfère tous les SQEs en attente dans la submission queue du ring.
/// Appelé une fois par itération de la boucle d'événements.
pub(crate) fn drain_into(ring: &mut io_uring::IoUring) {
    PENDING.with(|q| {
        let mut pending = q.borrow_mut();
        if pending.is_empty() {
            return;
        }
        let mut sq = ring.submission();
        // SAFETY: les ptrs dans les SQEs proviennent de BufPool dont la
        // durée de vie dépasse celle du ring — stables tant que Runtime vit.
        unsafe {
            sq.push_multiple(pending.as_slice())
                .expect("SQ pleine — augmenter DEFAULT_RING_ENTRIES")
        }
        pending.clear();
    });
}
