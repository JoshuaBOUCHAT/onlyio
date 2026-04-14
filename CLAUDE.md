# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Mission

`onlyio` est le runtime async monothread io_uring de RadixOx — remplacement direct de monoio, taillé pour zéro syscall / zéro copy / zéro allocation sur le chemin chaud. Toute décision de design doit être jugée à l'aune de ces trois contraintes.

## Commandes

```bash
cargo build
cargo build --release
cargo test
cargo test <nom_du_test>          # test unique
cargo clippy -- -D warnings
```

## Architecture cible (voir ~/rust/radixox/RUNTIME_ARCH.md)

### Event loop

Un seul thread, un seul syscall par itération (`io_uring_enter`). Les SQEs produites pendant le poll des tasks s'accumulent dans le SQ et partent ensemble au prochain `io_uring_enter`.

`IORING_SETUP_SQPOLL` est **optionnel** : quand il est actif, le kernel thread consomme les SQEs sans attendre `io_uring_enter` ; sans lui, `io_uring_enter` soumet et attend en même temps.

**Batching SQE** : feature obligatoire et puissante — il doit être possible de soumettre N SQEs en un seul `io_uring_enter` (cas typique : fan-out pub/sub, N writes vers N subscribers). Le design du runtime doit préserver cette capacité sans la brider.

### user_data packée sur 64 bits

```
[ 32 bits : op_idx ][ 16 bits : generation ][ 16 bits : task_idx ]
```

Deux sentinelles réservées en haut de l'espace :

```rust
const TIMEOUT_UDATA: u64 = u64::MAX;
const CANCEL_UDATA:  u64 = u64::MAX - 1;
const WAKER_UDATA:   u64 = u64::MAX - 2;
```

### Task

Une seule alloc : header + future inline. Type erasure à la création via deux `fn` bruts (`poll_fn<F>`, `drop_fn<F>`). `rc: u16` compte les SQEs en vol — le slot n'est libéré que quand `rc == 0`. Stocké dans un `HiSlab<Option<NonNull<Task>>>` (niche `NonNull` → pas de discriminant).

### Op slots

`OpSlot { result: Option<i32>, task_idx: u16, generation: u16 }`. La `generation` est incrémentée à chaque réutilisation du slot : toute CQE avec une génération obsolète est ignorée sans flag zombie.

### Connexions — kernel-owned fds

`IORING_OP_ACCEPT` avec `IORING_ACCEPT_MULTISHOT | IORING_ACCEPT_DIRECT` : un seul SQE pour toutes les connexions entrantes, résultat = `fixed_file_index` (invisible dans `/proc/pid/fd`, `lsof`, `ss`). Table initialisée en sparse via `IORING_REGISTER_FILES_SPARSE`.

### Buffers — `SlabBuffer` / `BufGuard`

`hislab::buffer_slab::BufferSlab<4096>` pré-enregistrée via `IORING_REGISTER_BUFFERS` à l'init (syscall bloquant, one-shot). Sur les reads, `IOSQE_BUFFER_SELECT` laisse le kernel choisir le bloc → `BufGuard(buf_id)` créé sur la CQE, dropped après parsing. Pour les writes, `IORING_OP_WRITE_FIXED` + `buf_id`. Fan-out pub/sub : `BufGuard::clone()` O(1) × N subscribers, N SQEs soumis en batch (un seul `io_uring_enter`), 0 copie mémoire.

### Thread-local, pas d'`Arc`

```rust
thread_local! {
    static RT: RefCell<Runtime> = RefCell::new(Runtime::new());
}
```

`Rc<RefCell<>>` partout. `spawn` panique hors contexte runtime.

## Dépendance hislab

`hislab` (crate personnelle, `~/rust/hislab`) fournit :
- `HiSlab<T>` — slab allocator O(1) insert/remove via bitmap hiérarchique 4 niveaux, `mmap` anonyme, pages pré-faultées via `madvise(MADV_POPULATE_WRITE | MADV_HUGEPAGE)`, `!Send + !Sync`, interior mutability via `UnsafeCell`.
- `buffer_slab::BufferSlab<const N: usize>` — pool de buffers taille fixe, même design, `AutoGuard<'a, N>` RAII.

Pour référencer la version locale pendant le développement :
```toml
hislab = { path = "../../hislab" }
```

## Versions kernel requises

| Feature | Minimum |
|---|---|
| io_uring base | 5.1 |
| `BUFFER_SELECT` | 5.7 |
| `ACCEPT_DIRECT` + `MULTISHOT` | 5.19 |
| Machine cible | 6.19 ✓ |

## Règles de rigueur

- **Aucune allocation sur le chemin chaud** : pas de `Vec::push` / `Box::new` hors init.
- **Aucun `Arc` / `Mutex`** : tout est single-thread ; utiliser `Rc<RefCell<>>` ou accès direct.
- **Aucun `unsafe` gratuit** : chaque bloc `unsafe` doit avoir un commentaire de justification `// SAFETY:`.
- Les CQEs avec génération obsolète sont silencieusement ignorées — ne pas `panic!` sur un CQE inattendu.
- `IORING_OP_ASYNC_CANCEL` émet un SQE supplémentaire ; après cancel, incrémenter la génération du slot immédiatement (avant la CQE d'annulation).
