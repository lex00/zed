# Plan: Agent history lists behind `AgentConnection` (v1-focused, implementer-ready)

## Goal

Move agent session history + text thread history to a capability-driven, connection-backed model with correct reset semantics, correct UX gating, and reliable “resume/open session” behavior.

This plan is intentionally concrete and file-oriented so it can be picked up by multiple people without needing additional context.

## Scope (v1)

This plan prioritizes v1 (`crates/agent_ui/...`) and treats v2 as out of scope for now.

## Current issues to fix

1. **Stale history list after switching top-level agents** (especially when the new agent does not support history).
2. **Undated sessions create a dedicated bucket header** (should be folded into “All”; rows should tolerate `updated_at: None`).
3. **TextThread history list is empty until typing** (needs to update when the store loads/changes).
4. **History navigation/menu should not appear if unsupported**.
5. **Recent sessions list renders after conversation starts** (should be pre-conversation only).
6. **Clicking history/recent items does nothing / logs “Session not found”** (resume must be wired correctly and should not silently no-op).

## Ground truth from diffs (key facts)

- `crates/agent_ui/src/acp/thread_history.rs` now:
  - takes an `Rc<dyn AgentSessionList>` directly
  - lists sessions via `session_list.list(SessionListParams::default(), cx)` and sorts by `updated_at`
  - builds bucketed vs search lists on background executor
  - uses `delete_session` / `delete_all_sessions` capabilities
  - introduces a `TimeBucket::Undated` bucket when `updated_at` is `None` (causes issue #2)

- `crates/agent_ui/src/acp/text_thread_history.rs` (new):
  - reads `TextThreadStore::unordered_text_threads()` and sorts by mtime
  - recomputes on search editor changes and explicit deletes
  - does **not** observe the store, so if the store populates after view creation the UI stays empty until the user types (issue #3)

- `crates/agent_ui/src/agent_panel.rs`:
  - moved “Recent” session list into the panel and renders it alongside the thread view (issue #5)
  - uses `connection.resume(session_id)` on the **active thread’s connection** for:
    - history list clicks
    - recent list clicks
    - navigation menu clicks
      This can be the wrong connection for those session ids (issue #6)
  - currently maintains session list state via:
    - `agent_session_list: Option<Rc<dyn AgentSessionList>>`
    - `agent_sessions: AgentSessions` (`Arc<Mutex<Vec<AgentSessionInfo>>>`)
    - capability flags + refresh task handle

## Definitions

- **Top-level agent**: the currently selected agent/provider (not a session/thread).
- **Session list capability**: `AgentConnection::session_list(&mut App) -> Option<Rc<dyn AgentSessionList>>`.
- **Agent session**: `AgentSessionInfo { session_id, title, updated_at, ... }`.

## High-level design (best design; churn is OK)

### Replace the shared snapshot with an entity model

Replace the current `AgentSessions` (`Arc<Mutex<Vec<_>>>`) approach with a single entity:

- `Entity<AgentSessionsModel>`

This entity is the sole owner of:

- provider + capability flags
- session list contents
- refresh tasks and cancellation
- an agent identity key and generation guard so old refreshes cannot overwrite new state

All UI surfaces (history view, recents list, nav menu) render from this model.

### Ensure resume/open session uses the correct agent context

All “open session” actions must route through a single panel method that:

- can create an appropriate thread/connection if none exists yet
- resumes through a connection valid for the agent that produced the session list
- surfaces visible errors instead of silently no-op

---

## Implementation guide: Entity-based `AgentSessionsModel`

### A. New model type (add to `crates/agent_ui/src/agent_panel.rs` or a nearby existing file)

Create:

- `struct AgentSessionsModel { ... }`
- owned as `Entity<AgentSessionsModel>` inside `AgentPanel`

#### A.1 AgentKey (pick a concrete key)

Define a small key type in v1. It must be cheap to clone and comparable.

Minimum viable:

- `enum AgentKey { Native, External(ExternalAgentServerName) }`

If model/provider affects session namespace, include that too (optional at first).

#### A.2 Model fields (concrete)

`AgentSessionsModel` should include:

- `agent_key: Option<AgentKey>`
- `provider: Option<Rc<dyn AgentSessionList>>`
- `supports_delete: bool`
- `supports_delete_all: bool`
- `sessions: Vec<AgentSessionInfo>`
- `error_message: Option<SharedString>`
- `refresh_generation: u64`
- `refresh_task: Option<Task<()>>`

#### A.3 Model methods (concrete signatures)

Implement:

- `fn clear(&mut self, cx: &mut Context<Self>)`
  - increments generation
  - cancels refresh_task (drop it)
  - sets provider None + capabilities false
  - clears sessions + error_message
  - `cx.notify()`

- `fn set_agent(&mut self, agent_key: AgentKey, provider: Option<Rc<dyn AgentSessionList>>, cx: &mut Context<Self>)`
  - if `self.agent_key != Some(agent_key)`:
    - call `clear(...)` first, then set `self.agent_key = Some(agent_key)`
  - increments generation
  - set `self.provider = provider`
  - update capabilities from provider
  - clear sessions if provider changed or provider is None
  - `cx.notify()`

- `fn refresh(&mut self, cx: &mut Context<Self>)`
  - if provider is None:
    - clear sessions + error_message; `cx.notify()`; return
  - capture `generation = self.refresh_generation`
  - start list task: `provider.list(SessionListParams::default(), cx)`
  - store spawned task in `self.refresh_task`
  - on completion:
    - ignore results if generation mismatch
    - sort sessions by `updated_at desc` (None last)
    - store sessions, clear error_message, `cx.notify()`

- `fn delete_session(&mut self, session_id: acp::SessionId, cx: &mut Context<Self>) -> Option<Task<()>>`
  - if provider missing or !supports_delete: return None
  - run deleter, and on success call `refresh`
  - log errors, and set `error_message` if you want UI feedback

- `fn delete_all(&mut self, cx: &mut Context<Self>) -> Option<Task<()>>`
  - similar; on success call `refresh`

### B. Replace current panel state with the entity (file: `crates/agent_ui/src/agent_panel.rs`)

#### B.1 Remove these fields from `AgentPanel`

- `agent_session_list: Option<Rc<dyn AgentSessionList>>`
- `agent_sessions: AgentSessions`
- `agent_delete_supported: bool`
- `agent_delete_all_supported: bool`
- `_agent_sessions_refresh_task: Task<()>`

#### B.2 Add this field to `AgentPanel`

- `agent_sessions_model: Entity<AgentSessionsModel>`

Initialize it in `AgentPanel::new` (near other entities).

#### B.3 Replace all usage sites

Concrete replacement checklist (grep-driven):

- `self.agent_sessions.is_empty()` → `self.agent_sessions_model.read(cx).sessions.is_empty()`
- `self.agent_sessions.list()` → `self.agent_sessions_model.read(cx).sessions.iter()...`
- `self.agent_sessions.get(&id)` → `self.agent_sessions_model.read(cx).sessions.iter().find(...)`
- `self.agent_delete_supported` → `self.agent_sessions_model.read(cx).supports_delete`
- `self.agent_delete_all_supported` → `self.agent_sessions_model.read(cx).supports_delete_all`
- `self.refresh_agent_session_snapshot(cx)` → `self.agent_sessions_model.update(cx, |m, cx| m.refresh(cx))`
- `self.set_agent_session_list_provider(provider, cx)` → `self.agent_sessions_model.update(cx, |m, cx| m.set_agent(agent_key, Some(provider), cx))`

Then delete `set_agent_session_list_provider` and `refresh_agent_session_snapshot`.

### C. Fix stale list on agent switch (#1) + hide history UI (#4)

**Where to implement**

- `crates/agent_ui/src/agent_panel.rs`: the code path that changes `selected_agent` / creates new threads / switches agents.

**Concrete steps**

1. Identify the “top-level agent changed” point(s). For each:
   - compute `AgentKey`
   - determine `provider = connection.session_list(cx)` (or None)
   - call: `agent_sessions_model.update(cx, |m, cx| m.set_agent(agent_key, provider, cx))`
   - if provider is Some: call refresh
2. If provider is None:
   - ensure history navigation/menu items are not rendered (see section F)
   - if currently in history view: switch to a safe view (thread view)

### D. Fix undated bucket header (#2) (file: `crates/agent_ui/src/acp/thread_history.rs`)

**Current behavior**
`build_bucketed_items` uses `TimeBucket::Undated` and emits a bucket separator.

**Target behavior**

- Do not create an “Undated” bucket separator.
- Undated sessions appear in the same list (“All”) without their own header.
- Rows already handle `updated_at: None` by hiding the timestamp label.

**Concrete implementation**

- Remove `TimeBucket::Undated` usage entirely or, minimally:
  - for `updated_at: None`, do not push a `BucketSeparator`
  - push the entry with `format: EntryTimeFormat::DateAndTime` (format doesn’t matter if timestamp is hidden)
- Ensure sorting places `None` timestamps last.

### E. Fix TextThread history empty until typing (#3) (file: `crates/agent_ui/src/acp/text_thread_history.rs`)

**Root cause**
No subscription/observation on `TextThreadStore`.

**Concrete steps**

- Change `TextThreadHistory::new(...)` to accept `Entity<TextThreadStore>` (not only a `WeakEntity`) OR obtain an `Entity` in the panel and pass it.
- Add `cx.observe(&text_thread_store_entity, |this, _, cx| this.update_visible_items(true, cx));`
  - store subscription in `_subscriptions`
- Keep the existing search editor subscription.

### F. Fix “Recent list shows after conversation starts” (#5) (file: `crates/agent_ui/src/agent_panel.rs`)

**Current**
`render_agent_session_recent_list` renders whenever in `ActiveView::ExternalAgentThread` and the snapshot is non-empty.

**Target**
Render only in “pre-conversation” state.

**Concrete definition (pick one and implement)**

- Option 1: pre-conversation = thread has no session yet / not ready
- Option 2: pre-conversation = thread has zero messages
- Option 3: pre-conversation = no active thread selected

Pick the one consistent with your UX; implement with a boolean:

- `fn conversation_started(&self, cx: &App) -> bool` (or similar)

Then gate:

- `if conversation_started { return empty }`

### G. Fix “click history/recent does nothing / Session not found” (#6) (file: `crates/agent_ui/src/agent_panel.rs`)

**Root cause**
Resume is attempted through the active thread’s connection, which may not match the provider that listed those sessions, and sometimes there is no active thread.

**Concrete steps**

1. Create a single method on `AgentPanel`:
   - `fn open_session(&mut self, session_id: acp::SessionId, window: &mut Window, cx: &mut Context<Self>)`
2. `open_session` should:
   - read `agent_key` from `agent_sessions_model`
   - obtain a connection suitable for that key:
     - if a thread view already exists for that agent, use its connection
     - otherwise create a new thread view / server connection for that agent (reuse your “new thread” machinery)
   - call `resume(session_id)` on that connection
   - on success: refresh model
   - on failure: show a user-facing error (and log session id)
3. Update all click call sites to call `open_session`:
   - history view subscription (`ThreadHistoryEvent::OpenSession`)
   - recent list click handler
   - navigation menu click handler
4. Eliminate silent early-returns:
   - if you can’t get a connection or resume is unsupported, log and show UI feedback.

### H. Optional cleanup: make history view render from the model

Once `AgentSessionsModel` exists, you can remove duplicate fetch logic by changing `AcpThreadHistory` to take the model entity and render from it.

This is optional in the first pass, but recommended so history and recents always agree.

---

## Execution order (recommended)

1. Introduce `AgentSessionsModel` entity and migrate `AgentPanel` reads/writes to it.
2. Fix resume/open session path (#6) by routing all clicks through `AgentPanel::open_session`.
3. Fix stale state on agent switch + hide history UI (#1/#4).
4. Fix undated bucket (#2).
5. Fix TextThread history observation (#3).
6. Fix recent list gating (#5).
7. (Optional) Refactor `AcpThreadHistory` to render from the model.

## Acceptance checklist (copy/paste for QA)

- Switch top-level agents:
  - sessions list resets immediately; no stale sessions are visible
  - history nav/menu is hidden if unsupported
- TextThread history:
  - list populates without typing once store has threads
- Agent session history:
  - no “Undated/Other” bucket header
  - undated sessions are visible in All and show no timestamp
- Resume:
  - clicking history entry opens/resumes session
  - clicking nav menu entry opens/resumes session
  - clicking recent entry opens/resumes session
  - failures show a visible error (not silent no-op)
- Recent list:
  - visible only pre-conversation
  - delete works and list refreshes
  - same logic exists, and `TimeBucket::Undated` is displayed as `"Other"`.

- `build_bucketed_items(...)` currently does:
  - `TimeBucket::from_dates(...)` if `updated_at.is_some()`
  - `TimeBucket::Undated` otherwise
  - pushes `ListItemType::BucketSeparator(entry_bucket)` when bucket changes

**Plan**

- Adjust bucketing logic so undated sessions do not introduce a new bucket separator.
- Minimal, safe approach given the current structure:
  - Skip pushing bucket separators for undated sessions.
  - Continue pushing `ListItemType::Entry { entry: session, format: ... }` for undated sessions, using a format that makes sense for “All” (likely `EntryTimeFormat::DateAndTime` or a new “None” format, but since `render_history_entry` already suppresses timestamp display when `updated_at` is `None`, the format won’t matter for display).
- If the history UI supports selecting buckets (e.g. “Today”, “Yesterday”, “All”), ensure undated sessions show up under “All” and not under a separate header.

**Acceptance criteria**

- No “Undated” / “Other” / “Unknown date” bucket header is rendered (v1 + v2).
- Sessions with `updated_at: None` still appear in the list (at least in “All” view).

---

### 3) TextThread history shows nothing until typing query

**Problem**
TextThread history view doesn’t show any items until you start typing a query.

**Concrete finding**
In `crates/agent_ui/src/acp/text_thread_history.rs`, `TextThreadHistory::new(...)` calls `update_visible_items(false, cx)` immediately, but there is no subscription/observation of the `TextThreadStore` entity. So if `TextThreadStore` populates _after_ the view is constructed (common if it loads asynchronously), the UI won’t recompute `visible_items` until something else triggers it (typing updates the search editor, which triggers recomputation).

**Plan**

- Make `TextThreadHistory` reactive to store changes:
  - Add a `cx.observe(&store_entity, ...)` or a `cx.subscribe(...)`-style hook (whatever is appropriate for `TextThreadStore`) and call:
    - `this.update_visible_items(true/false, cx)`
  - Store the resulting subscription in `_subscriptions`.
- If `TextThreadStore` has an explicit async “load/refresh” API:
  - Kick it off when history view is created (or when switching into `HistoryText`), then update visible items on completion.
- Keep query behavior identical:
  - empty query => `build_bucketed_items(...)`
  - non-empty query => `build_search_items(...)`

**Acceptance criteria**

- Open TextThread history with empty query:
  - once `TextThreadStore` has data, the list populates without typing.
- Deleting a thread still updates the view as it does now (it already calls `update_visible_items(...)` after delete).

---

### 4) Hide history navigation menu if agent doesn’t support history (v1 + v2)

**Problem**
UI still shows history navigation when agent has no history capability.

**Concrete implication from v1 panel changes**
Because `AgentPanel` now carries `agent_session_list: Option<Rc<dyn AgentSessionList>>` and a shared `agent_sessions` snapshot, the panel is in a good position to gate UI. But it also means you must be careful to clear `agent_sessions` (or otherwise not render them) when `agent_session_list` becomes `None`, to avoid stale sessions showing in any navigation-related list.

**Concrete implication from v2 panel changes**
In `crates/agent_ui_v2/src/agents_panel.rs`, history is initialized with an `EmptySessionList` and then later replaced by `NativeAgentConnection::session_list(cx)` once an async connection is established. Combined with `AgentThreadPane`’s ability to open sessions using only a `SessionId`, this means:

- history navigation should be gated on “do we currently have a real provider?” (not `EmptySessionList`)
- switching agents (or providers) must reset:
  - the provider (`agent_session_list`)
  - the snapshot (`agent_sessions`)
  - and any active history view state
    otherwise v2 can show stale session lists/titles even when the current agent does not support history.

**Plan**

- Gate both v1 and v2 navigation entry creation on:
  - `connection.session_list(cx).is_some()`
  - (in v1 specifically, this likely maps to `AgentPanel.agent_session_list.is_some()`)
- Also gate any history route/view rendering:
  - if unsupported, render nothing (or a minimal empty state) and ensure no stale list is visible.
- When switching agents:
  - if the current active view is `HistoryAgent` and the new agent lacks history, switch to a safe default view (e.g. the thread view).
  - clear `agent_sessions` at the same time (issue #1).

**Acceptance criteria**

- For an agent without `session_list`, history navigation entry is not shown.
- Switching from a history-capable agent to a non-capable agent does not leave history UI visible and does not show stale sessions anywhere.

---

### 5) Recent threads list moved to agent panel renders after conversation starts

**Problem**
Recent list is now in the agent panel and still renders after the conversation has started.

**Plan**

- Define a single source of truth for “conversation started” that both v1 and v2 can access.
- Recommended approach:
  - Render recent threads only when there is no active session/thread selected (or when the active thread has zero messages), depending on the intended UX.
- Implementation approach depends on architecture, but the intent is:
  - move the _decision_ about “pre-conversation vs in-conversation” closer to the thread/session state
  - keep the _data_ (recent sessions) sourced from the same session list provider to avoid duplicate fetches

**Acceptance criteria**

- Once a conversation has started, recent list is not rendered.
- When returning to pre-conversation state, recent list can render again.

## 6) Fix: clicking agent session history items does nothing / logs “Session not found”

**Problem**
Clicking items in:

- the agent session history view, and/or
- the agent navigation “recently opened” menu

does nothing, and sometimes logs “Session not found”.

**Likely root causes (based on the current architecture)**

1. Resume is attempted through the _currently active thread’s_ connection, but the session list was fetched from a provider that may not correspond to that connection.
2. Resume requires `active_thread_view()` to exist; if the user hasn’t started an agent thread yet (or the thread isn’t ready), click handlers early-return.
3. If session ids are stale (e.g. after switching agents without resetting), resume may be invoked on the wrong agent, producing “Session not found”.

**Plan**

- Introduce a consistent “history connection context” for agent sessions:
  - When installing `agent_session_list` / refreshing the session snapshot, also store an identifier for the agent/connection that owns these sessions (e.g. a connection handle, or enough info to rebuild the correct connection).
  - All “open session” actions (history view, recent list, navigation menu) must resume through this same context.
- Make resume possible even when there’s no active thread yet:
  - If there is no active thread/connection to resume through, create or activate an appropriate thread/connection for the selected top-level agent, then run resume.
  - If that isn’t feasible yet, disable click affordances with an explanation (temporary mitigation).
- Improve failure handling:
  - If `resume` returns `None` or the resume task errors, surface a visible UI error (toast/callout) and include the session id in logs.
  - If resume fails with “Session not found”, treat that as a signal that either:
    - the session list is stale (needs refresh/reset), or
    - the wrong connection was used (bug).

**Acceptance criteria**

- Clicking a session in history opens/resumes that session reliably.
- Clicking a session in the navigation menu opens/resumes that session reliably.
- If the session can’t be resumed, the user gets a clear UI error, and the UI does not silently no-op.
- Switching agents cannot leave you in a state where old sessions are clickable but always fail.

## 7) Best design: `Entity<AgentSessionsModel>` as the single source of truth (recommended)

You said churn is acceptable. In that case, the cleanest design is to replace the ad-hoc `Arc<Mutex<Vec<_>>>` snapshot with a dedicated entity that owns:

- the active session list provider
- the loaded sessions
- capability flags
- in-flight refresh task(s)
- a stable “agent key” so old async results can’t clobber new state
- (optionally) the correct “resume connection context” used to open sessions

### 7.1 Proposed model API

Create `Entity<AgentSessionsModel>` (owned by `AgentPanel`) with roughly:

- **Identity / capability**
  - `agent_key: Option<AgentKey>` (whatever uniquely identifies the top-level agent + server)
  - `provider: Option<Rc<dyn AgentSessionList>>`
  - `supports_delete: bool`
  - `supports_delete_all: bool`

- **Data**
  - `sessions: Vec<AgentSessionInfo>`
  - `last_refreshed_at: Option<Instant>` (optional)
  - `error: Option<anyhow::Error>` (optional, for UI empty states)

- **Async**
  - `refresh_task: Option<Task<()>>` (drop to cancel)
  - `refresh_generation: u64` (increment on agent switch/provider change; check in completion path)

- **Public methods**
  - `clear_for_agent_switch(agent_key: AgentKey, cx)`
    - sets `provider = None`
    - clears sessions + errors
    - cancels refresh task
    - resets capabilities
  - `set_provider(agent_key: AgentKey, provider: Option<Rc<dyn AgentSessionList>>, cx)`
    - updates capabilities from provider
    - cancels old refresh
    - clears sessions if provider changed or provider is None
  - `refresh(cx)`
    - if provider is None: clear sessions + return
    - run `provider.list(SessionListParams::default(), cx)` and apply results only if generation matches
  - `delete_session(session_id, cx) -> Task<Result<()>>` (optional convenience)
  - `delete_all(cx) -> Task<Result<()>>` (optional convenience)

### 7.2 Why `Entity` is the best fit here

- All dependent UI can simply `observe` this entity and rerender when it changes.
- It becomes easy to enforce invariants:
  - one writer (the entity itself)
  - no mutexes in UI code
  - async cancellation + generation guards are centralized
- It naturally supports:
  - “always reset on top-level agent switch”
  - “hide history nav if unsupported”
  - “keep recents + history + menu consistent”

### 7.3 Migration steps (v1)

1. Replace `AgentSessions` (`Arc<Mutex<...>>`) with `Entity<AgentSessionsModel>` in `AgentPanel`.
2. Convert all reads of the session list snapshot to:
   - `sessions_model.read(cx).sessions` (or a safe accessor returning an iterator/slice).
3. Replace `AgentPanel.refresh_agent_session_snapshot` with `sessions_model.update(...).refresh(...)` or `sessions_model.update(..., |m, cx| m.refresh(cx))`.
4. Change `AcpThreadHistory` to stop “owning” a provider:
   - either:
     - keep `AcpThreadHistory` as-is but always call `set_session_list(...)` from the sessions model changes, or
   - (preferable):
     - have `AcpThreadHistory` accept `Entity<AgentSessionsModel>` and render from it directly (no duplicated fetch logic in the view).
5. Change the navigation menu rebuild to read from `AgentSessionsModel` and rebuild when the entity changes.

### 7.4 Migration steps (v1) for correctness with “resume”

In addition to data ownership, the entity should carry enough information to resume sessions correctly:

- store an `AgentKey` that identifies which top-level agent the sessions belong to
- the panel should map `AgentKey -> Rc<dyn AgentConnection>` (or be able to create a connection on demand)
- “OpenSession” actions should:
  - use the `AgentKey` from the sessions model to obtain the correct connection
  - create an agent thread if needed
  - call `resume(session_id)` through that connection

This avoids the current failure mode where clicks resume through the _active thread’s_ connection even if it’s unrelated.

## Cross-cutting: testing & verification

### Manual test matrix

1. Agent supports history:
   - history lists sessions, search works, delete works when supported.
2. Agent does not support history:
   - no history nav entry, no stale list.
3. Switching agents:
   - switching clears history model state and cancels/ignores old async refresh results.
4. TextThread history:
   - loads items without typing, search filters correctly.
5. Missing timestamps:
   - no “Undated” header, undated items are still present under “All”.

### Suggested targeted tests

- Use the existing stub/mocks (`MockAgentSessionList` in `acp_thread::connection` test support) to verify:
  - switching providers resets visible items
  - undated sessions do not create a separate bucket header
- Add a TextThread history test that:
  - populates `TextThreadStore` after view creation
  - verifies the view updates without query input

## Open questions (answering these will reduce rework)

1. What is the canonical “top-level agent identity” in the UI layer (provider id? server id? connection instance pointer?) that we should key history state resets on?
2. What is the exact definition of “conversation started” for hiding recents?
   - session selected?
   - first user message sent?
   - any messages exist?
3. For sessions with `updated_at: None`, do you want them:
   - always at the bottom of All, or
   - interleaved by some other heuristic?

## Execution order

1. Capability gating + reset-on-agent-switch (fix correctness + stale UI) — v1 + v2.
2. TextThread history store observation/load trigger (fix empty list until typing) — v1.
3. Remove `Undated` / “Other” bucket separator behavior (fold into All) — v1 + v2.
4. Recent list visibility rule (pre-conversation only) — v1 + v2.
5. Add/adjust tests for regressions (including v2 coverage where possible).
