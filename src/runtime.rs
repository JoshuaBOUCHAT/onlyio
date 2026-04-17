use std::{
    alloc::{Layout, alloc_zeroed, dealloc},
    cell::{Cell, RefCell},
    num::NonZero,
    u16, u32,
};

const PAGE_SIZE: usize = 4096;

use io_uring::{IoUring, squeue::Entry};

use crate::{
    IOResult,
    buf_pool::BufPool,
    task::{SyscallNbCompResult, Task},
    task_slab::{Slab, SlabIdx},
};

/// Profondeur de la SQ/CQ par défaut. Doit être une puissance de 2.
const DEFAULT_RING_ENTRIES: u32 = 1024;

/// Nombre maximum de connexions simultanées (taille de la fixed files table).
const DEFAULT_MAX_CONNECTIONS: u32 = 1024;

thread_local! {
    pub static GLOBAL_RUNTIME: RefCell<Runtime> = RefCell::new(
        Runtime::new(DEFAULT_RING_ENTRIES, DEFAULT_MAX_CONNECTIONS).expect("Runtime creation failed")
    );
    pub static CURRENT_TASK:Cell<Option<TaskInfo>>=Cell::new(None);
    pub static RUNTIME_FREE_LIST:RefCell<Vec<SlabIdx>>=RefCell::new(Vec::new());
}

pub(crate) struct Runtime {
    tasks: Slab,
    ring: IoUring,
    pub(crate) buffer: BufPool<1>,
    pending: Vec<Entry>,
    buf_ring: *mut u8,
    buf_ring_layout: Layout,
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
        let buffer = BufPool::new(512, 512);
        let io_vec = buffer.build_iovecs();

        // Tous les buffers (read + write) → register_buffers (fixed I/O)
        unsafe { ring.submitter().register_buffers(&io_vec)? };

        // Read buffers → register_buf_ring (BUFFER_SELECT, bgid=0)
        let read_count = buffer.read_count as usize;
        let buf_ring_layout = Layout::from_size_align(read_count * 16, PAGE_SIZE).unwrap();
        // SAFETY: layout non-nul et page-aligné — requis par register_buf_ring.
        let buf_ring = unsafe { alloc_zeroed(buf_ring_layout) };
        assert!(!buf_ring.is_null(), "buf_ring: OOM");

        for i in 0..read_count {
            let entry = unsafe { buf_ring.add(i * 16) };
            // SAFETY: entrées contigues de 16 octets, index dans les bornes.
            unsafe {
                (entry as *mut u64).write(io_vec[i].iov_base as u64); // addr
                (entry.add(8) as *mut u32).write(io_vec[i].iov_len as u32); // len
                (entry.add(12) as *mut u16).write(i as u16); // bid
            }
        }
        // tail = read_count : tous les buffers disponibles dès le départ.
        // Stocké à entry[0].resv (offset 14) — overlap intentionnel du kernel ABI.
        unsafe { (buf_ring.add(14) as *mut u16).write(read_count as u16) };

        unsafe {
            ring.submitter().register_buf_ring_with_flags(
                buf_ring as u64,
                read_count as u16,
                0,
                0,
            )?
        };

        let tasks = Slab::with_capacity(NonZero::new(16).unwrap());
        let pending = Vec::new();
        Ok(Self {
            tasks,
            ring,
            buffer,
            pending,
            buf_ring,
            buf_ring_layout,
        })
    }
    fn submit_and_wait(&mut self) -> IOResult<usize> {
        {
            if self.pending.is_empty() {
                panic!("call submit on 0 entry");
            }
            let mut sq = self.ring.submission();
            // SAFETY: les ptrs dans les SQEs proviennent de BufPool dont la
            // durée de vie dépasse celle du ring — stables tant que Runtime vit.

            unsafe {
                sq.push_multiple(&self.pending)
                    .expect("SQ pleine — augmenter DEFAULT_RING_ENTRIES")
            }
            self.pending.clear();
        }

        self.ring.submit_and_wait(1)
    }
    fn sqe_queue_push(&mut self, entry: Entry) {
        self.pending.push(entry);
    }

    /// Insère la future dans le slab et retourne son index — sans la poller.
    fn register_future(&mut self, future: impl Future<Output = ()>) -> SlabIdx {
        let task = Task::new(future);
        self.tasks.insert(task)
    }

    pub(crate) fn block_on(future: impl Future<Output = ()>) -> IOResult<()> {
        // Insertion dans le slab — borrow libéré immédiatement après.
        let task_idx = GLOBAL_RUNTIME.with(|rt| rt.borrow_mut().register_future(future));

        // Premier poll hors borrow : la future peut appeler submit_xxx librement.
        {
            let mut task = GLOBAL_RUNTIME.with_borrow(|rt| rt.tasks[task_idx].clone());
            CURRENT_TASK.set(Some(TaskInfo::initial_poll(task_idx)));
            let _ = task.poll();
            println!("Test RC: {}", task.get_rc());
            task.release();
            println!("Test RC after release: {}", task.get_rc());
        }
        println!(
            "slab_count: {}",
            GLOBAL_RUNTIME.with_borrow_mut(|rt| rt.tasks.count())
        );
        println!("Droped");
        println!(
            "free list_size: {}",
            RUNTIME_FREE_LIST.with_borrow(|l| l.len())
        );
        free_unsed_slab_slot();

        loop {
            // poll ready tasks
            let maybe_submit = GLOBAL_RUNTIME.with_borrow_mut(|rt| {
                if !rt.pending.is_empty() {
                    println!("Submit {} sqe", rt.pending.len());
                    Some(rt.submit_and_wait())
                } else {
                    None
                }
            });
            if let Some(submit) = maybe_submit {
                if submit? == 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            let entries: Vec<_> =
                GLOBAL_RUNTIME.with(|rt| rt.borrow_mut().ring.completion().collect()); //Collect the entries outside the borow
            // to allow to borow GLOBAL_RUNTIME inside entry handling
            println!("Recieve {} CQE", entries.len());
            if entries.len() == 0 {
                panic!("Entries len ==0 runtime shouldnt been wakeup");
            }

            for entry in entries {
                // CQEs internes (WBuffer writes, etc.) — pas de task à réveiller.
                /*if crate::buf_pool::is_internal_cqe(entry.user_data()) {
                    continue;
                }*/

                // IORING_CQE_F_MORE : le kernel a encore des CQEs à envoyer pour ce SQE
                // (multishot kernel : accept_multishot, recv_multishot…).
                // Si set → ne pas appeler release() — le rc a été incrémenté une seule fois
                // pour ce SQE et doit survivre jusqu'à la CQE finale.

                let is_buffer = io_uring::cqueue::buffer_more(entry.flags());
                let is_multishoot = io_uring::cqueue::more(entry.flags());
                if is_more {
                    println!("Recieve a is more");
                }

                let task_info = TaskInfo::from_entry(&entry);
                let nb_syscall = task_info.tag.syscall_nb;
                let task_idx = task_info.tag.task_idx;
                let mut task = GLOBAL_RUNTIME.with_borrow_mut(|rt| rt.tasks[task_idx].clone());

                match task.evaluate_syscall_nb(nb_syscall) {
                    SyscallNbCompResult::Error => {
                        panic!("Recieve a SQE newer than expected")
                    }
                    SyscallNbCompResult::Expired => {}
                    SyscallNbCompResult::Normal => {
                        let mut task_info = task_info;
                        task_info.tag.syscall_nb += 1;
                        task.augmente_syscall_nb();
                        CURRENT_TASK.set(Some(task_info));
                        let _ = task.poll();
                    }
                    SyscallNbCompResult::Multiple => {
                        println!("Recieve a multiple");
                        // Batch SQEs ou multishot kernel : toujours poller.
                        // Release uniquement sur la CQE finale (pas de F_MORE).
                        CURRENT_TASK.set(Some(task_info));
                        let _ = task.poll();
                    }
                }
                //Should always release the task expect in is_more mode
                if !is_more {
                    task.release();
                }
            }
            free_unsed_slab_slot(); //Use the free list to clear finished task (use this mekanism of extern free list because of the cross borowing problem)

            if GLOBAL_RUNTIME.with(|rt| rt.borrow().tasks.count()) == 0 {
                break;
            }
        }
        Ok(())
    }
    /// user_data pour la task courante. `multishot=true` pose le MULTIPLE_MASK sur syscall_nb.
    fn current_udata(&self, multishot: bool) -> u64 {
        CURRENT_TASK.with(|c| {
            let info = c.get().expect("appelé hors contexte runtime");
            let syscall_nb = if multishot {
                info.tag.syscall_nb | 0x8000 // MULTIPLE_MASK
            } else {
                info.tag.syscall_nb
            };
            UserDataPayload {
                task_idx: info.tag.task_idx,
                response_gen: 0,
                syscall_nb,
            }
            .into_user_data()
        })
    }

    fn send_read(&mut self, fd: u32) {
        let udata = self.current_udata(false);
        let task_idx = CURRENT_TASK.with(|c| c.get().unwrap().tag.task_idx);
        self.tasks[task_idx].inc_rc();
        let sqe = io_uring::opcode::Read::new(
            io_uring::types::Fixed(fd),
            std::ptr::null_mut(),
            BufPool::<1>::BUF_SIZE as u32,
        )
        .buf_group(0)
        .build()
        .flags(io_uring::squeue::Flags::BUFFER_SELECT)
        .user_data(udata);
        self.pending.push(sqe);
    }

    /// ACCEPT_DIRECT single-shot : une connexion entrante → un fixed_file_index.
    /// `listen_fd` est le fd du socket en écoute (non-fixed).
    fn send_accept(&mut self, listen_fd: i32) {
        let udata = self.current_udata(false);
        let task_idx = CURRENT_TASK.with(|c| c.get().unwrap().tag.task_idx);
        self.tasks[task_idx].inc_rc();
        let sqe = io_uring::opcode::Accept::new(
            io_uring::types::Fd(listen_fd),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
        .file_index(Some(io_uring::types::DestinationSlot::auto_target()))
        .build()
        .user_data(udata);
        self.pending.push(sqe);
    }

    /// WRITE_FIXED : envoie `len` octets du buffer `buf_id` sur `fd` (fixed file).
    fn send_write(&mut self, fd: u32, buf_id: u16, len: u32) {
        let udata = self.current_udata(false);
        let task_idx = CURRENT_TASK.with(|c| c.get().unwrap().tag.task_idx);
        self.tasks[task_idx].inc_rc();
        let buf_ptr = self.buffer.write_buf_ptr(buf_id);
        let sqe =
            io_uring::opcode::WriteFixed::new(io_uring::types::Fixed(fd), buf_ptr, len, buf_id)
                .build()
                .user_data(udata);
        self.pending.push(sqe);
    }

    /// RECV_MULTISHOT : un seul SQE, N CQEs via IORING_CQE_F_MORE.
    /// MULTIPLE_MASK posé → la task est repollée sur chaque CQE.
    fn send_recv_multishot(&mut self, fd: u32) {
        let udata = self.current_udata(true); // MULTIPLE_MASK
        let task_idx = CURRENT_TASK.with(|c| c.get().unwrap().tag.task_idx);
        self.tasks[task_idx].set_multishot();
        self.tasks[task_idx].inc_rc();
        let sqe = io_uring::opcode::RecvMulti::new(io_uring::types::Fixed(fd), 0)
            .build()
            .user_data(udata);
        self.pending.push(sqe);
    }

    /// ACCEPT_MULTISHOT + ACCEPT_DIRECT : un seul SQE accepte toutes les connexions entrantes.
    /// Chaque CQE donne un fixed_file_index. MULTIPLE_MASK posé → task repollée à chaque CQE.
    fn send_accept_multishot(&mut self, listen_fd: i32) {
        let udata = self.current_udata(true); // MULTIPLE_MASK
        let task_idx = CURRENT_TASK.with(|c| c.get().unwrap().tag.task_idx);
        self.tasks[task_idx].set_multishot();
        self.tasks[task_idx].inc_rc();
        let sqe = io_uring::opcode::AcceptMulti::new(io_uring::types::Fd(listen_fd))
            .allocate_file_index(true)
            .build()
            .user_data(udata);
        self.pending.push(sqe);
    }

    /// WriteFixed pour `commit()` : soumet le write, réveille la task via la CQE.
    /// Pas de replenish read — le buf_ring est libéré en userspace après la CQE.
    fn send_commit_write(&mut self, ptr: *const u8, fd_idx: u32, len: u32, buf_id: u16) {
        let udata = self.current_udata(false);
        let task_idx = CURRENT_TASK.with(|c| c.get().unwrap().tag.task_idx);
        self.tasks[task_idx].inc_rc();
        let sqe =
            io_uring::opcode::WriteFixed::new(io_uring::types::Fixed(fd_idx), ptr, len, buf_id)
                .build()
                .user_data(udata);
        self.pending.push(sqe);
    }

    /// Rend le slot `buf_idx` au buf_ring (groupe 0) sans syscall.
    ///
    /// Écrit l'entrée à la position `tail & mask`, puis avance le tail.
    /// Un fence Release garantit que l'entrée est visible du kernel avant le tail.
    fn release_buf_to_ring(&self, buf_idx: u32) {
        let ring_entries = self.buffer.read_count as usize;
        // SAFETY: ring_entries doit être une puissance de 2 — garanti à la construction.
        let mask = ring_entries - 1;

        let tail_ptr = unsafe { self.buf_ring.add(14) as *mut u16 };
        let tail = unsafe { tail_ptr.read() } as usize;
        let slot = (tail & mask) * 16;

        let addr = self.buffer.read_buf_addr(buf_idx) as u64;
        unsafe {
            (self.buf_ring.add(slot) as *mut u64).write(addr);
            (self.buf_ring.add(slot + 8) as *mut u32).write(BufPool::<1>::BUF_SIZE as u32);
            (self.buf_ring.add(slot + 12) as *mut u16).write(buf_idx as u16);
            // Fence : l'entrée doit être visible avant que le kernel lise le nouveau tail.
            std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
            tail_ptr.write((tail + 1) as u16);
        }
    }

    /// CONNECT : connecte `fd` (fixed file) à `addr`.
    /// `addr` doit rester valide jusqu'à la CQE — la future doit la stocker inline.
    fn send_connect(&mut self, fd: u32, addr: *const libc::sockaddr, addrlen: libc::socklen_t) {
        let udata = self.current_udata(false);
        let task_idx = CURRENT_TASK.with(|c| c.get().unwrap().tag.task_idx);
        self.tasks[task_idx].inc_rc();
        let sqe = io_uring::opcode::Connect::new(io_uring::types::Fixed(fd), addr, addrlen)
            .build()
            .user_data(udata);
        self.pending.push(sqe);
    }

    /// SOCKET : crée un socket TCP (SOCK_STREAM) et l'installe dans un slot fixed-file auto.
    fn send_socket(&mut self) {
        let udata = self.current_udata(false);
        let task_idx = CURRENT_TASK.with(|c| c.get().unwrap().tag.task_idx);
        self.tasks[task_idx].inc_rc();
        let sqe = io_uring::opcode::Socket::new(
            libc::AF_INET,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
        .file_index(Some(io_uring::types::DestinationSlot::auto_target()))
        .build()
        .user_data(udata);
        self.pending.push(sqe);
    }
}

fn free_unsed_slab_slot() {
    GLOBAL_RUNTIME.with_borrow_mut(|rt| {
        RUNTIME_FREE_LIST.with_borrow_mut(|f_list| {
            for &idx in f_list.iter() {
                println!("Remove {:?}", idx);
                let _ = rt.tasks.remove_dropped_task(idx);
                println!("task size:{}", rt.tasks.count());
            }
            f_list.clear();
        })
    })
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // SAFETY: buf_ring alloué avec buf_ring_layout dans new(), libéré une seule fois.
        unsafe { dealloc(self.buf_ring, self.buf_ring_layout) };
    }
}

pub(crate) fn sqe_queue_push(entry: Entry) {
    GLOBAL_RUNTIME.with(|runtime| runtime.borrow_mut().sqe_queue_push(entry));
}

pub(crate) fn submit_read(fd: u32) {
    GLOBAL_RUNTIME.with(|rt| rt.borrow_mut().send_read(fd));
}
pub(crate) fn submit_accept(listen_fd: i32) {
    GLOBAL_RUNTIME.with(|rt| rt.borrow_mut().send_accept(listen_fd));
}
pub(crate) fn submit_write(fd: u32, buf_id: u16, len: u32) {
    GLOBAL_RUNTIME.with(|rt| rt.borrow_mut().send_write(fd, buf_id, len));
}
pub(crate) fn submit_recv_multishot(fd: u32) {
    GLOBAL_RUNTIME.with(|rt| rt.borrow_mut().send_recv_multishot(fd));
}
pub(crate) fn submit_accept_multishot(listen_fd: i32) {
    GLOBAL_RUNTIME.with(|rt| rt.borrow_mut().send_accept_multishot(listen_fd));
}
pub(crate) fn submit_commit_write(ptr: *const u8, fd_idx: u32, len: u32, buf_id: u16) {
    GLOBAL_RUNTIME.with(|rt| rt.borrow_mut().send_commit_write(ptr, fd_idx, len, buf_id));
}
pub(crate) fn release_read_buf(buf_idx: u32) {
    GLOBAL_RUNTIME.with_borrow(|rt| rt.release_buf_to_ring(buf_idx));
}
pub(crate) fn submit_connect(fd: u32, addr: *const libc::sockaddr, addrlen: libc::socklen_t) {
    GLOBAL_RUNTIME.with(|rt| rt.borrow_mut().send_connect(fd, addr, addrlen));
}
pub(crate) fn submit_socket() {
    GLOBAL_RUNTIME.with(|rt| rt.borrow_mut().send_socket());
}

pub(crate) fn current_result() -> i32 {
    CURRENT_TASK.with(|c| {
        c.get()
            .expect("current_result hors contexte runtime")
            .result
    })
}
pub(crate) fn current_cqe_flags() -> u32 {
    CURRENT_TASK.with(|c| {
        c.get()
            .expect("current_cqe_flags hors contexte runtime")
            .flags
    })
}

union UserData {
    user_data: u64,
    user_data_payload: UserDataPayload,
}
#[repr(C, align(8))]
#[derive(Clone, Copy)]

struct UserDataPayload {
    task_idx: SlabIdx,
    response_gen: u16,
    syscall_nb: u16,
}
#[derive(Clone, Copy)]
struct TaskInfo {
    pub(crate) tag: Tag,
    pub(crate) result: i32,
    pub(crate) flags: u32,
    pub(crate) response_gen: u16,
}

#[derive(Clone, Copy)]
struct Tag {
    task_idx: SlabIdx,
    syscall_nb: u16,
}
impl UserDataPayload {
    ///Safety: should be only use on valide user data
    unsafe fn from_user_data(user_data: u64) -> Self {
        let data = UserData { user_data };
        unsafe { data.user_data_payload }
    }
    fn into_user_data(self) -> u64 {
        let data = UserData {
            user_data_payload: self,
        };
        unsafe { data.user_data }
    }
    fn from_entry(entry: &io_uring::cqueue::Entry) -> Self {
        let user_data = entry.user_data();
        //Safety: As the user data comme from an entry, consider user_data as valid
        unsafe { Self::from_user_data(user_data) }
    }
}

impl TaskInfo {
    fn initial_poll(task_idx: SlabIdx) -> Self {
        Self {
            tag: Tag::new(task_idx),
            result: i32::MAX,
            flags: 0,
            response_gen: u16::MAX,
        }
    }
    fn from_entry(entry: &io_uring::cqueue::Entry) -> Self {
        let payload = UserDataPayload::from_entry(entry);
        let tag = Tag {
            syscall_nb: payload.syscall_nb,
            task_idx: payload.task_idx,
        };
        Self {
            tag,
            result: entry.result(),
            flags: entry.flags(),
            response_gen: payload.response_gen,
        }
    }
}

impl Tag {
    fn new(task_idx: SlabIdx) -> Self {
        Self {
            task_idx,
            syscall_nb: 0,
        }
    }
}
