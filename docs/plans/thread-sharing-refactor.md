# Plan: Rework Thread Sharing After Session List Refactor

## Problem Statement

The `session-list-connection-refactor` removed `HistoryStore` from `AcpThreadView` and `AgentPanel`, but thread sharing functionality was added during/after this refactor and still references the removed `history_store` field.

### Current Compilation Errors

```
error[E0609]: no field `history_store` on type `&mut AcpThreadView`
    --> crates/agent_ui/src/acp/thread_view.rs:1001:34
     |
1001 |         let history_store = self.history_store.clone();
     |                                  ^^^^^^^^^^^^^ unknown field
```

### Affected Locations

1. **`sync_thread` in `thread_view.rs`** (L991-1050): Uses `self.history_store.save_thread()` to save synced thread data
2. **`handle_open_request` in `main.rs`** (L850-905): Uses `AgentPanel::thread_store()` to save imported threads

## Thread Sharing Flow

### Sharing a Thread

1. User clicks share button on a thread
2. `share_thread()` serializes `DbThread` → `SharedThread`
3. Sends to collab server via `ShareAgentThread` proto message
4. Server stores and returns shareable URL

### Importing a Shared Thread (via URL)

1. User opens `zed://shared-agent-thread/{session_id}` URL
2. `handle_open_request` fetches thread via `GetSharedAgentThread`
3. Deserializes to `DbThread` with `imported: true`
4. **Currently broken**: Tries to save via `history_store.save_thread()`
5. Opens thread in agent panel via `panel.open_thread()`

### Syncing an Imported Thread

1. User clicks sync button on an imported thread
2. `sync_thread()` fetches latest via `GetSharedAgentThread`
3. **Currently broken**: Tries to save via `self.history_store.save_thread()`
4. Resets thread view to reload

## Solution: Use `ThreadsDatabase` Directly

Since thread sharing only works with the native agent (which uses `ThreadsDatabase`), we can use `ThreadsDatabase::connect(cx)` directly instead of going through `HistoryStore`.

### Why This Works

- `ThreadsDatabase` is a global singleton accessible via `ThreadsDatabase::connect(cx)`
- The native agent already uses `ThreadsDatabase` for all thread storage
- `AgentSessionList` (used for session history) queries `ThreadsDatabase` directly
- When `AcpThreadView` emits `ThreadReady`, `AgentPanel` refreshes the session list from the database

### Changes Required

#### 1. Fix `sync_thread` in `thread_view.rs`

**Before:**

```rust
fn sync_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    // ...
    let history_store = self.history_store.clone();
    // ...
    history_store
        .update(&mut cx.clone(), |store, cx| {
            store.save_thread(session_id.clone(), db_thread, cx)
        })
        .await?;
    // ...
}
```

**After:**

```rust
fn sync_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    // ...
    let database_future = agent::ThreadsDatabase::connect(cx);
    // ...
    let database = database_future.await.map_err(|err| anyhow::anyhow!(err))?;
    database.save_thread(session_id.clone(), db_thread).await?;
    // ...
}
```

#### 2. Fix `handle_open_request` in `main.rs`

**Before:**

```rust
let (client, history_store) = workspace
    .update(cx, |workspace, cx| {
        let history_store: Option<gpui::Entity<HistoryStore>> = workspace
            .panel::<AgentPanel>(cx)
            .map(|panel| panel.read(cx).thread_store().clone());
        (client, history_store)
    })?;

let Some(history_store) = history_store else {
    anyhow::bail!("Agent panel not available");
};

// ...

history_store
    .update(&mut cx.clone(), |store, cx| {
        store.save_thread(session_id.clone(), db_thread, cx)
    })
    .await?;
```

**After:**

```rust
let client = workspace.update(cx, |workspace, cx| {
    workspace.project().read(cx).client()
})?;

// ...

let database = cx.update(|cx| agent::ThreadsDatabase::connect(cx))?.await
    .map_err(|err| anyhow::anyhow!(err))?;
database.save_thread(session_id.clone(), db_thread).await?;
```

#### 3. Ensure Session List Refresh

The session list refresh should happen automatically because:

1. For **sync**: After `sync_thread` saves and calls `reset()`, the thread reloads and emits `ThreadReady`
2. For **import**: After `handle_open_request` calls `panel.open_thread()`, a new `AcpThreadView` is created

However, we should verify the session list refreshes after import. If not, we may need to explicitly call `refresh_agent_session_snapshot()` on the panel.

#### 4. Update Imports

In `thread_view.rs`:

```rust
use agent::ThreadsDatabase;
```

In `main.rs`:

```rust
use agent::ThreadsDatabase;
```

## Visibility Issue: `ThreadsDatabase` is `pub(crate)`

`ThreadsDatabase` is currently marked as `pub(crate)` in `agent/src/db.rs`, meaning it's not accessible from `agent_ui` or `zed` crates.

### Options

**Option A: Make `ThreadsDatabase` public** (Recommended)

- Change `pub(crate) struct ThreadsDatabase` to `pub struct ThreadsDatabase`
- Simplest change, exposes the existing API
- Other crates can then call `agent::ThreadsDatabase::connect(cx)`

**Option B: Add a helper function**

- Keep `ThreadsDatabase` as `pub(crate)`
- Add a public helper function like `pub fn save_imported_thread(id, thread, cx) -> Task<Result<()>>`
- More encapsulated but adds indirection

**Option C: Keep using HistoryStore for thread sharing only**

- `HistoryStore` still exists in the `agent` crate
- Could pass it specifically for thread sharing operations
- Adds complexity and goes against the refactor goals

### Recommendation

**Option A** is the cleanest approach. `ThreadsDatabase` is a simple database wrapper with no complex invariants. Making it public allows direct database access for operations that don't need the full `HistoryStore` machinery (which includes text thread tracking, recently opened tracking, etc.).

## Implementation Steps

- [x] **Step 0**: Make `ThreadsDatabase` public in `agent/src/db.rs`
- [x] **Step 1**: Update `sync_thread` in `thread_view.rs` to use `ThreadsDatabase` directly
- [x] **Step 2**: Update `handle_open_request` in `main.rs` to use `ThreadsDatabase` directly
- [x] **Step 3**: Remove unused `HistoryStore` imports from both files
- [x] **Step 4**: Run `cargo check -p agent_ui` and `cargo check -p zed` to verify compilation
- [ ] **Step 5**: Test thread sharing flow:
  - Share a thread and get URL
  - Import the shared thread via URL
  - Verify imported thread appears in session list
  - Sync an imported thread
  - Verify synced changes are reflected
- [ ] **Step 6**: Run `./script/clippy` to check for warnings (note: unrelated errors in livekit_client)

## Testing

### Manual Test Cases

1. **Share Thread**
   - Create a new thread with some messages
   - Click share button
   - Verify URL is copied and toast appears

2. **Import Thread**
   - Open a `zed://shared-agent-thread/{id}` URL
   - Verify thread loads with "🔗" prefix in title
   - Verify thread appears in session history

3. **Sync Thread**
   - Have original sharer update the thread
   - Click sync button on imported thread
   - Verify "Thread synced" toast appears
   - Verify changes are reflected

### Automated Tests

The existing tests in `collab/src/tests/agent_sharing_tests.rs` should continue to pass:

- `test_share_and_retrieve_thread`
- `test_reshare_updates_existing_thread`
- `test_get_nonexistent_thread`
- `test_sync_imported_thread`

## Notes

- The `imported: true` flag on `DbThread` is set by `SharedThread::to_db_thread()`, which is already correct
- Session list refresh is triggered by the `ThreadReady` event subscription in `AgentPanel::set_active_view()`
- `HistoryStore::save_thread` previously called `reload()` after saving to refresh the history list; in the new approach, session list refresh happens via `ThreadReady` event
- The `main.rs` import handler may need to explicitly trigger a session list refresh after saving, since no `ThreadReady` event fires until the thread view loads

## Success Criteria

- [x] `cargo check -p agent_ui` passes
- [x] `cargo check -p zed` passes
- [ ] Thread sharing, importing, and syncing work correctly
- [ ] Session list shows imported threads
- [x] No clippy warnings in changed files (unrelated warnings exist in other crates)
