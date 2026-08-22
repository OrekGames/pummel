## 2026-08-19 - Replace Arc with Rc in thread_local caches
**Learning:** `thread_local!` data is inherently isolated per thread. Using `Arc` for shared reference counting inside these caches adds unnecessary performance overhead from atomic operations.
**Action:** Always prefer `std::rc::Rc` over `std::sync::Arc` for data stored in `thread_local!` to avoid atomic reference counting overhead.
