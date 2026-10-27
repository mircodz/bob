# Bob roadmap

Consolidated on 2026-10-14. Prioritize dependable daily use, especially on Windows. Use the local VS Code Copilot implementation and Claude Code as primary references, with Codex and OpenCode as additional comparisons.

Refactor aggressively where ownership or data flow is wrong. Replace duplicated paths rather than adding parallel implementations. Preserve existing conversations, permissions, scoped instruction boundaries, and the CLI focus.

## 1. Session restoration and transcript performance — first priority

### Confirmed current problems

- Normal event-backed resume loads/deserializes the full journal five times and repeatedly reconstructs/clones history.
- Replay repeatedly scans all previous cells for retries, drafts, and completion state, producing quadratic work on growing transcripts.
- Ignored child events still increment the main view revision, triggering full-history exploration-group and visible-index reconstruction.
- The resume picker loads full session blobs just to produce short previews.
- Provider setup, MCP connection, Git commands, usage-ledger parsing, and restoration delay the first usable UI.
- Rendering is viewport-driven, but a newly visited large message is still parsed and laid out as a whole cell. Agent switching discards its focused renderer cache.

Entry points: `src/core/src/core/session.rs:331`, `src/cli/src/tui/mod.rs:467`, `src/cli/src/tui/view.rs:331`, `src/cli/src/tui/view.rs:864`, `src/cli/src/tui/exploration.rs:99`, `src/cli/src/tui/scrollback.rs:255`.

### Work

- [ ] Add end-to-end resume measurements, separating database reads, decoding, history reconstruction, replay, first frame, and input readiness.
- [ ] Replace repeated loading with one consistent restore operation: checkpoint, journal tail, working context, transcript metadata, and agent summaries.
- [ ] Restore finalized transcript state without replaying every historical streaming delta through live reducers.
- [ ] Load old tool bodies and child transcripts on demand; avoid unnecessary copies of large payloads.
- [ ] Store lightweight session previews and paginate picker results instead of loading every conversation.
- [ ] Show the conversation UI early; initialize ancillary services asynchronously with explicit readiness/error state.
- [ ] Replace full-cell scans with indexes for active drafts, retries, tool calls, and agent/workflow entries.
- [ ] Separate structural revisions from individual cell-content revisions. Ignore irrelevant events without invalidating the main view.
- [ ] Update exploration groups and visible indexes incrementally, including efficient expanded-group membership checks.
- [ ] Retain the existing viewport renderer; add block-level preparation for oversized messages and byte-budgeted caches.
- [ ] Preserve per-agent viewport/cache state and use indexed or estimated heights for large scroll jumps.

### Measurement requirements

- [ ] Cover small and large SQLite conversations, long tool output, large Markdown/code blocks, many agents, and active streaming while scrolling.
- [ ] Measure release builds on real Windows terminals: resume-to-input latency, input-to-frame latency, frame-time distribution, memory, and terminal output volume.
- [ ] Compare scaling at increasing history sizes, not just one fixture.

Existing macOS ARM64 benchmark: 5,000 mostly short cells, twelve warm viewport width toggles in 1.183 ms total in release mode; eager full-history reference took 263.177 ms. This excludes SQLite, replay, live event reduction, full application drawing, and terminal I/O. It does not reproduce the reported Windows issue or prove end-to-end scrolling is fast.

## 2. Agent-message routing — part of the transcript refactor

### Intended visibility

- Main conversation: human messages, the root's user-facing synthesis, and brief lifecycle/error notices. No raw incoming agent reports or internal wake instructions masquerading as human chat.
- Agent conversations: incoming and outgoing communication, including root → child, child → parent, and child → child messages at every supported nesting depth.
- Keep useful compact outgoing tool previews; detailed communication remains inspectable in the relevant agent threads.
- Show a concise failure in main; retain partial results and successful sibling reports in agent views.
- Keep the agents sidebar/main viewport as the interface. Do not add an input recipient badge or duplicate drawer.

### Confirmed current problems and work

- [ ] Stop discarding all `AgentMessage` events in agent transcripts; route incoming messages to the actual recipient.
- [ ] Preserve sender, recipient, message purpose, and human/internal provenance independently of provider message roles.
- [ ] Prevent failed `task`/`explore` results from exposing raw child reports in main, without hiding the failure itself.
- [ ] Replace synthetic user prompts for background completion with typed internal wake/context events.
- [ ] Keep live rendering, event replay, and persisted-transcript fallback consistent.
- [ ] Test root/child, nested parent/child, and sibling messaging in both directions, including automatic completion and partial failure.
- [ ] Distinguish routing leaks from the root model unnecessarily repeating internal reports; do not use text-pattern hiding as the primary routing mechanism.

Entry points: `src/cli/src/tui/team.rs:261`, `src/cli/src/tui/view.rs:124`, `src/cli/src/tui/mod.rs:889`.

## 3. Capability-aware prompt and context composition

Copilot's useful architectural pattern is shared behavior plus capability/profile/model-specific composition—not one universal prompt or wholesale copied text.

- [ ] Replace prebuilt system-prompt strings with one request composer used by root and child agents.
- [ ] Resolve actual tools, role, operating mode, model capabilities, and prompt sections together. Do not copy the root prompt and independently remove child tools.
- [ ] Keep mandatory trust/safety framing separate from customizable behavioral instructions.
- [ ] Represent project instructions, learned memory, task context, and current work separately from the behavioral base.
- [ ] Include optional LSP, memory, editing, delegation, and interaction guidance only when applicable.
- [ ] Ensure model/profile/tool/mode changes produce coherent fresh requests and correct context-budget accounting.
- [ ] Add model-specific adjustments only where repeated paired evaluations demonstrate a need.
- [ ] Use targeted current-turn reminders for demonstrated long-conversation failures, rather than continually enlarging the base prompt.
- [ ] Surface runtime-known file/context changes concisely where useful; retain hash-based stale-edit enforcement. Do not infer user intent or introduce a broad file watcher just for parity.

Preserve the guidance already added: early exploration, reuse of complete explorer reports, hypothesis-driven uncertain edits, implementation precedents, scoped autonomy, and permission-aware error recovery.

## 4. Unified run lifecycle, continuation, and delegation

- [ ] Move run state and budget ownership into the core instead of separate CLI, named-agent, and workflow drivers.
- [ ] Define states for running, waiting on owned work, paused for budget, completed, cancelled, and failed.
- [ ] Track run identity, initiating request, iterations, provider attempts, extensions, and finalization allowance explicitly.
- [ ] Persist typed stop reasons and continuation eligibility, not just usage and text.
- [ ] Add visible Continue/Pause handling at a soft limit, with an opportunity to revise the request.
- [ ] Define separately configured automatic extension and hard-ceiling policies. Do not equate YOLO permission bypass with autonomous continuation.
- [ ] Handle human requests, steering, child/job completion, and explicit continuation as distinct triggers without accidentally resetting budgets or replaying mutations.
- [ ] Launch task, explore, named, and workflow children through one factory and context contract.
- [ ] Give children explicit objectives, authorized scope, ownership, prior evidence, failed approaches, unresolved questions, verification responsibility, deliverables, and budgets.
- [ ] Return structured child outcomes, evidence, and verification status rather than flattening failures into prose.
- [ ] Build a bounded current-work view from existing todos/jobs/agents/events, including whether results have been collected or incorporated. Avoid a second independent task database.
- [ ] Preserve successful siblings and retry only unresolved work. Keep fresh child history by default.
- [ ] Keep file-read tracking isolated: another agent's report is not proof the current agent has read the bytes required for a guarded edit.

Do not change limits merely to match Copilot's numbers. The goal is understandable, resumable behavior with consistent accounting.

## 5. Complete scoped instruction loading

The previous pass fixed auxiliary-read failures and ordinary output truncation. It did not implement a complete instruction lifecycle or Copilot instruction-file compatibility.

- [ ] Define supported formats and precedence explicitly. Current first-nonempty selection means `AGENTS.md` suppresses `CLAUDE.md` in the same directory; decide whether aliases should instead combine.
- [ ] Support the chosen global, repository, ancestor, and nested AGENTS/CLAUDE/Bob locations consistently.
- [ ] Add `.github/copilot-instructions.md` and path-scoped `.github/instructions/*.instructions.md` with validated applicability semantics.
- [ ] Decide the scope of additional Claude locations, rules, and imports separately; do not promise complete vendor compatibility accidentally.
- [ ] Do not load arbitrary `.github` workflows, templates, or Markdown as instructions.
- [ ] Move instruction delivery out of ordinary tool-output text into bounded, deduplicated, scoped request context.
- [ ] Ensure relevant rules reach the model before it generates built-in file mutations, including new files and secondary LSP batch targets. Loading rules during a mutation is too late.
- [ ] Detect changed instruction content and require reconsideration rather than automatically replaying a mutation generated under old rules.
- [ ] Preserve source/scope provenance and canonical repository/worktree/symlink boundaries; prevent sibling-rule leakage.
- [ ] Keep valid sources usable when another source is malformed; make global and project loader failures visible and recoverable.
- [ ] Keep instruction context coherent through compaction, resume, custom profiles, and child launches, without unbounded accumulation.
- [ ] Make active instruction sources and applicability inspectable.

Do not claim comprehensive pre-mutation enforcement for arbitrary shell commands, hooks, or MCP tools whose target paths cannot be determined reliably.

## 6. Windows daily-use readiness

Native Git Bash resolution, truthful shell reporting, LSP `.exe` lookup, and additional Windows tests are implemented locally. Actual Windows execution still needs verification.

- [ ] Run Windows CI and representative real-terminal workflows: search, read, edit, build/test, cancellation, resume, scrolling, and agent switching.
- [ ] Verify child/grandchild cleanup and retained output through foreground timeout, cancellation, background jobs, and shutdown.
- [ ] Verify common dependency installation layouts and provide actionable setup/preflight errors without disabling file-only work.
- [ ] Test npm/MCP `.cmd` launch and argument preservation or safe rejection, including effective PATH/PATHEXT configuration. Do not add ad hoc `cmd.exe` string concatenation.
- [ ] Exercise spaces, Unicode, CRLF, file-sharing failures, junctions, UNC paths, long paths, drive-relative paths, and case aliases where relevant. Treat untested cases as coverage gaps, not confirmed bugs.
- [ ] If adding native PowerShell, implement matching command-language guidance, quoting, process handling, and permission analysis. Merely swapping the executable is insufficient.

## 7. Remaining recovery and session correctness

- [ ] Add controlled MCP/LSP connection replacement after server exit or permanent transport failure, so later independent requests can work without restarting Bob.
- [ ] Expose connection health and recovery status; use bounded recovery and never automatically replay a request with unknown side effects.
- [ ] Define cross-workspace `--resume ID` behavior: restore the stored workspace, reject the mismatch, or require confirmation. Do not silently mix old history with a new workspace's instructions.
- [ ] Define session matching across launch directories, canonical paths, repositories, and worktrees, including Windows path identity.
- [ ] Exercise root and manual-compaction panic handling through the real UI/session lifecycle, including persistence and safe continuation/restart boundaries.

## 8. Verification gates for the refactors

- [ ] Preserve the current behavioral baseline before replacing architecture.
- [ ] Keep meaningful tests colocated with production behavior; avoid tests whose sole purpose is checking prompt wording or removed labels.
- [ ] Add deterministic coverage for budget exhaustion/extension, cancellation, late reports, partial failure, unknown mutation outcomes, and replay equivalence.
- [ ] Extend opt-in live evaluations for capability restrictions, mode/model changes, diagnosis-only requests, failed hypotheses, implementation handoffs, and continuation.
- [ ] Use GitHub Copilot as the live model backend only; no silent fallback to direct provider billing.
- [ ] Evaluate correctness, scope compliance, grounded evidence, duplicate work, recovery, and user intervention—not just whether a preferred tool was called.
- [ ] Keep strict failures visible. In the latest live run, all eight cases passed correctness, selection, recovery, and cleanup; one exceeded the post-exploration efficiency allowance. The suite therefore failed overall at 7/8.
- [ ] Re-evaluate the subsequent hypothesis-driven and implementation-precedent prompt additions against live models.
- [ ] Distinguish unit/rendering tests, live model checks, end-to-end UI checks, performance measurements, and actual Windows results in reports.
- [ ] Run formatting, workspace build/tests, and Clippy with `-D warnings` using the declared toolchain. Do not suppress warnings to make CI green.

## Implemented baseline — retain and verify, not redo

- Rust 1.99.0 toolchain alignment and `async-trait` 0.1.92 Clippy fix.
- Explicit output-limit outcomes and visible iteration-limit summaries.
- Tool/root panic containment, preserved sibling results, and bounded cooperative cancellation.
- Committed edit results independent of optional LSP synchronization; ordering barriers for subsequent requests.
- MCP application-error flags preserved as failures.
- Structured read output, auxiliary-instruction warnings, and bounded attachment handling.
- Native Bash discovery/override and Windows LSP executable lookup.
- Early/parallel exploration, reduced repeated investigation, and actionable missing-file recovery guidance.

Last full deterministic verification: 936 non-ignored tests, workspace build, formatting, and Clippy passed locally. Later prompt-only changes passed 30 relevant tests and Clippy. Local success does not establish that Windows CI or a release has passed.

## Recommended execution order

1. Establish representative performance baselines and the message-visibility contract.
2. Replace restoration and event reduction; fix communication routing in the same transcript workstream.
3. Make grouping/cache invalidation incremental, then address large-cell and Windows-specific bottlenecks based on measurements.
4. Replace prompt/context composition across all agent roles.
5. Centralize run continuation and child handoffs, then complete managed instruction delivery.
6. Complete integration reconnection and remaining platform/session verification.

Do not add an SDK, remote-control product, or unrelated agent features as part of this work. Preserve existing conversations; aggressive refactoring is not authorization to delete user data. Commit, push, and release only when requested.
