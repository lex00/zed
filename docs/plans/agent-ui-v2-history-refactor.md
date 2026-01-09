# Plan: Refactor `agent_ui_v2` to Use `AgentConnection::session_list()`

## Problem Statement

The `agent_ui_v2` crate still depends on `HistoryStore` for thread history, while `agent_ui` (v1) has been refactored to use `AgentConnection::session_list()`. This inconsistency:

1. Prevents removal of `HistoryStore` from the `agent` crate
2. Means v2 doesn't benefit from the connection-backed session list architecture
3. Creates maintenance burden with two different history patterns

## Goal

Refactor `agent_ui_v2` to match `agent_ui`'s architecture:
- Agent session history comes from `AgentConnection::session_list()`
- Text thread history comes from `TextThreadStore` directly
- Remove `HistoryStore` dependency entirely

## Current Architecture (v2)

```
AgentsPanel
├── history_store: Entity<HistoryStore>  ← combines agent + text threads
├── history: Entity<AcpThreadHistory>
│   └── history_store: Entity<HistoryStore>
│       └── entries() → Iterator<HistoryEntry>  ← mixed agent/text
```

### Files Using `HistoryStore`

1. **`agent_ui_v2/src/agents_panel.rs`**
   - Creates `HistoryStore` in `new()`
   - Uses `history_store.read(cx).entries()` in `restore_utility_pane()`
   - Passes `history_store` to `AcpThreadHistory::new()`

2. **`agent_ui_v2/src/thread_history.rs`**
   - `AcpThreadHistory` holds `Entity<HistoryStore>`
   - `update_visible_items()` calls `history_store.update(cx, |store, _| store.entries())`
   - `remove_thread()` calls `history_store.update(cx, |this, cx| this.delete_thread(...))`
   - `remove_history()` calls `history_store.update(cx, |store, cx| store.delete_threads(cx))`
   - `render()` checks `history_store.read(cx).is_empty(cx)`

## Target Architecture (matching v1)

```
AgentsPanel
├── agent_session_list: Option<Rc<dyn AgentSessionList>>  ← from connection
├── agent_sessions: AgentSessions  ← shared snapshot
├── text_thread_store: Entity<TextThreadStore>  ← for text threads
├── acp_history: Entity<AcpThreadHistory>  ← agent sessions only
│   └── session_list: Rc<dyn AgentSessionList>
├── text_history: Entity<TextThreadHistory>  ← text threads only
│   └── text_thread_store: WeakEntity<TextThreadStore>
```

## Key Differences Between v1 and v2

| Aspect | v1 (agent_ui) | v2 (agent_ui_v2) |
|--------|---------------|------------------|
| History source | `AgentConnection::session_list()` | `HistoryStore` |
| History views | Separate: `AcpThreadHistory` + `TextThreadHistory` | Combined: single `AcpThreadHistory` |
| Session refresh | Via `ThreadReady` event subscription | Via `HistoryStore` observation |
| Delete support | `AgentSessionList::delete_session()` | `HistoryStore::delete_thread()` |
| Entry type | `AgentSessionInfo` | `HistoryEntry` (enum) |

## Implementation Plan

### Phase 1: Create Separate History Views

#### Step 1.1: Update `AcpThreadHistory` in v2

Copy/adapt `agent_ui/src/acp/thread_history.rs` patterns:

- Change from `Entity<HistoryStore>` to `Rc<dyn AgentSessionList>`
- Change `HistoryEntry` to `AgentSessionInfo`
- Update `update_visible_items()` to call `session_list.list()`
- Update `remove_thread()` to call `session_list.delete_session()`
- Update `remove_history()` to call `session_list.delete_all_sessions()`
- Add `set_session_list()` and `refresh()` methods
- Update event to `OpenSession(SessionId)` instead of `Open(HistoryEntry)`

#### Step 1.2: Create `TextThreadHistory` in v2 (if needed)

If v2 needs text thread history:
- Copy/adapt `agent_ui/src/acp/text_thread_history.rs`
- Takes `WeakEntity<TextThreadStore>`
- Uses `SavedTextThreadMetadata` entries

### Phase 2: Update `AgentsPanel`

#### Step 2.1: Replace `HistoryStore` with Direct Sources

```rust
pub struct AgentsPanel {
    // Remove:
    // history_store: Entity<HistoryStore>,

    // Add:
    agent_session_list: Option<Rc<dyn AgentSessionList>>,
    agent_sessions: AgentSessions,  // shared snapshot for completions
    text_thread_store: Entity<TextThreadStore>,

    // Keep but change type signature:
    history: Entity<AcpThreadHistory>,  // now takes session_list
}
```

#### Step 2.2: Wire Up Session List from Connection

When a thread is opened/created:
1. Get `AgentConnection` from the thread
2. Call `connection.session_list(cx)` to get provider
3. Pass to `AcpThreadHistory::set_session_list()`
4. Subscribe to `ThreadReady` event to refresh

#### Step 2.3: Update `restore_utility_pane()`

Change from:
```rust
let entry = self.history_store.read(cx).entries().find(|e| ...);
```

To:
```rust
// For agent threads, load directly from database or session list
// For text threads, query TextThreadStore
```

### Phase 3: Remove `HistoryStore` Dependency

#### Step 3.1: Remove from `agent_ui_v2`

- Remove `use agent::HistoryStore` imports
- Remove `history_store` field from `AgentsPanel`
- Update all methods that referenced `history_store`

#### Step 3.2: Check for Other Consumers

After v2 is updated, check if anything else uses `HistoryStore`:
```bash
grep -r "HistoryStore" crates/ --include="*.rs"
```

If nothing else uses it, `HistoryStore` can be removed from `agent` crate.

### Phase 4: Cleanup

- [ ] Remove `HistoryStore` from `agent` crate (if unused)
- [ ] Remove `HistoryEntry`, `HistoryEntryId` types (if unused)
- [ ] Update any remaining references

## File Changes Summary

### `agent_ui_v2/src/thread_history.rs`

- Change `history_store: Entity<HistoryStore>` → `session_list: Rc<dyn AgentSessionList>`
- Change `HistoryEntry` → `AgentSessionInfo`
- Update all methods to use `AgentSessionList` API
- Add `set_session_list()` method

### `agent_ui_v2/src/agents_panel.rs`

- Remove `history_store` field
- Add `agent_session_list`, `agent_sessions`, keep `text_thread_store`
- Update `new()` to not create `HistoryStore`
- Update `restore_utility_pane()` to query appropriate source
- Add session list refresh logic (subscribe to `ThreadReady`)

### `agent/src/history_store.rs` (eventual removal)

After refactor, this file can be removed if no other consumers exist.

## Testing

### Manual Tests

1. Open agent panel in v2 mode
2. Verify session history loads
3. Create new thread, verify it appears in history
4. Delete a thread, verify it's removed
5. Delete all threads, verify history is cleared
6. Search threads, verify filtering works
7. Open a thread from history, verify it loads

### Automated Tests

Update any existing v2 tests that use `HistoryStore` to use the new architecture.

## Migration Notes

- `agent_ui_v2` is behind a feature flag, so this is lower risk
- Can be done incrementally - update `AcpThreadHistory` first, then `AgentsPanel`
- The `AgentSessionList` trait already exists and is battle-tested in v1

## Success Criteria

- [ ] `agent_ui_v2` no longer imports `HistoryStore`
- [ ] Agent session history comes from `AgentConnection::session_list()`
- [ ] Text thread history comes from `TextThreadStore` directly
- [ ] All existing v2 functionality works (list, search, delete, open)
- [ ] `HistoryStore` can be removed from `agent` crate
- [ ] `cargo check -p agent_ui_v2` passes
- [ ] No clippy warnings in changed files
