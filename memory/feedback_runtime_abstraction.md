---
name: Runtime abstracted from futures
description: Futures must not access io_uring types directly — Runtime exposes typed methods (send_read, send_write) and free helper functions
type: feedback
---

Futures must never import or use io_uring opcodes/types directly. The Runtime exposes typed methods (`send_read`, `send_write`, etc.) that build SQEs internally. Futures call free functions like `submit_read(fd)` and `current_result()`.

**Why:** Clean separation between the runtime layer and the async API layer. Futures are easier to write and test, and io_uring details stay contained in runtime.rs.

**How to apply:** When writing a future or async fn that does I/O:
- Call `submit_read(fd)`, `submit_write(fd, buf_id, len)` etc. (free functions in runtime.rs)
- Read CQE data via `current_result()`, `current_cqe_flags()`
- Never import `io_uring::opcode`, `io_uring::types`, `io_uring::squeue` in future code
