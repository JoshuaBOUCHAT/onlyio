use std::{cell::RefCell, num::NonZero};

use io_uring::IoUring;

use crate::{IOResult, buf_pool::BufPool, sqe_queue, task_slab::Slab};

/// Profondeur de la SQ/CQ par défaut. Doit être une puissance de 2.
const DEFAULT_RING_ENTRIES: u32 = 1024;

/// Nombre maximum de connexions simultanées (taille de la fixed files table).
const DEFAULT_MAX_CONNECTIONS: u32 = 1024;

thread_local! {
    pub static GLOBAL_RUNTIME: RefCell<Runtime> = RefCell::new(
        Runtime::new(DEFAULT_RING_ENTRIES, DEFAULT_MAX_CONNECTIONS).expect("Runtime creation failed")
    );
}

pub(crate) struct Runtime {
    tasks: Slab,
    ring: IoUring,
    pub(crate) buffer: BufPool<1>,
}

impl Runtime {
    /// Initialise le ring io_uring et enregistre la fixed files table sparse.
    ///
    /// - `entries` : profondeur SQ/CQ (puissance de 2, ex. 256)
    /// - `max_connections` : nombre de slots dans la fixed files table
    ///
    /// `IORING_SETUP_SQPOLL` est intentionnellement absent : optionnel,
    /// activable via `IoUring::builder().setup_sqpoll(idle_ms)` si besoin.
    fn new(entries: u32, max_connections: u32) -> IOResult<Self> {
        let ring = IoUring::builder()
            .build(entries)
            .expect("io_uring: échec de la création du ring");

        // SAFETY: syscall bloquant one-shot à l'init.
        // Réserve `max_connections` slots dans la fixed files table du kernel.
        // Mémoire virtuelle uniquement — aucune page physique engagée avant ACCEPT_DIRECT.
        ring.submitter().register_files_sparse(max_connections)?;

        // 512 slots read (kernel), 512 slots write (user).
        let mut buffer = BufPool::new(512, 512);
        let io_vec = buffer.build_iovecs();
        unsafe { ring.submitter().register_buffers(&io_vec)? };
        let tasks = Slab::with_capacity(NonZero::new(16).unwrap());
        Ok(Self {
            tasks,
            ring,
            buffer,
        })
    }
    /// Draine sqe_queue dans la SQ puis soumet via io_uring_enter.
    /// Appelé à chaque fin d'itération de la boucle d'événements.
    fn flush_sqes(&mut self) {
        sqe_queue::drain_into(&mut self.ring);
        // Le submit effectif se fait au prochain io_uring_enter (submit_and_wait).
    }
}
