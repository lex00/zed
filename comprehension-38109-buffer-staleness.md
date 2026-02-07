# Comprehension: macOS Buffer Empty After External File Rewrite

## Problem Statement

On macOS (and likely all platforms), when git worktree operations externally delete and rewrite files, open buffers in Zed show the file content as permanently empty even though the file has correct content on disk. Only a full Zed quit+restart fixes it.

## Root Cause Analysis

### Suspected Issue: Empty Buffer After Delete+Recreate

When a file that has an open buffer is deleted and recreated externally, the buffer sometimes shows empty content even though the file has content on disk. The issue likely stems from **entry_id mismatch** and **timing of snapshot updates**, but the exact failure mode is still being investigated.

#### The Issue in Three Steps

**Step 1: File is deleted (external event)**
- FS event triggers `worktree.rs:3990 BackgroundScanner::process_events()`
- Worktree updates its snapshot, removing the entry
- The deleted entry had an `entry_id` (e.g., `ProjectEntryId(42)`)
- A `PathChange::Removed` event is emitted for this path

**Step 2: Entry ID mismatch when file is recreated**
- File is recreated at the same path
- Worktree creates a **new entry with a new entry_id** (e.g., `ProjectEntryId(43)`)
- A `PathChange::Added` event is emitted with the **new entry_id**

**Step 3: Buffer lookup fails - orphaned state**
- `BufferStore::local_worktree_entries_changed()` (buffer_store.rs:465-482) processes the events
- For `PathChange::Removed`, it calls `local_worktree_entry_changed()` with `entry_id=42` (the old ID)
- For `PathChange::Added`, it calls `local_worktree_entry_changed()` with `entry_id=43` (the new ID)

The buffer still holds **entry_id=42** from when the file was originally scanned.

#### Buffer Lookup Logic (buffer_store.rs:497-501)

```rust
let buffer_id = this
    .as_local_mut()
    .and_then(|local| local.local_buffer_ids_by_entry_id.get(&entry_id))
    .copied()
    .or_else(|| this.path_to_buffer_id.get(&project_path).copied())?;
```

This tries two lookups:
1. **Entry ID lookup** (line 499): `local_buffer_ids_by_entry_id.get(&entry_id)`
2. **Path fallback** (line 501): `path_to_buffer_id.get(&project_path)`

### The Race Condition

When `PathChange::Removed` is processed with **old entry_id=42**:
- Lookup 1 finds the buffer (because the buffer was indexed under entry_id=42)
- The code updates the file to `DiskState::Deleted` (buffer_store.rs:546)
- Lines 579-581 remove the old entry_id from the index:
  ```rust
  if let Some(entry_id) = old_file.entry_id {
      local.local_buffer_ids_by_entry_id.remove(&entry_id);  // Removes 42
  }
  ```

When `PathChange::Added` is processed with **new entry_id=43**:
- Lookup 1 **fails** (entry_id=43 is not in the index yet)
- Lookup 2 should succeed via **path fallback**:
  - The `path_to_buffer_id` index still contains the entry for this path (because path didn't change during Removed)
  - `path_to_buffer_id.get(&(worktree_id, "/file"))` should find the buffer
  - Buffer should be updated with the new entry_id=43 and transitioned to `DiskState::Present`
  - `ReloadNeeded` event should be emitted (buffer.rs:1685-1686, transition from Deleted → Present)

**If this is working correctly**, the buffer should be reloaded and the content should appear.

**However, there are several potential failure modes:**
1. **Path lookup failure** — The path is removed from index during Removed, or path doesn't match exactly
2. **Snapshot race condition** — The snapshot snapshot may not yet include the recreated entry when Removed event is processed
3. **Reload execution** — ReloadNeeded event is emitted, but reload fails or the file is empty when loaded
4. **Event ordering** — Events are processed out of order or batched differently than expected

### Why "Permanently Empty"?

When reload is triggered (buffer.rs:1551-1651):

```rust
let file = this.file.as_ref()?.as_local()?;
Some((
    file.disk_state().mtime(),
    file.load_bytes(cx),  // Calls File::load_bytes()
    this.encoding,
))
```

The `File::load_bytes()` (worktree.rs:3269-3274) constructs the absolute path from the buffer's current file:

```rust
fn load_bytes(&self, cx: &App) -> Task<Result<Vec<u8>>> {
    let worktree = self.worktree.read(cx).as_local().unwrap();
    let abs_path = worktree.absolutize(&self.path);
    let fs = worktree.fs.clone();
    cx.background_spawn(async move { fs.load_bytes(&abs_path).await })
}
```

If the buffer has `DiskState::Deleted`, the reload may:
1. Return early because the file is marked as deleted, or
2. Actually load from disk and succeed, but the user won't see the content because the buffer remains stuck in a "deleted" state

More likely: **The buffer never gets a new `ReloadNeeded` event after the delete+recreate**, so `buffer.reload()` is never called.

### Event Coalescing Issue

From worktree.rs:77:
```rust
pub const FS_WATCH_LATENCY: Duration = Duration::from_millis(100);
```

FS events are batched over 100ms windows. If a file is deleted and recreated within this window:
1. Both changes may be coalesced into a single event batch
2. The worktree must distinguish between:
   - `PathChange::Removed` then `PathChange::Added` (two separate events)
   - Or `PathChange::Updated` or `PathChange::AddedOrUpdated` (single event)

If the worktree reports `PathChange::Removed` followed by `PathChange::Added` in the same batch:
- The `Removed` event sets `DiskState::Deleted`
- The `Added` event creates a **new entry_id** that's never matched to the buffer

### Why "Close Tab + Reopen" Doesn't Fix It

From buffer_store.rs, buffers are cached by entry_id and path. When you close and reopen a tab:
1. The buffer entity may be dropped, but it's still in the `BufferStore`
2. On reopen, the store looks up the buffer by path (buffer_store.rs:501)
3. **It finds the stale buffer with entry_id=42 and DiskState::Deleted**
4. The same orphaning issue occurs again

The buffer needs a **new snapshot entry with a matching entry_id** to break out of this state. But since the entry_id mismatched (old=42, new=43), it never re-associates.

### Why Only Quit+Restart Fixes It

On quit, all buffers are dropped. On restart:
1. The file is scanned fresh
2. A new entry is created with a new entry_id
3. A new buffer is created with a reference to that new entry_id
4. No mismatch occurs

## Code References

### Primary Issue: Orphaned Buffer After Delete+Recreate

- **Entry ID lookup** (buffer_store.rs:497-501)
- **File update logic** (buffer_store.rs:520-601)
  - **Entry ID management** (buffer_store.rs:578-587)
- **DiskState transitions** (language/buffer.rs:1672-1703)
  - **ReloadNeeded emission** (language/buffer.rs:1685-1686)
- **Reload implementation** (language/buffer.rs:1551-1651)
  - **File path resolution** (worktree.rs:3269-3274)

### Event Processing Chain

1. FSEvents/inotify → `BackgroundScanner::process_events()` (worktree.rs:3990)
2. Snapshot updated → `set_snapshot()` emits `Event::UpdatedEntries` (worktree.rs:1145-1167)
3. Event received → `BufferStore::local_worktree_entries_changed()` (buffer_store.rs:465-482)
4. For each change → `local_worktree_entry_changed()` (buffer_store.rs:484-608)

### FS Watch Configuration

- **Batch latency** (worktree.rs:77): `FS_WATCH_LATENCY = 100ms`
- **Watch registration** (worktree.rs:1078-1081)

## Hypothesized Root Causes

### Hypothesis 1: Snapshot Timing Issue (Most Likely)

When a file is deleted and recreated within the 100ms FS_WATCH_LATENCY window:

1. Both changes are detected in a single scan batch
2. `build_diff()` creates two separate changes: `(path="/file", id=42, Removed)` and `(path="/file", id=43, Added)`
3. **When Removed event is processed:**
   - Snapshot has already been updated to include the **new** entry_id=43
   - `snapshot.entry_for_path("/file")` returns entry 43 (not None as expected)
   - New file is created with `entry_id=43` (not preserved `entry_id=42`)
   - File stays at `DiskState::Present` (not transitioned to Deleted)
   - **No ReloadNeeded event is emitted** (no state change)
4. **When Added event is processed:**
   - Buffer is found via path lookup
   - Snapshot still shows entry_id=43
   - New file is created with same `entry_id=43`, path unchanged
   - New file compared to old file: **No difference** (both have entry_id=43)
   - **Return early** (line 556: `if new_file == *old_file { return None }`)
   - **No ReloadNeeded event is emitted**
   - **Buffer is never reloaded**

5. If the file happened to be empty when the snapshot was taken during the Removed event processing, the buffer content would reflect that empty state and never be updated.

### Hypothesis 2: Snapshot Race Condition

The snapshot is updated asynchronously in the background scanner. If a file is deleted and recreated:

1. Deleted event is processed with old snapshot (entry exists)
2. But by the time Added event is processed, the **old** snapshot is used (doesn't include recreated entry)
3. Path lookup fails to find the new entry
4. Buffer is updated to `DiskState::Deleted` instead of `DiskState::Present`
5. No ReloadNeeded event is emitted for the Added event

### Hypothesis 3: Path Lookup Fails Due to Collateral Removal

When the Removed event is processed and the file becomes `DiskState::Deleted`:

1. Path is normally preserved if path doesn't change (line 560 condition is false)
2. **But** if the old file state and new file state are **identical** (line 555), the function returns early
3. This could prevent the path from being registered properly
4. Then when Added event is processed, path lookup fails
5. Buffer is never updated

## Reproduction Scenario

**Steps to reproduce:**

1. Open a file in Zed (file is indexed with entry_id=42)
2. In a terminal, delete and immediately recreate the file:
   ```bash
   rm myfile.txt && echo "new content" > myfile.txt
   ```
3. Observe: The buffer in Zed shows empty content
4. Try: Close and reopen the tab → still empty
5. Try: Window reload → still empty
6. Try: Full Zed quit + restart → file content appears

**Why git worktree ops trigger this:**

Git worktree operations (like `git checkout --force`, `git reset --hard`) can rapidly delete and recreate multiple files. If these operations complete within the 100ms FS_WATCH_LATENCY window, they can trigger this bug.

## Recommended Fix

The fix should **prevent orphaning by ensuring entry_id is always updated together**, or **use path-based lookups for file identity after deletion**.

### Option A: Entry ID Reuse After Delete+Recreate (Preferred)

When a file is deleted and then recreated at the same path within a short time window, check if an old entry_id can be reused rather than creating a completely new one. This would keep `entry_id=42` consistent across the delete+recreate boundary.

**Pros:**
- Buffer stays indexed under the same entry_id
- Simpler state machine
- Path lookup fallback still works

**Cons:**
- Requires inode tracking or path+mtime deduplication
- Adds complexity to the scan process

### Option B: Path-Based Lookup Fallback After Deletion

When a buffer's entry_id no longer exists in the snapshot but the file was previously `DiskState::Deleted`, check the path lookup more aggressively.

Modify buffer_store.rs:527-530:
```rust
let snapshot_entry = old_file
    .entry_id
    .and_then(|entry_id| snapshot.entry_for_id(entry_id))
    .or_else(|| snapshot.entry_for_path(old_file.path.as_ref()));

// If the file was deleted but now exists at the same path, use the new entry
if matches!(old_file.disk_state, DiskState::Deleted) {
    snapshot_entry.or_else(|| snapshot.entry_for_path(old_file.path.as_ref()))
}
```

**Pros:**
- Minimal changes
- Leverages existing path lookup
- Fixes the immediate symptom

**Cons:**
- Doesn't address the underlying entry_id mismatch
- Path collisions could cause issues

### Option C: Don't Remove Entry ID Immediately on Deletion

Delay removing the old entry_id from the index when `DiskState::Deleted` is set. Reuse the same entry_id if the path reappears within N seconds.

**Pros:**
- Natural recovery from delete+recreate
- Works within filesystem latency windows

**Cons:**
- Memory leak if files are deleted and recreated under different names
- Need to define "N seconds" carefully

## Key Uncertainty: Snapshot Consistency

The critical unknown is: **When the Removed event is processed, does the snapshot already contain the newly recreated entry?**

If the worktree updates the snapshot **all at once** before emitting events:
- Both Removed and Added events see a consistent snapshot with both old_entry gone and new_entry present
- The Removed event may incorrectly treat the new_entry as the recreated file at the same path
- The Added event may find the buffer but see no changes and return early

If the worktree updates the snapshot **incrementally**:
- Removed event sees old_entry deleted, new_entry not yet added → creates Deleted file
- Added event sees new_entry added → should update to Present and emit ReloadNeeded

The code at buffer_store.rs:527-530 suggests the snapshot can have intermediate states where an entry is deleted but path doesn't yet exist. But the code also suggests a path fallback that should handle this.

## Test Case for Validation

To validate these hypotheses, create a test that:

1. Opens a file in Zed
2. Externally deletes and immediately recreates the file (within 100ms)
3. Checks that:
   - The buffer's DiskState transitions correctly (Deleted, then back to Present)
   - ReloadNeeded is emitted
   - Buffer content reflects the recreated file content (not empty)
4. Vary the timing and content of the recreated file to identify at which point the bug occurs

Specifically:
- Recreate file with same content → buffer should show content
- Recreate file with different content → buffer should show new content
- Recreate file as empty → buffer may show empty (but next modification should fix it)
- Delay recreate beyond 100ms → buffer should definitely work (different event batches)

## Next Steps

1. Verify which snapshot state (old, intermediate, or new) is visible to the Removed event handler
2. Add logging to trace buffer state transitions during delete+recreate
3. Implement a regression test for this scenario
4. Based on findings, implement one of the recommended fixes (Option A-C)
