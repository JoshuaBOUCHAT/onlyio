use std::{
    alloc::{Layout, alloc, dealloc},
    future::Future,
    mem::{align_of, size_of},
    pin::Pin,
    ptr::{NonNull, drop_in_place},
    task::{Context, Poll},
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
#[repr(C)]
struct RawTask {
    poll_fn: Option<PollFn>,
    drop_fn: DropFn,
    alloc_size: u32,
    ref_counter: u16,
    _pad: [u64; 0],
}

/// Handle vers une task allouée inline (`[RawTask | F]`).
///
/// - `Clone` : O(1), bump `ref_counter` + copie du ptr (8 B).
/// - `Drop`  : décrémente `ref_counter` ; libère quand il atteint 0.
/// - `!Send + !Sync` par construction (ptr non-atomique, single-thread uniquement).
pub(crate) struct Task {
    ptr: NonNull<RawTask>,
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
                poll_fn: Some(poll_fn::<F>),
                drop_fn: drop_fn::<F>,
                alloc_size: alloc_size as u32,
                ref_counter: 1,
                _pad: [],
            });
        }

        Task {
            ptr: unsafe { NonNull::new_unchecked(raw as *mut RawTask) },
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
        unsafe { (*self.ptr.as_ptr()).ref_counter += 1 };
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
    pub(crate) fn poll(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        let raw = unsafe { self.ptr.as_mut() };
        let Some(f) = raw.poll_fn else {
            return Poll::Ready(());
        };
        let result = unsafe { f(self.data_ptr(), cx) };
        if result.is_ready() {
            raw.poll_fn = None;
        }
        result
    }
    pub(crate) unsafe fn free(&mut self) {
        let raw = unsafe { self.ptr.as_mut() };
        raw.ref_counter -= 1;
        if raw.ref_counter == 0 {
            // Future non consommée (teardown / abandon) : drop propre avant dealloc.
            if raw.poll_fn.is_some() {
                unsafe { (raw.drop_fn)(self.data_ptr()) };
            }
            let layout = unsafe {
                // SAFETY: alignement toujours align_of::<RawTask>(), jamais modifié après alloc.
                Layout::from_size_align_unchecked(raw.alloc_size as usize, align_of::<RawTask>())
            };
            // SAFETY: ptr provient de `alloc(layout)`, ref_counter == 0 → libération unique.
            unsafe { dealloc(self.ptr.as_ptr() as *mut u8, layout) };
        }
    }
}

impl Clone for Task {
    /// O(1) : bump `ref_counter` + copie du ptr.
    #[inline]
    fn clone(&self) -> Self {
        // SAFETY: single-thread, ref_counter ne peut pas déborder (u16 max = 65535 tasks en vol).
        unsafe { (*self.ptr.as_ptr()).ref_counter += 1 };
        Task { ptr: self.ptr }
    }
}

impl Drop for Task {
    #[inline]
    fn drop(&mut self) {
        unsafe {
            self.free();
        }
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
        assert_eq!(size_of::<RawTask>(), 24);
        assert_eq!(size_of::<Task>(), size_of::<usize>()); // thin ptr = 8 B
        assert_eq!(size_of::<Option<Task>>(), size_of::<usize>()); // niche NonNull
    }

    #[test]
    fn ready_future() {
        let mut t = Task::new(async {});
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(t.poll(&mut cx).is_ready());
        // ref_counter toujours 1 : seul handle, pas de SQEs
        assert_eq!(unsafe { t.ptr.as_ref().ref_counter }, 1);
        // drop implicite ici → dealloc
    }

    #[test]
    fn clone_bump_et_drop_dec() {
        let t1 = Task::new(async {});
        assert_eq!(unsafe { t1.ptr.as_ref().ref_counter }, 1);

        let t2 = t1.clone();
        assert_eq!(unsafe { t1.ptr.as_ref().ref_counter }, 2);

        drop(t2);
        assert_eq!(unsafe { t1.ptr.as_ref().ref_counter }, 1);
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
        assert_eq!(unsafe { t.ptr.as_ref().ref_counter }, 1);

        let raw = t.into_raw();
        // ref_counter inchangé (transfert de propriété, pas de décrément)
        let raw_task = unsafe { raw.cast::<RawTask>().as_ref() };
        assert_eq!(raw_task.ref_counter, 1);

        let t2 = unsafe { Task::from_raw(raw) };
        assert_eq!(unsafe { t2.ptr.as_ref().ref_counter }, 1);
        // drop(t2) → dealloc
    }
}
