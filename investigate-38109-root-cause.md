# Issue #38109: Root Cause Analysis

## Summary

An open buffer can become permanently empty when an external tool writes a file.

The standard write pattern — `File::create()` then `write_all()` — truncates
the file to 0 bytes before writing content. If Zed's scanner reads the file
during that truncation window, the buffer reloads to empty. Whether the buffer
recovers depends entirely on whether the final write produces a different mtime.
If the mtime matches (same-second granularity, coarse timestamps, or rapid
sequential writes), `file_updated()` sees no change and never triggers a second
reload. The buffer is stuck.

Reproduced deterministically on a real macOS filesystem.

## Investigation Path

Prior work by community contributors and earlier research on this branch
(specs 001–006) systematically ruled out hypotheses and narrowed the search:

- **Linux inotify stale registration** (PRs #46036, #47099): Real bug, but
  Linux-only and affects the project panel, not buffer content. Ruled out as
  the cross-platform cause.
- **FakeFs delete+recreate tests**: All passed. The buffer store's event
  handling logic is correct in isolation — the bug is timing-dependent.
- **RealFs delete+recreate tests**: Also passed. APFS nanosecond mtime means
  two `std::fs::write` calls produce different timestamps, so the second write
  always triggers a reload. This pointed toward mtime collision as the key
  variable.
- **EventKind::Other dropped**: Fixed on this branch (commit 1af6231). A real
  gap but requires FSEvents queue overflow — too rare to explain the volume of
  reports.
- **Scanner and entry_id reuse analysis**: Confirmed `process_events()` re-stats
  from disk and handles entry_id correctly. The scanner is not the problem.

Each dead end eliminated a layer until the actual gate was exposed:
`file_updated()` decides whether to reload based solely on mtime comparison.

## The Bug

### Sequence

1. User has `file.txt` open with content
2. External tool calls `std::fs::write("file.txt", "new content")`, which
   expands to `open(O_TRUNC)` + `write()` + `close()`
3. Between truncate and write, Zed's scanner processes an FSEvent
4. Scanner re-stats the file: 0 bytes, mtime T
5. `file_updated()` sees mtime changed → emits `ReloadNeeded`
6. `reload_impl()` reads 0 bytes → buffer becomes empty
7. `did_reload()` stamps `saved_mtime = T` — empty state is now "saved"
8. Tool finishes writing → file has content, mtime T (or T+ε)
9. Scanner re-stats: mtime T → same as saved → **no reload triggered**
10. Buffer permanently shows empty content

### The gate (buffer.rs:1683)

```rust
if old_state != new_state {  // DiskState::Present { mtime } comparison
    if !was_dirty && matches!(new_state, DiskState::Present { .. }) {
        cx.emit(BufferEvent::ReloadNeeded)
    }
}
```

Mtime-only. No file size. When mtime doesn't change, no reload.

### Why "close and reopen" doesn't fix it

`open_buffer()` (buffer_store.rs:855) returns the cached `Entity<Buffer>`
without checking disk. The buffer entity stays alive even after closing the tab
(held by subscriptions, predictions, etc.). Only `Workspace: reload` drops all
entities and forces fresh reads.

### Why it's getting worse

AI agents write files constantly while users have them open. `std::fs::write`
— the pattern every tool uses — is non-atomic. The truncation window has always
existed, but the frequency of external writes has increased dramatically.

## Reproduction

Two tests in `crates/project/tests/integration/project_tests.rs`:

**`test_buffer_reload_during_truncate_then_write`** — Reproduces the bug on a
real filesystem. Truncates file, flushes events (buffer goes empty), writes new
content, resets mtime to match truncated file's mtime, flushes again. Buffer is
stuck empty while file has content on disk.

**`test_buffer_recovers_from_truncate_when_mtime_differs`** — Control test.
Same sequence without resetting mtime. Buffer recovers. Confirms the mtime
comparison is the exact gate.

## Other Bugs in the #38109 Umbrella

The issue thread collects several distinct bugs with overlapping symptoms:

| Bug | Platform | What it affects | Status |
|---|---|---|---|
| inotify stale registration | Linux | Project panel | PRs #46036, #47099 (unmerged) |
| EventKind::Other dropped | macOS | Scanner misses rescan signals | Fixed on this branch |
| **Truncate-then-write race** | **All** | **Buffer content** | **Reproduced** |
| Network FS no events | WSL/NFS/FUSE | Everything | lilith's pollwatcher branch |
| Dirty buffer suppression | All | Buffer content after undo | Identified, not fixed |

The existing PRs address real bugs. They helped rule out the Linux watcher
lifecycle and narrow the search to the buffer reload path, where the mtime-only
comparison was found.

## Suggested Fix

Compare file size in addition to mtime in the reload decision. The scanner
already tracks size in `EntryKind::File(u64)`. A 0→N byte transition should
always trigger `ReloadNeeded`, regardless of mtime.
