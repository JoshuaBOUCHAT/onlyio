---
name: No atomics in single-threaded runtime
description: Never use AtomicX types in onlyio — everything is single-threaded, plain types only
type: feedback
---

Never use `AtomicU16`, `AtomicU32`, etc. in this codebase. Everything is single-threaded by design. Use plain `u16`, `u32`, raw pointer writes, etc.

**Why:** The entire runtime is monothread — no shared state across threads, no need for atomic operations. Adding atomics is wrong and shows a misunderstanding of the architecture.

**How to apply:** Any time you need to read/write a counter or flag, use plain types and raw pointer writes (with `write_volatile` if needed for ring tail). Never reach for `std::sync::atomic`.
