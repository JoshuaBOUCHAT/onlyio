use std::{
    alloc::{Layout, alloc, dealloc},
    cell::UnsafeCell,
    future::Future,
    mem::{align_of, size_of},
    ops::Add,
    pin::Pin,
    ptr::{NonNull, drop_in_place},
    task::{Context, Poll, Waker},
};

use crate::{
    buf_pool::WBuffer,
    runtime::{GLOBAL_RUNTIME, RUNTIME_FREE_LIST},
    task_slab::SlabIdx,
};

type PollFn = unsafe fn(*mut u8, &mut Context<'_>) -> Poll<()>;
type DropFn = unsafe fn(*mut u8);

/// Header interne. Invisible hors de ce module — abstraction totale.
///
/// Layout (64-bit) :
/// ```text
/// offset  0 : poll_fn      Option<PollFn>   8 B
/// offset  8 : drop_fn      DropFn           8 B
/// offset 16 : alloc_size   u32              4 B
/// offset 20 : ref_counter  u16              2 B
/// offset 22 : _pad         [u64; 0]         0 B  ─┐ 2 B padding implicite
/// offset 24 : [F inline, implicite]               ─┘
/// sizeof = 24 B
/// ```
#[repr(C, align(32))]

struct RawTask {
    poll_fn: PollFn,
    drop_fn: DropFn,
    alloc_size: u32,
    ref_counter: UnsafeCell<u16>,
    syscall_nb: UnsafeCell<u16>,
    slab_idx: Option<SlabIdx>,
}

/// Handle vers une task allouée inline (`[RawTask | F]`).
///
/// - `Clone` : O(1), bump `ref_counter` + copie du ptr (8 B).
/// - `Drop`  : décrémente `ref_counter` ; libère quand il atteint 0.
/// - `!Send + !Sync` par construction (ptr non-atomique, single-thread uniquement).
pub(crate) struct Task {
    ptr: NonNull<RawTask>,
}

pub(crate) enum SyscallNbCompResult {
    Expired,
    Normal,
    Multiple,
    Error,
}

impl Task {
    /// Alloue une task contenant `future` inline.
    ///
    /// Layout : `[RawTask | F]`, une seule alloc. `ref_counter` démarre à 1.
    /// Après construction, `F` est complètement effacé (type erasure via fn ptrs).
    ///
    /// # Panics
    /// Si `align_of::<F>() > align_of::<RawTask>()` (> 8).
    pub(crate) fn new<F: Future<Output = ()>>(future: F) -> Self {
        assert!(
            align_of::<F>() <= align_of::<RawTask>(),
            "align_of::<F>() = {} dépasse align_of::<RawTask>() = {} — non supporté",
            align_of::<F>(),
            align_of::<RawTask>(),
        );

        let (layout, future_offset) = Layout::new::<RawTask>()
            .extend(Layout::new::<F>())
            .expect("layout overflow");
        let layout = layout.pad_to_align();

        debug_assert_eq!(future_offset, size_of::<RawTask>());

        let alloc_size = layout.size();
        assert!(
            alloc_size <= u32::MAX as usize,
            "alloc_size dépasse u32::MAX"
        );

        // SAFETY: layout.size() > 0 — RawTask n'est pas un ZST.
        let raw = unsafe { alloc(layout) };
        assert!(!raw.is_null(), "task allocation échouée");

        // Écrit la future avant le header : si `write` panique (F partiellement construit),
        // le header non initialisé n'est jamais lu.
        let future_ptr = unsafe { raw.add(future_offset) as *mut F };
        unsafe { future_ptr.write(future) };

        // SAFETY: raw est valide pour l'écriture de RawTask.
        unsafe {
            (raw as *mut RawTask).write(RawTask {
                poll_fn: poll_fn::<F>,
                drop_fn: drop_fn::<F>,
                alloc_size: alloc_size as u32,
                ref_counter: UnsafeCell::new(1),
                syscall_nb: UnsafeCell::new(0),
                slab_idx: None,
            });
        }

        Task {
            ptr: unsafe { NonNull::new_unchecked(raw as *mut RawTask) },
        }
    }
    pub(crate) fn get_syscall_nb(&self) -> u16 {
        unsafe { *(*self.ptr.as_ptr()).syscall_nb.get() }
    }
    pub(crate) fn evaluate_syscall_nb(&self, incoming: u16) -> SyscallNbCompResult {
        const MULTIPLE_MASK: u16 = 0x8000;
        let stored = self.get_syscall_nb();
        let stored_rank = stored & !MULTIPLE_MASK;
        let incoming_rank = incoming & !MULTIPLE_MASK;
        if incoming_rank < stored_rank {
            return SyscallNbCompResult::Expired;
        }
        if incoming_rank > stored_rank {
            return SyscallNbCompResult::Error;
        }
        let stored_multiple = stored & MULTIPLE_MASK;
        let incoming_multiple = incoming & MULTIPLE_MASK;
        if stored_multiple != incoming_multiple {
            return SyscallNbCompResult::Error;
        }
        if stored_multiple != 0 {
            return SyscallNbCompResult::Multiple;
        } else {
            return SyscallNbCompResult::Normal;
        }
    }
    pub(crate) fn augmente_syscall_nb(&mut self) {
        unsafe {
            let nb_syscall = self.ptr.as_mut().syscall_nb.get_mut();
            *nb_syscall = *nb_syscall + 1;
        }
    }

    /// Pose MULTIPLE_MASK sur syscall_nb — appelé quand un SQE multishot est soumis.
    pub(crate) fn set_multishot(&mut self) {
        unsafe {
            let nb = self.ptr.as_mut().syscall_nb.get_mut();
            *nb |= 0x8000;
        }
    }

    /// Pointeur vers le début de `F` dans l'alloc (juste après le header).
    #[inline]
    fn data_ptr(&self) -> *mut u8 {
        // SAFETY: future_offset == size_of::<RawTask>() (garanti à la construction).
        unsafe { self.ptr.as_ptr().add(1) as *mut u8 }
    }

    /// Incrémente `ref_counter` pour un SQE soumis.
    /// Appeler `drop(task_clone)` quand le CQE correspondant arrive.
    #[inline]
    pub(crate) fn inc_rc(&self) {
        // SAFETY: single-thread, pas d'accès concurrent.
        unsafe {
            let rc = (*self.ptr.as_ptr()).ref_counter.get_mut();
            *rc = *rc + 1;
        };
    }

    /// Consomme le handle sans décrémenter `ref_counter`.
    /// Utiliser pour stocker un ptr opaque dans le slab (la "référence slab" est transférée).
    /// Rendre la main avec [`Task::from_raw`].
    #[inline]
    pub(crate) fn into_raw(self) -> NonNull<()> {
        let ptr = self.ptr.cast::<()>();
        std::mem::forget(self);
        ptr
    }

    /// Reconstruit un `Task` depuis un ptr opaque sans incrémenter `ref_counter`.
    ///
    /// # Safety
    /// `ptr` doit provenir de [`into_raw`] et être encore vivant.
    #[inline]
    pub(crate) unsafe fn from_raw(ptr: NonNull<()>) -> Self {
        Task {
            ptr: ptr.cast::<RawTask>(),
        }
    }

    /// Poll la task.
    ///
    /// Met `poll_fn` à `None` si la future retourne `Ready(())`.
    /// Si `poll_fn` est déjà `None`, retourne `Ready(())` immédiatement.
    ///
    /// # Safety
    /// Appelé depuis le thread runtime uniquement.
    #[inline]
    pub(crate) fn poll(&mut self) -> Poll<()> {
        let mut cx = Context::from_waker(Waker::noop());
        let raw = unsafe { self.ptr.as_mut() };

        let result = unsafe { (raw.poll_fn)(self.data_ptr(), &mut cx) };

        result
    }
    pub(crate) fn release(&mut self) {
        let raw = unsafe { self.ptr.as_mut() };
        let counter = raw.ref_counter.get_mut();
        *counter = *counter - 1;

        if *counter == 0 {
            // Drop the futur first
            unsafe { (raw.drop_fn)(self.data_ptr()) };
            let idx = raw.slab_idx;
            let layout = unsafe {
                // SAFETY: alignement toujours align_of::<RawTask>(), jamais modifié après alloc.
                Layout::from_size_align_unchecked(raw.alloc_size as usize, align_of::<RawTask>())
            };
            // SAFETY: ptr provient de `alloc(layout)`, ref_counter == 0 → libération unique.
            unsafe { dealloc(self.ptr.as_ptr() as *mut u8, layout) };

            //Once the task is totaly Drop we remove it from the slab in the runtime
            if let Some(task_idx) = idx {
                RUNTIME_FREE_LIST.with_borrow_mut(|l| l.push(task_idx))
            }
        }
    }
    fn get_rc(&self) -> u16 {
        unsafe { *self.ptr.as_ref().ref_counter.get() }
    }
    pub(crate) fn set_task_idx(&mut self, slab_idx: SlabIdx) {
        unsafe {
            self.ptr.as_mut().slab_idx = Some(slab_idx);
        }
    }
}

impl Clone for Task {
    /// O(1) : bump `ref_counter` + copie du ptr.
    #[inline]
    fn clone(&self) -> Self {
        // SAFETY: single-thread, ref_counter ne peut pas déborder (u16 max = 65535 tasks en vol).

        self.inc_rc();

        Task { ptr: self.ptr }
    }
}

impl Drop for Task {
    #[inline]
    fn drop(&mut self) {
        self.release();
    }
}

// ── fn ptrs instanciés à la construction, pendant que F est connu ────────────

/// # Safety
/// `data` pointe sur une `F` initialisée et correctement alignée.
unsafe fn poll_fn<F: Future<Output = ()>>(data: *mut u8, cx: &mut Context<'_>) -> Poll<()> {
    // SAFETY: data pointe sur une F initialisée, jamais déplacée (alloc fixe).
    let pinned = unsafe { Pin::new_unchecked(&mut *(data as *mut F)) };
    pinned.poll(cx)
}

/// # Safety
/// `data` pointe sur une `F` initialisée.
unsafe fn drop_fn<F>(data: *mut u8) {
    // SAFETY: data pointe sur une F initialisée, non encore droppée.
    unsafe { drop_in_place(data as *mut F) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::{RawWaker, RawWakerVTable, Waker};

    fn noop_waker() -> Waker {
        const VTABLE: RawWakerVTable =
            RawWakerVTable::new(|p| RawWaker::new(p, &VTABLE), |_| {}, |_| {}, |_| {});
        unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
    }

    #[test]
    fn layout() {
        assert_eq!(size_of::<RawTask>(), 32);
        assert_eq!(size_of::<Task>(), size_of::<usize>()); // thin ptr = 8 B
        assert_eq!(size_of::<Option<Task>>(), size_of::<usize>()); // niche NonNull
    }

    #[test]
    fn ready_future() {
        let mut t = Task::new(async {});
        assert!(t.poll().is_ready());
        // ref_counter toujours 1 : seul handle, pas de SQEs
        assert_eq!(unsafe { *t.ptr.as_ref().ref_counter.get() }, 1);
        // drop implicite ici → dealloc
    }

    #[test]
    fn clone_bump_et_drop_dec() {
        let t1 = Task::new(async {});
        assert_eq!(t1.get_rc(), 1);

        let t2 = t1.clone();
        assert_eq!(t1.get_rc(), 2);

        drop(t2);
        assert_eq!(t1.get_rc(), 1);
        // drop(t1) → ref_counter = 0 → dealloc
    }

    #[test]
    fn drop_fn_appelé_si_future_non_consommée() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let dropped = Arc::new(AtomicBool::new(false));

        struct DropProbe(Arc<AtomicBool>);
        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        // probe capturé dans le State 0 de la state machine → droppé même sans poll.
        let probe = DropProbe(dropped.clone());
        let t = Task::new(async move {
            let _p = probe;
            std::future::pending::<()>().await;
        });

        drop(t); // ref_counter 1 → 0 → drop_fn appelé
        assert!(dropped.load(Ordering::SeqCst), "drop_fn non appelé");
    }

    #[test]
    fn into_raw_from_raw_preserves_rc() {
        let t = Task::new(async {});
        assert_eq!(t.get_rc(), 1);

        let raw = t.into_raw();
        // ref_counter inchangé (transfert de propriété, pas de décrément)
        let raw_task = unsafe { raw.cast::<RawTask>().as_ref() };
        assert_eq!(unsafe { *raw_task.ref_counter.get() }, 1);

        let t2 = unsafe { Task::from_raw(raw) };
        assert_eq!(t2.get_rc(), 1);
        // drop(t2) → dealloc
    }
}
