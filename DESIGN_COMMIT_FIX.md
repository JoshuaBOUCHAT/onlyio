# Bug : le runtime « digère » ses propres writes

## Diagnostic

### Root cause

`send_commit_write` utilise `current_udata(false)` qui retourne le `syscall_nb`
du recv_multishot *avec* le `MULTIPLE_MASK` (`0x8000`). Résultat :

1. recv_multishot SQE soumis → `user_data = { task_idx:0, syscall_nb:0x8000 }`
2. CQE recv arrive → `Multiple` case → `CURRENT_TASK = { result=4, flags=buf_id|F_MORE }` → task pollée
3. `ReadStream::Running` → livre `Some(buf)` → tâche continue
4. `CommitFuture::Init` → `submit_commit_write` → **même `user_data` que le recv** (`syscall_nb=0x8000`)
5. CQE write arrive → `Multiple` case → **CURRENT_TASK = { result=38, flags=0 }** → task pollée à nouveau
6. `CommitFuture::Pending` → résout → `ReadStream::Running` (toujours `Running`) → lit `result=38, flags=0` → `buf_id=0`, `bytes=38` → livre le contenu du write comme s'il était une réception

C'est pour ça que le client reçoit deux réponses et que le test imprime
`"Recieve: Total number of char recieve is: 4"`.

### Problème secondaire : ReadStream::Running ne suspendait jamais

Même *sans* le bug CQE, après que `CommitFuture` se résout (via le wake-list
ou autrement), le future chaîne vers `read_stream.next()`.
`ReadStream::Running` est appelée dans le même contexte de poll → lit
`current_result()` qui contient encore le contexte de la CQE précédente →
livre un buffer fantôme en boucle infinie.

---

## Solution choisie (approche "write task séparée")

### Principe

`commit()` spawne une **nouvelle tâche** dans le runtime pour le write. Cette
tâche a son propre `task_idx`, donc :
- la CQE du write est dispatchée vers la write task, pas vers la read task
- aucune interférence entre les deux flux

La write task, une fois terminée, pousse `(parent_idx, write_result)` dans un
**`RUNTIME_WAKE_LIST`**. Le runtime le draine en fin de chaque itération et
re-polle la tâche parent avec le résultat.

### Nouveau state de ReadStream : `NeedSuspend`

```
Init         → soumet recv_multishot → Running → Pending
Running      → CQE reçue → NeedSuspend → Ready(Some(buf))
NeedSuspend  → Running → Pending          ← NOUVEAU : suspend avant le prochain CQE
Done         → Ready(None)
```

`NeedSuspend` est indispensable même avec la write task séparée : quand la
wake list re-polle le parent (avec `CURRENT_TASK.result = write_result`),
`CommitFuture::Pending` se résout, puis `ReadStream` est appelée dans le même
poll. Sans `NeedSuspend` elle lirait `write_result` comme si c'était un recv.

### Flux complet corrigé

```
1. recv CQE (F_MORE, result=4)
   → Multiple case → CURRENT_TASK = {result=4, flags=buf_id|F_MORE}
   → parent task pollée
   → ReadStream::Running → NeedSuspend → Ready(Some(buf))
   → user : commit(34)
   → CommitFuture::Init :
       parent_idx = CURRENT_TASK.task_idx
       spawn WriteFuture { ptr, fd_idx, len, buf_id, parent_idx }
         ↳ WriteFuture::Init : submit_commit_write (avec task_idx=WRITE_TASK)
         ↳ returns Pending
   → CommitFuture::Pending attente
   → parent task returns Pending

2. write CQE
   → lookup task_idx = WRITE_TASK (≠ read task)
   → Normal case → WriteFuture::Pending
   → wake_task(parent_idx, write_result)  → RUNTIME_WAKE_LIST
   → write task rc→0 → freed

3. fin de l'itération CQE → drain RUNTIME_WAKE_LIST
   → parent task pollée avec CURRENT_TASK = {result=write_result}
   → CommitFuture::Pending → release_read_buf → Ready(write_result)
   → ReadStream::NeedSuspend → Running → Pending
   → parent task returns Pending

4. prochain recv CQE
   → ReadStream::Running lit le vrai résultat ✓
```

---

## Changements nécessaires

### `runtime.rs`
- `RUNTIME_WAKE_LIST: RefCell<Vec<(SlabIdx, i32)>>`
- `pub(crate) fn spawn_task(future)` — insert + initial poll + save/restore CURRENT_TASK
- `pub(crate) fn wake_task(task_idx, result)` — push dans RUNTIME_WAKE_LIST
- `TaskInfo::wake(task_idx, result)` — constructeur
- Dans la boucle `block_on` : drainer la wake list après `free_unsed_slab_slot()`
- **Fix loop** : quand `pending` est vide, appeler `ring.submit_and_wait(1)` quand
  même pour bloquer jusqu'au prochain CQE (sans ça le test panique sur
  "Entries len == 0" dès que le write a été soumis et la queue CQ est vide)

### `calls/commit.rs`
```rust
// WriteFuture : submit write + wake parent
struct WriteFuture { parent_idx, ptr, fd_idx, len, buf_id, submitted }

// CommitFuture (modifié)
Init  → spawn WriteFuture, save buf_idx → Pending { buf_idx }
Pending { buf_idx } → release_read_buf(buf_idx) + current_result() → Ready
```

### `calls/read.rs`
- Ajouter `NeedSuspend` à `ReadStreamState`
- `result <= 0` traité comme `Done` (EOF + erreur)

### `lib.rs` (tests)
- `stream_test` : utiliser `accept` (single-shot) au lieu de `accept_stream`
  → permet la terminaison propre quand la connexion ferme
  (avec multishot accept, l'accept SQE reste en vol indéfiniment → rc ne
  tombe jamais à 0 → la boucle ne sort pas)

---

## Questions ouvertes / points de design à discuter

1. **rc et terminaison** : avec accept single-shot + recv_multishot, quand la
   connexion ferme (result≤0, F_MORE=false) le rc tombe à 0 proprement.
   Avec accept_multishot en boucle, il faudra un mécanisme de cancellation
   IORING_OP_ASYNC_CANCEL pour les SQEs orphelins.

2. **wake list vs waker Rust** : actuellement aucun waker ; la wake list est un
   mécanisme ad-hoc. À terme il faudra peut-être aligner avec les vraies
   `Waker` de Rust si on veut composer avec d'autres futures.

3. **multiple writes concurrents** : si une task fait N commits avant que le
   premier ne revienne, N write tasks sont en vol. Le parent peut être woken N
   fois. CommitFuture::Pending re-lit `current_result()` à chaque wake. À
   vérifier que le bon résultat correspond au bon commit.
   → Pour l'instant le design est sequential (1 commit à la fois par task),
   ce qui est suffisant pour RadixOx.

4. **submit_and_wait quand pending vide** : la boucle actuelle panique si
   entries == 0. Il faut appeler `ring.submit_and_wait(1)` même sans SQEs à
   soumettre pour bloquer jusqu'au prochain CQE. Deux options :
   a) retirer le `if pending.is_empty() { panic }` de `submit_and_wait` et
      toujours l'appeler
   b) ajouter une méthode `wait_cqe` séparée
