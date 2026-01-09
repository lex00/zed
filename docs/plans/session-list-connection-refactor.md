# Plan: Agent Session History via `AgentConnection`

## Problem Statement

Zed historically mixed agent sessions (ACP threads) and text threads together in a single `HistoryStore`. This created coupling between UI and persistence details and made it difficult to support agent-server-backed connections.

This refactor separates the two concepts:

- **Agent session history** comes from `AgentConnection` capabilities (connection-backed, agent-specific)
- **Text thread history** is owned by `TextThreadStore`

## Goals

1. **Agent Session History** lists agent sessions only, sourced from `AgentConnection::session_list()`
2. **Text Thread History** lists text threads only, sourced from `TextThreadStore`
3. **Single "History" action** routes based on current mode (agent vs text thread)
4. **Native agent** supports: list, open/resume, delete, delete-all
5. **Consistent ordering** across "Recent" section, navigation menu, and History view
6. **Thread completions** work in both agent panel message editor and inline assist

## Non-Goals

- Implementing `cwd` filtering (parameter carried for forward compatibility)
- Modifying ACP protocol/SDK methods
- Supporting remote ACP agents (deferred until protocol support exists)

---

## Implementation Status: ✅ COMPLETE

### API Surface (`acp_thread` crate)

- `AgentConnection::session_list()` returns `Option<Rc<dyn AgentSessionList>>`
- `AgentSessionList` trait with:
  - `list(params, cx)` - returns sessions
  - `supports_delete()` - capability flag
  - `delete_session(id, cx)` - delete single session
  - `delete_all_sessions(cx)` - delete all sessions
- Types: `SessionListParams`, `SessionListResult`, `AgentSessionInfo`
- `AgentSessionInfo::display_title()` - unified title fallback logic
- `EmptySessionList` - public placeholder provider

### Native Agent Implementation

- `NativeAgentConnection::session_list()` returns provider backed by `ThreadsDatabase`
- Supports all operations: list, delete, delete-all
- Sessions sorted by `updated_at desc`

### Agent Panel Behavior

- **History routing**: Single "History" action routes to agent or text thread history based on mode
- **Agent session history** (`AcpThreadHistory`): Lists sessions from provider, supports search, time bucketing, delete
- **Text thread history** (`TextThreadHistory`): Lists text threads from `TextThreadStore`, same UX as agent history
- **Unified ordering**: "Recent" section, navigation menu, and History all use same provider-backed snapshot
- **Cold-start UX**: Subscribes to `ThreadReady` event to populate snapshot when thread loads

### Shared Agent Sessions (`AgentSessions` type)

Created `AgentSessions` wrapper type with visibility-based access control:

```rust
// In agent_panel.rs
#[derive(Clone)]
pub struct AgentSessions {
    inner: Arc<Mutex<Vec<AgentSessionInfo>>>,
}

impl AgentSessions {
    // Private - only AgentPanel can write
    fn new() -> Self { ... }
    fn update(&self, sessions: Vec<AgentSessionInfo>) { ... }
    fn clear(&self) { ... }
    fn is_empty(&self) -> bool { ... }

    // Public - anyone with a clone can read
    pub fn get(&self) -> Vec<AgentSessionInfo> { ... }
}
```

This enables:

- `AgentPanel` owns and updates the sessions
- `MessageEditor` (agent thread view) gets thread completions via shared reference
- `PromptEditor` (inline assist) gets thread completions via shared reference
- All consumers see live updates when sessions change
- Tests can pass `None` for no completions or create their own `AgentSessions`

### Thread Completions

- **Agent panel message editor**: Takes `Option<AgentSessions>`, uses `.get()` in `thread_entries()`
- **Inline assist**: Looks up `AgentPanel` from workspace, gets shared `AgentSessions`
- **Terminal inline assist**: Same pattern as inline assist
- Both convert `AgentSessionInfo` to `ThreadCompletionEntry` on access

---

## Completed Work

### Phase 1: Core Refactor (Previous Sessions)

- [x] Added `AgentConnection::session_list()` API
- [x] Implemented `AgentSessionList` trait and types
- [x] Implemented native agent session list provider
- [x] Created `AcpThreadHistory` view for agent sessions
- [x] Created `TextThreadHistory` view for text threads
- [x] Implemented history routing based on mode
- [x] Implemented provider-backed snapshot for unified ordering
- [x] Added cold-start UX with `ThreadReady` event
- [x] Added integration tests for history behavior

### Phase 2: HistoryStore Removal (Previous Sessions)

- [x] Removed `history` field from `NativeAgent`
- [x] Removed `history` parameter from `NativeAgentServer::new()`
- [x] Removed `history_store` parameters from various constructors
- [x] Added `ThreadCompletionEntry` type for completions
- [x] Updated navigation menu to use `TextThreadStore` directly

### Phase 3: AgentSessions Wrapper (Current Session)

- [x] Removed `history_store` field from `AgentPanel`
- [x] Created `AgentSessions` wrapper type with visibility-based access control
- [x] Updated `AgentPanel` to use `AgentSessions` instead of raw `Vec`
- [x] Updated `AgentPanel::agent_sessions()` to return cloned `AgentSessions`
- [x] Updated `PromptEditor` to take `Option<AgentSessions>`
- [x] Updated `PromptEditorCompletionProviderDelegate` to use `AgentSessions`
- [x] Updated `MessageEditor` to take `Option<AgentSessions>`
- [x] Updated `AcpThreadView::new()` to take and pass `Option<AgentSessions>`
- [x] Updated inline assist to look up `AgentSessions` from `AgentPanel`
- [x] Updated terminal inline assist similarly
- [x] Updated all tests and `agent_ui_v2` for new signatures
- [x] Fixed all compiler warnings and clippy errors

---

## Remaining Work (Optional Follow-up)

### Low Priority

1. **Remove `HistoryStore` from `agent` crate**
   - Currently still exists but unused by `agent_ui`
   - Still used by `agent_ui_v2` (behind feature flag)
   - Could simplify to just `load_agent_thread()` function

2. **Text thread completions for inline assist**
   - `ThreadCompletionKind::TextThread` variant exists but is unused
   - Would need to also query `TextThreadStore` in addition to `AgentSessions`
   - Lower priority since text threads are less commonly referenced

3. **ACP agent-server-backed connections**
   - `AcpConnection` currently returns `None` for `session_list()`
   - Enable when ACP protocol adds session listing support
   - Gate behind `feature_flags::AcpBetaFeatureFlag`

---

## Key Files Modified

### Core Agent Panel

- `agent_ui/src/agent_panel.rs` - `AgentSessions` type, removed `HistoryStore`, passes sessions to thread views

### Completions

- `agent_ui/src/completion_provider.rs` - `ThreadCompletionEntry` type
- `agent_ui/src/inline_prompt_editor.rs` - `PromptEditor` takes `Option<AgentSessions>`
- `agent_ui/src/inline_assistant.rs` - Looks up `AgentSessions` from `AgentPanel`
- `agent_ui/src/terminal_inline_assistant.rs` - Same pattern as inline assist
- `agent_ui/src/acp/message_editor.rs` - Takes `Option<AgentSessions>`

### Thread Views

- `agent_ui/src/acp/thread_view.rs` - Takes and passes `Option<AgentSessions>`
- `agent_ui/src/acp/entry_view_state.rs` - Passes `None` for edit mode

### History Views

- `agent_ui/src/acp/thread_history.rs` - Agent session history
- `agent_ui/src/acp/text_thread_history.rs` - Text thread history

### V2 UI (Feature-flagged)

- `agent_ui_v2/src/agent_thread_pane.rs` - Updated for new signatures

---

## Key Commands

```bash
# Check compilation
cargo check -p agent_ui

# Run clippy
./script/clippy -p agent_ui

# Run tests
cargo test -p agent_ui -q
```

---

## Success Criteria (✅ All Met)

- ✅ Agent session history lists agent sessions only via `AgentConnection::session_list()`
- ✅ Text thread history is separate, sourced from `TextThreadStore`
- ✅ Native agent supports list/open/delete/delete-all
- ✅ Navigation menu is mode-specific (agent sessions vs text threads)
- ✅ "History" routes by current mode
- ✅ Ordering is consistent across Recent, navigation menu, and History
- ✅ Thread completions work in agent panel message editor
- ✅ Thread completions work in inline assist
- ✅ Thread completions get live updates when sessions change
- ✅ `AgentSessions` enforces read-only access for consumers
- ✅ Code follows GPUI conventions
- ✅ All tests pass, no warnings, clippy clean
