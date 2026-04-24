# onlyio

Single-threaded async runtime built entirely on `io_uring`. Designed as the execution engine for [RadixOx](https://github.com/joshua-bouchat/radixox) — a Redis-compatible key-value store.

**Three hard constraints drive every design decision: zero syscall, zero copy, zero allocation on the hot path.**

---

## Design

### Event loop

One thread, one syscall per iteration (`io_uring_enter`). SQEs produced during task polling accumulate in a staging buffer and are flushed together — enabling N SQEs in a single `io_uring_enter` call (fan-out, pub/sub, batch writes).

```
1. WAKE_LIST  — full drain (userspace, 0 syscall)
2. YIELD_LIST — single snapshot pass (starvation guard)
3. check tasks.count() == 0 → break
4. submit_and_wait (io_uring_enter — the only syscall)
5. Process CQEs
```

### `user_data` packing (64 bits)

```
[ 32 bits: task_idx ][ 16 bits: syscall_nb ][ 16 bits: response_gen ]
```

Reserved sentinels (never produced by legitimate packing):

```rust
const TIMEOUT_UDATA: u64 = u64::MAX;
const CANCEL_UDATA:  u64 = u64::MAX - 1;
const WAKER_UDATA:   u64 = u64::MAX - 2;
```

Stale CQE detection is free: if `cqe.syscall_nb != task.syscall_nb`, the entry is silently ignored and `rc` is decremented. No extra state, no lock.

### Tasks

One allocation per task: header + future inline.

```
[ fn_poll ][ fn_drop ][ alloc_size: u32 ][ rc: u16 ][ syscall_nb: u16 ][ future... ]
```

`rc` starts at 1 (spawn token). Each submitted SQE increments it; each completed poll decrements it. Task is freed when `rc == 0`. Type-erased at creation via `poll_fn<F>` / `drop_fn<F>`.

### Connections — kernel-owned fds

`IORING_OP_ACCEPT_MULTISHOT` + `IORING_ACCEPT_DIRECT`: one SQE accepts all incoming connections. Each CQE yields a `fixed_file_index` — invisible to `/proc/pid/fd`, `lsof`, `ss`. Fixed-file table pre-registered as sparse via `IORING_REGISTER_FILES_SPARSE`.

### Buffers — split read/write pool

- **Read side**: pre-registered via `IORING_REGISTER_BUFFERS` + `register_buf_ring` (group 0). `IOSQE_BUFFER_SELECT` lets the kernel pick a free slot. Returns a `RwBuffer` on the CQE.
- **Write side**: `IORING_OP_WRITE_FIXED` + `buf_id`. Zero-copy: no memcpy between read and write.
- Fan-out: clone `RwBuffer` O(1) × N subscribers, submit N SQEs in one `io_uring_enter`. Zero allocation, zero copy.

### Waker / inter-task communication

`Waker` is 8 bytes, stack-allocated. Built from `CURRENT_TASK` at poll time. `wake(self)` pushes into `WAKE_LIST` and calls `mem::forget` — rc ownership transfers to the runtime. `Drop` (waker discarded without wake) decrements rc.

`WAKE_LIST` is drained fully each turn: a chain `A wakes B → B submits SQE` resolves in one pass with minimal latency.

`YIELD_LIST` uses a snapshot drain for starvation safety: re-yields land in the next turn, not the current one.

---

## Kernel requirements

| Feature | Minimum |
|---|---|
| io_uring base | 5.1 |
| `BUFFER_SELECT` | 5.7 |
| `ACCEPT_DIRECT` + `MULTISHOT` | 5.19 |
| Target machine | 6.19 ✓ |

---

## Usage

```rust
use onlyio::{block_on, accept, read, spawn};

fn main() {
    let listener = std::net::TcpListener::bind("0.0.0.0:6379").unwrap();
    listener.set_nonblocking(true).unwrap();
    let fd = listener.as_raw_fd();

    block_on(async move {
        loop {
            let conn_fd = accept(fd).await.expect("accept failed");
            spawn(handle(conn_fd));
        }
    }).unwrap();
}

async fn handle(conn_fd: u32) {
    let mut buf = read(conn_fd).await.expect("read failed");
    let len = buf.bytes;
    buf.commit(len).await; // zero-copy echo
}
```

---

## API surface

| Function | Description |
|---|---|
| `block_on(future)` | Runs the runtime until the root future completes |
| `spawn(future)` | Spawns a task onto the current runtime thread |
| `accept(fd)` | Single-shot accept → `fixed_file_index` |
| `read(fd)` | Single recv → `RwBuffer` |
| `alloc_write_buffer()` | Allocate a fixed write buffer |

`RwBuffer::commit(len)` writes `len` bytes and releases the read slot back to the buf_ring.

---

## Build

```bash
cargo build
cargo build --release
cargo test
cargo clippy -- -D warnings
```

Requires Linux 5.19+ at runtime. Tested on 6.19.

---

## Architecture rules

- **No allocation on the hot path** — no `Vec::push` / `Box::new` outside init.
- **No `Arc` / `Mutex`** — single-thread only; `Rc<RefCell<>>` or direct access.
- **No gratuitous `unsafe`** — every `unsafe` block carries a `// SAFETY:` comment.
- `rc` is decremented *after* the poll, never before.
- Stale CQEs (`syscall_nb` mismatch) are silently ignored — no poll, but `rc -= 1`.

---

## Roadmap

### Standard `Waker` interface

The current wake mechanism is a hand-rolled `WAKE_LIST` — efficient but opaque. The goal is to expose a proper `std::task::Waker` so that onlyio futures can compose with any external code that drives a standard `Future`. This requires implementing a vtable-backed `RawWaker` that pushes into the runtime's wake list, without breaking the existing zero-alloc invariant (the waker itself stays stack-allocated).

### `futures` crate compatibility — `AsyncRead` / `AsyncWrite` / combinators

Implementing `AsyncRead` and `AsyncWrite` on connection handles would unlock the full `futures` ecosystem: `join!`, `select!`, `StreamExt`, `SinkExt`, timeouts, and third-party protocol parsers that expect standard traits. This depends on the Waker work above.

### Variable-size write buffers for fan-out > 4096 bytes

Today all write buffers are fixed at 4096 bytes. Fan-out of large payloads (e.g. pub/sub with a big value) either requires splitting across multiple SQEs or copying into multiple slots. The plan is to support multi-slot contiguous allocations from the write pool — allocate N consecutive pages, register as a single `iovec`, submit one `WRITE_FIXED` per subscriber pointing at the same physical region with zero copy.

---

## Dependencies

- [`io-uring`](https://crates.io/crates/io-uring) — safe bindings to the Linux io_uring API
- [`libc`](https://crates.io/crates/libc) — raw socket / syscall types
- `hislab` *(local crate, `../../hislab`)* — O(1) slab allocator backed by `mmap` + hierarchical bitmap
