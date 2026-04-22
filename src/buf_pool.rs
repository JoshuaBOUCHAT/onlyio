use std::{
    alloc::{Layout, alloc_zeroed, dealloc},
    collections::VecDeque,
    mem,
    ops::{Deref, DerefMut},
};

use io_uring::{opcode, types};

use crate::{
    calls::write::{WriteResult, write_multiple},
    runtime::sqe_queue_push,
};

const PAGE_SIZE: usize = 4096;

// ============================================================================
// user_data interne — invisible de l'API publique
// ============================================================================
//
// Layout task CQE (bit 63 = 0) :  [ op_idx 32b ][ generation 16b ][ task_idx 16b ]
// Layout interne  (bit 63 = 1) :  [ type 16b ][ payload 32b ]
//
// Le runtime dispatche sur bit 63 en premier.

const INTERNAL_FLAG: u64 = 1 << 63;

/// WRITE_FIXED issu de RwBuffer::commit — le runtime ignore ce CQE (pas de pool à libérer).
const RWBUF_WRITE_UDATA: u64 = INTERNAL_FLAG | (1 << 32);

/// WRITE_FIXED issu de WBuffer::submit — le runtime appelle on_write_cqe(abs_idx).
#[inline]
pub(crate) fn wbuf_write_udata(abs_idx: u32) -> u64 {
    INTERNAL_FLAG | abs_idx as u64
}

/// Décode abs_idx depuis un user_data de WBuffer write. Panics si pas un udata WBuffer.
#[inline]
pub(crate) fn decode_wbuf_udata(udata: u64) -> u32 {
    debug_assert!(udata & INTERNAL_FLAG != 0 && udata >> 32 == 1);
    (udata & 0xFFFF_FFFF) as u32
}

/// Vrai si le CQE est interne (pas de task à réveiller).
#[inline]
pub(crate) fn is_internal_cqe(udata: u64) -> bool {
    udata & INTERNAL_FLAG != 0
}

// ============================================================================
// BufPool
// ============================================================================
//
// Allocation unique, deux zones logiques :
//
//   [ 0 .. read_count )       — read pool  : slots RwBuffer, jamais dans une
//                               free list, toujours re-soumis directement.
//   [ read_count .. total )   — write pool : slots WBuffer, gérés par write_free.
//
// Index absolu = position dans la table register_buffers du kernel.
// Les fd sont toujours des types::Fixed(idx) — aucun raw fd.

pub struct BufPool<const N: usize> {
    base: *mut u8,
    layout: Layout,
    pub read_count: u32,
    write_free: VecDeque<u32>,
    /// rc par slot write, indexé par (abs_idx - read_count).
    /// 0 = libre, 1..MAX-1 = N handles en vol, MAX = jamais utilisé ici.
    write_rc: Box<[u32]>,
}

impl<const N: usize> BufPool<N> {
    pub const BUF_SIZE: usize = N * PAGE_SIZE;

    pub fn new(read_count: u32, write_count: u32) -> Self {
        assert!(read_count > 0 && write_count > 0);
        let total = (read_count + write_count) as usize;
        let layout = Layout::from_size_align(total * Self::BUF_SIZE, PAGE_SIZE)
            .expect("BufPool: layout invalide");

        // SAFETY: layout non-nul et page-aligné.
        let base = unsafe { alloc_zeroed(layout) };
        assert!(!base.is_null(), "BufPool: OOM");

        Self {
            base,
            layout,
            read_count,
            write_free: (read_count..read_count + write_count).collect(),
            write_rc: vec![0u32; write_count as usize].into_boxed_slice(),
        }
    }

    /// Pointeur vers le buffer write d'index absolu `buf_id` (0..write_count dans register_buffers).
    #[inline]
    pub fn write_buf_ptr(&self, buf_id: u16) -> *const u8 {
        let abs = self.read_count as usize + buf_id as usize;
        // SAFETY: abs < total garanti par l'appelant (buf_id < write_count).
        unsafe { self.base.add(abs * Self::BUF_SIZE) }
    }

    /// Pointeur vers le buffer read d'index `buf_idx` dans l'allocation.
    /// Utilisé par le runtime pour construire les entrées du buf_ring.
    #[inline]
    pub(crate) fn read_buf_addr(&self, buf_idx: u32) -> *const u8 {
        debug_assert!(buf_idx < self.read_count);
        // SAFETY: buf_idx < read_count, layout contigu.
        unsafe { self.base.add(buf_idx as usize * Self::BUF_SIZE) }
    }

    /// Table d'`iovec` couvrant TOUS les slots (read + write) pour
    /// `register_buffers`. Éphémère — à dropper après le syscall.
    pub fn build_iovecs(&self) -> Vec<libc::iovec> {
        let total = self.read_count as usize + self.write_rc.len();
        (0..total)
            .map(|i| libc::iovec {
                // SAFETY: i < total, plage allouée garantie.
                iov_base: unsafe { self.base.add(i * Self::BUF_SIZE) } as *mut libc::c_void,
                iov_len: Self::BUF_SIZE,
            })
            .collect()
    }

    /// Crée un `RwBuffer` depuis une CQE d'un read multishot.
    ///
    /// La release du slot buf_ring est différée :
    /// - drop sans commit → release immédiate (userspace)
    /// - commit()         → release après la CQE du WriteFixed (via CommitFuture)
    #[inline]
    pub fn checkout_read(&self, buf_idx: u32, fd_idx: u32, bytes: u32) -> RwBuffer<N> {
        debug_assert!(buf_idx < self.read_count);
        RwBuffer {
            // SAFETY: buf_idx < read_count, layout contigu.
            ptr: unsafe { self.base.add(buf_idx as usize * Self::BUF_SIZE) },
            buf_idx,
            fd_idx,
            bytes,
            committed: false,
        }
    }

    /// Emprunte un slot write libre (rc = 1).
    /// Panics si le write pool est épuisé.
    #[inline]
    pub fn alloc_write(&mut self) -> WBuffer<N> {
        let abs_idx = self
            .write_free
            .pop_front()
            .expect("BufPool: write pool épuisé");
        let rel = (abs_idx - self.read_count) as usize;
        debug_assert_eq!(self.write_rc[rel], 0);
        self.write_rc[rel] = 1;
        WBuffer {
            // SAFETY: abs_idx < total, layout contigu.
            ptr: unsafe { self.base.add(abs_idx as usize * Self::BUF_SIZE) },
            abs_idx,
            pool: self as *mut Self,
        }
    }

    /// Appelé par le runtime sur chaque CQE de WRITE_FIXED.
    /// Décrémente rc — remet le slot dans write_free si rc atteint 0
    /// (fan-out : le dernier CQE libère).
    #[inline]
    pub fn on_write_cqe(&mut self, abs_idx: u32) {
        let rel = (abs_idx - self.read_count) as usize;
        debug_assert!(self.write_rc[rel] > 0, "on_write_cqe: slot déjà libre");
        self.write_rc[rel] -= 1;
        if self.write_rc[rel] == 0 {
            self.write_free.push_back(abs_idx);
        }
    }
}

impl<const N: usize> Drop for BufPool<N> {
    fn drop(&mut self) {
        // SAFETY: base alloué avec ce layout exact.
        unsafe { dealloc(self.base, self.layout) };
    }
}

// ============================================================================
// RwBuffer — slot read/write
// ============================================================================
//
// Créé par BufPool::checkout_read() sur CQE d'un READ_FIXED.
//
// Deux destins :
//   drop (committed=false) → READ_FIXED replenish dans sqe_queue
//   commit(len, user_data) → WRITE_FIXED(IO_LINK) + READ_FIXED dans sqe_queue
//
// write_slice() est safe car :
//   - le kernel a rendu le buffer via la CQE (committed=false à ce stade)
//   - aucun SQE n'est soumis avant commit(self)
//   - commit prend self par move → ne peut être appelé qu'une fois

pub struct RwBuffer<const N: usize> {
    ptr: *mut u8,
    pub buf_idx: u32,
    fd_idx: u32,
    /// Octets écrits par le kernel — peut être < BUF_SIZE (donnée au runtime).
    pub bytes: u32,
    /// true si commit() a été appelé → CommitFuture gère la release du buf_ring.
    committed: bool,
}

impl<const N: usize> RwBuffer<N> {
    /// Slice sur les octets effectivement reçus du kernel.
    #[inline]
    pub fn read_slice(&self) -> &[u8] {
        // SAFETY: ptr valide pour BUF_SIZE, bytes garanti <= BUF_SIZE par le kernel.
        unsafe { std::slice::from_raw_parts(self.ptr as *const u8, self.bytes as usize) }
    }

    /// Slice mutable sur le buffer entier pour écrire la réponse en place.
    ///
    /// Safe : kernel a rendu le buffer (CQE reçue), aucun SQE soumis avant commit(self).
    #[inline]
    pub fn write_slice(&mut self) -> &mut [u8] {
        // SAFETY: voir commentaire de struct.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, BufPool::<N>::BUF_SIZE) }
    }

    /// Soumet un WriteFixed asynchrone et libère le slot buf_ring après confirmation.
    /// Consomme self — ne peut être appelé qu'une fois.
    pub async fn commit(mut self, len: u32) -> i32 {
        self.committed = true;
        crate::calls::commit::make_commit_future::<N>(self.ptr, self.buf_idx, self.fd_idx, len)
            .await
    }
}

impl<const N: usize> Drop for RwBuffer<N> {
    fn drop(&mut self) {
        if self.committed {
            // CommitFuture gère la release après la CQE du write.
            return;
        }
        // Lecture seule terminée (pas de commit) : rendre le slot au buf_ring immédiatement.
        // Écriture userspace, zéro syscall.
        crate::runtime::release_read_buf(self.buf_idx);
    }
}

// ============================================================================
// WBuffer — slot write avec rc pour fan-out
// ============================================================================
//
// rc sémantique (dans BufPool::write_rc) :
//   0              → libre (dans write_free)
//   1 .. MAX       → N handles en vie (user) ou en vol (kernel)
//
// submit(fd_idx, len, user_data) :
//   push WRITE_FIXED + mem::forget(self) → rc inchangé.
//   Le rc sera décrémenté par BufPool::on_write_cqe() à chaque CQE.
//
// Drop (non soumis) :
//   rc-- ; si rc == 0 → write_free.
//
// Fan-out : alloc(rc=1) + clone×(N-1) (rc=N) + submit×N (forget×N).
//   N CQEs arrivent, chacune appelle on_write_cqe → rc-- → libre quand rc=0.

pub struct WBuffer<const N: usize> {
    ptr: *mut u8,
    abs_idx: u32,
    pool: *mut BufPool<N>,
}

impl<const N: usize> WBuffer<N> {
    /// Slice mutable sur le buffer entier.
    #[inline]
    pub fn write_slice(&mut self) -> &mut [u8] {
        // SAFETY: slot write-pool exclusif tant que rc >= 1 et non soumis.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, BufPool::<N>::BUF_SIZE) }
    }

    /// Index absolu dans la table register_buffers — champ buf_index du SQE.
    #[inline]
    pub fn index(&self) -> u16 {
        self.abs_idx as u16
    }

    /// Soumet un WRITE_FIXED dans sqe_queue. Consomme self sans décrémenter rc —
    /// la libération se fait via BufPool::on_write_cqe() à la CQE.
    pub fn submit(self, fd_idx: u32, len: u32) {
        let write =
            opcode::WriteFixed::new(types::Fixed(fd_idx), self.ptr, len, self.abs_idx as u16)
                .build()
                .user_data(wbuf_write_udata(self.abs_idx));

        sqe_queue_push(write);
    }
    pub(crate) fn write(self, fd_idx: u32, len: u32, user_data: u64) -> io_uring::squeue::Entry {
        opcode::WriteFixed::new(types::Fixed(fd_idx), self.ptr, len, self.abs_idx as u16)
            .build()
            .user_data(user_data)
    }
}
impl WBuffer<1> {
    pub async fn write_multiple(self, fds: Vec<u32>, len: u32) -> Vec<WriteResult> {
        write_multiple(fds, self, len).await
    }
}

impl<const N: usize> Clone for WBuffer<N> {
    /// Fan-out O(1). Incrémente rc.
    #[inline]
    fn clone(&self) -> Self {
        let pool = unsafe { &mut *self.pool };
        let rel = (self.abs_idx - pool.read_count) as usize;
        debug_assert!(pool.write_rc[rel] > 0, "clone sur slot libre");
        pool.write_rc[rel] += 1;
        WBuffer {
            ptr: self.ptr,
            abs_idx: self.abs_idx,
            pool: self.pool,
        }
    }
}

impl<const N: usize> Drop for WBuffer<N> {
    #[inline]
    fn drop(&mut self) {
        let pool = unsafe { &mut *self.pool };
        let rel = (self.abs_idx - pool.read_count) as usize;
        debug_assert!(pool.write_rc[rel] > 0, "drop sur slot déjà libre");
        pool.write_rc[rel] -= 1;
        if pool.write_rc[rel] == 0 {
            pool.write_free.push_back(self.abs_idx);
        }
    }
}

impl<const N: usize> Deref for WBuffer<N> {
    type Target = [u8];
    #[inline]
    fn deref(&self) -> &[u8] {
        // SAFETY: ptr valide pour BUF_SIZE tant que WBuffer est en vie.
        unsafe { std::slice::from_raw_parts(self.ptr, N * PAGE_SIZE) }
    }
}
impl<const N: usize> DerefMut for WBuffer<N> {
    #[inline]
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: ptr valide pour BUF_SIZE tant que WBuffer est en vie.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, N * PAGE_SIZE) }
    }
}
