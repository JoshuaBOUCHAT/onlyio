use std::{
    alloc::{Layout, alloc_zeroed, dealloc},
    cell::{Cell, RefCell},
    num::NonZero,
    u16, u32,
};

const PAGE_SIZE: usize = 4096;

use io_uring::{IoUring, SubmissionQueue, squeue::Entry};

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
    fn handle_completion(&mut self) {}
    /// Insère la future dans le slab et retourne son index — sans la poller.
    fn register_future(&mut self, future: impl Future<Output = ()>) -> SlabIdx {
        let task = Task::new(future);
        self.tasks.insert(task)
    }

    pub(crate) fn remove_task(&mut self, task_idx: SlabIdx) {
        self.tasks.remove(task_idx);
    }
    pub(crate) fn block_on(future: impl Future<Output = ()>) -> IOResult<()> {
        // Insertion dans le slab — borrow libéré immédiatement après.
        let task_idx = GLOBAL_RUNTIME.with(|rt| rt.borrow_mut().register_future(future));

        // Premier poll hors borrow : la future peut appeler submit_xxx librement.
        {
            let mut task = GLOBAL_RUNTIME.with_borrow(|rt| rt.tasks[task_idx].clone());
            CURRENT_TASK.set(Some(TaskInfo::initial_poll(task_idx)));
            let _ = task.poll();
            task.release();
        }
        free_unsed_slab_slot();

        loop {
            // poll ready tasks
            let maybe_submit = GLOBAL_RUNTIME.with_borrow_mut(|rt| {
                if !rt.pending.is_empty() {
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

            for entry in entries {
                let task_info = TaskInfo::from_entry(&entry);
                let nb_syscall = task_info.tag.syscall_nb;
                let task_idx = task_info.tag.task_idx;
                let mut task = GLOBAL_RUNTIME.with_borrow_mut(|rt| rt.tasks[task_idx].clone()); //Cheap clone

                match task.evaluate_syscall_nb(nb_syscall) {
                    SyscallNbCompResult::Error => {
                        panic!("Recieve a SQE newer than expected")
                    }
                    SyscallNbCompResult::Expired => {}
                    SyscallNbCompResult::Normal => {
                        let mut task_info = task_info;
                        task_info.tag.syscall_nb += 1; // We increment the tag has it may be use to produce next SQE
                        task.augmente_syscall_nb(); //This allow to make all new incoming CQE with previous syscall_nb expired
                        CURRENT_TASK.set(Some(task_info));
                        let _ = task.poll();
                    }
                    SyscallNbCompResult::Multiple => {
                        //As it is multiple we poll every time, the futur have to handle the logic
                        CURRENT_TASK.set(Some(task_info));
                        let _ = task.poll();
                    }
                }

                task.release();
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

    /// ACCEPT_MULTISHOT + ACCEPT_DIRECT : un seul SQE pour toutes les connexions entrantes.
    /// Chaque CQE donne un fixed_file_index. `listen_fd` est le fd du socket en écoute (non-fixed).
    fn send_accept(&mut self, listen_fd: i32) {
        let udata = self.current_udata(true); // multishot → MULTIPLE_MASK
        // Synchronise le syscall_nb stocké dans la task avec le MULTIPLE_MASK du SQE,
        // sinon evaluate_syscall_nb voit un mismatch et retourne Error.
        let task_idx = CURRENT_TASK.with(|c| c.get().unwrap().tag.task_idx);
        self.tasks[task_idx].set_multishot();
        self.tasks[task_idx].inc_rc();
        let sqe = io_uring::opcode::Accept::new(
            io_uring::types::Fd(listen_fd),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
        .file_index(Some(io_uring::types::DestinationSlot::auto_target())) // ACCEPT_DIRECT, slot auto
        .build()
        .flags(io_uring::squeue::Flags::ASYNC)
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
                let _ = rt.tasks.remove(idx);
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
