# Memory Index

- [No atomics in single-threaded runtime](feedback_no_atomics.md) — Never use AtomicX, everything is single-threaded, plain types only
- [Runtime abstracted from futures](feedback_runtime_abstraction.md) — Futures use free functions (submit_read, current_result), never io_uring types directly
