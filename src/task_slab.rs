use std::{
    alloc::{Layout, alloc, realloc},
    mem::ManuallyDrop,
    num::NonZeroU32,
    ops::{Index, IndexMut},
    ptr::NonNull,
    u32,
};

use crate::task::Task;

/// Un slot du slab : soit une Task en cours d'exécution, soit le next index libre.
///
/// Tagging sur le bit 0 de `idx[0]` (les 32 bits bas de l'union) :
/// - bit 0 == 0  →  Task   (garanti car `align_of::<Task>() >= 8`, low 3 bits = 0)
/// - bit 0 == 1  →  index libre  (encodé comme `(next << 1) | 1`)
union SlabSlot {
    task: ManuallyDrop<Task>,
    idx: [u32; 2],
}

impl SlabSlot {
    #[inline]
    fn is_free(&self) -> bool {
        unsafe { self.idx[0] & 1 == 1 }
    }
    #[inline]
    fn is_task(&self) -> bool {
        unsafe { self.idx[0] & 1 == 0 }
    }

    /// Retourne l'index suivant encodé dans ce slot libre.
    #[inline]
    fn next_free(&self) -> u32 {
        debug_assert!(self.is_free());
        unsafe { self.idx[0] >> 1 }
    }
}

impl From<Task> for SlabSlot {
    fn from(t: Task) -> Self {
        Self {
            task: ManuallyDrop::new(t),
        }
    }
}

/// Encode un index libre dans un slot.
/// Le bit 0 est mis à 1 pour distinguer des Task ptrs (qui ont toujours bit 0 = 0).
#[inline]
fn free_slot(next: u32) -> SlabSlot {
    SlabSlot {
        idx: [(next << 1) | 1, 0],
    }
}

impl Drop for SlabSlot {
    fn drop(&mut self) {
        if self.is_free() {
            return;
        }
        // SAFETY: le slot contient une Task — ManuallyDrop empêche le double-drop.
        unsafe { ManuallyDrop::drop(&mut self.task) };
    }
}

// ─────────────────────────────────────────────────────────────────────────────

pub(crate) struct SlabIdx(u32);

/// Slab de Tasks avec free list intrusive.
///
/// Invariant : `head` est toujours un index valide vers le prochain slot libre,
/// ou `head == len` quand le slab est plein (déclenche `growth` au prochain `insert`).
pub(crate) struct Slab {
    ptr: NonNull<SlabSlot>,
    /// Index du prochain slot libre. `head == len` → slab plein.
    head: u32,
    len: u32,
}

impl Slab {
    /// Construit un slab avec une capacité initiale.
    ///
    /// # Panics
    /// `capacity` doit être > 0 et <= `u32::MAX / 2`.
    pub(crate) fn with_capacity(capacity: NonZeroU32) -> Self {
        let cap = capacity.get();
        assert!(cap <= u32::MAX / 2, "capacity dépasse u32::MAX / 2");

        let layout = Self::layout_for(cap);
        let raw = unsafe { alloc(layout) } as *mut SlabSlot;
        assert!(!raw.is_null(), "Slab alloc failed");
        let ptr = unsafe { NonNull::new_unchecked(raw) };

        // Chaîne la free list : slot[i] → i+1, sentinel slot[cap-1] → cap (= len).
        for i in 0..cap - 1 {
            unsafe { ptr.add(i as usize).write(free_slot(i + 1)) };
        }
        unsafe { ptr.add(cap as usize - 1).write(free_slot(cap)) }; // sentinel → cap

        Self {
            ptr,
            head: 0,
            len: cap,
        }
    }

    /// Insère une task et retourne son index.
    pub(crate) fn insert(&mut self, task: Task) -> SlabIdx {
        if self.head == self.len {
            self.growth();
        }
        let slot = unsafe { self.ptr.add(self.head as usize).as_mut() };
        let idx = SlabIdx(self.head);
        self.head = slot.next_free(); // avance la free list avant d'écraser le slot
        unsafe { std::ptr::write(slot, SlabSlot::from(task)) };
        idx
    }

    /// Retire la task à l'index donné et la retourne.
    ///
    /// # Panics
    /// Si l'index est hors bornes ou ne pointe pas sur une Task.
    pub(crate) fn remove(&mut self, idx: SlabIdx) -> Task {
        let i = idx.0;
        assert!(
            (i as usize) < self.len as usize,
            "remove out of bounds : index={}, len={}",
            i,
            self.len
        );
        let slot = unsafe { self.ptr.add(i as usize).as_mut() };
        assert!(slot.is_task(), "remove sur un slot libre (index={})", i);

        // Lit la Task sans déclencher ManuallyDrop::drop (ptr::read = copie bitwise).
        let task = unsafe { ManuallyDrop::into_inner(std::ptr::read(&slot.task)) };
        // Réécrit le slot comme slot libre pointant sur l'ancien head.
        unsafe { std::ptr::write(slot, free_slot(self.head)) };
        // Remet l'index libéré en tête de free list.
        self.head = i;
        task
    }

    fn growth(&mut self) {
        let old_len = self.len;
        if old_len == u32::MAX {
            panic!("Slab completly full");
        }
        let new_len = old_len.checked_mul(2).unwrap_or(u32::MAX);

        let new_size = new_len as usize * size_of::<SlabSlot>();
        let new_ptr = unsafe {
            realloc(
                self.ptr.as_ptr() as *mut u8,
                Self::layout_for(old_len),
                new_size,
            )
        } as *mut SlabSlot;
        assert!(!new_ptr.is_null(), "Slab realloc failed");

        let ptr = unsafe { NonNull::new_unchecked(new_ptr) };

        // Initialise les nouveaux slots : old_len..new_len-1 → suivant, sentinel → new_len.
        for i in old_len..new_len - 1 {
            unsafe { ptr.add(i as usize).write(free_slot(i + 1)) };
        }
        unsafe { ptr.add(new_len as usize - 1).write(free_slot(new_len)) };

        // `head` était `old_len` (slab plein) → pointe maintenant sur le 1er nouveau slot.
        // Pas besoin de le modifier : head = old_len est déjà le bon index.
        self.len = new_len;
        self.ptr = ptr;
    }

    fn layout_for(len: u32) -> Layout {
        Layout::from_size_align(len as usize * size_of::<SlabSlot>(), 64).unwrap()
    }
}

impl Drop for Slab {
    fn drop(&mut self) {
        // Drop toutes les tasks encore présentes.
        for i in 0..self.len {
            let slot = unsafe { self.ptr.add(i as usize).as_mut() };
            unsafe { std::ptr::drop_in_place(slot) };
        }
        unsafe {
            std::alloc::dealloc(self.ptr.as_ptr() as *mut u8, Self::layout_for(self.len));
        }
    }
}

impl Index<SlabIdx> for Slab {
    type Output = Task;
    fn index(&self, index: SlabIdx) -> &Self::Output {
        let i = index.0 as usize;
        assert!(i < self.len as usize, "index out of bounds");
        let slot = unsafe { self.ptr.add(i).as_ref() };
        assert!(slot.is_task(), "slot libre à l'index {}", i);
        unsafe { &*slot.task }
    }
}

impl IndexMut<SlabIdx> for Slab {
    fn index_mut(&mut self, index: SlabIdx) -> &mut Self::Output {
        let i = index.0 as usize;
        assert!(i < self.len as usize, "index out of bounds");
        let slot = unsafe { self.ptr.add(i).as_mut() };
        assert!(slot.is_task(), "slot libre à l'index {}", i);
        unsafe { &mut *slot.task }
    }
}

#[cfg(test)]
mod test {
    use std::num::NonZeroU32;
    use std::task::{Context, Waker};

    use crate::{task::Task, task_slab::Slab};

    #[test]
    fn simple_test() {
        let mut slab = Slab::with_capacity(NonZeroU32::new(1).unwrap());
        let task = Task::new(async { eprintln!("hey") });
        let idx = slab.insert(task);
        assert_eq!(idx.0, 0);
        let mut task = slab.remove(idx);
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        assert!(task.poll(&mut context).is_ready());
    }

    #[test]
    fn free_list_reuse() {
        let mut slab = Slab::with_capacity(NonZeroU32::new(2).unwrap());
        let i0 = slab.insert(Task::new(async {}));
        let i1 = slab.insert(Task::new(async {}));
        assert_eq!(i0.0, 0);
        assert_eq!(i1.0, 1);

        slab.remove(i0); // libère slot 0
        let i2 = slab.insert(Task::new(async {}));
        assert_eq!(i2.0, 0, "slot 0 doit être réutilisé");
    }

    #[test]
    fn growth() {
        let mut slab = Slab::with_capacity(NonZeroU32::new(2).unwrap());
        let _i0 = slab.insert(Task::new(async {}));
        let _i1 = slab.insert(Task::new(async {}));
        // slab plein → doit grow automatiquement
        let i2 = slab.insert(Task::new(async {}));
        assert_eq!(i2.0, 2);
        assert_eq!(slab.len, 4);
    }
}
