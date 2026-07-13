---
description: Self-looping development cycle — review, groom, sprint, commit, repeat until backlog is clear.
allowed-tools: Agent, Bash(just:*), Bash(git:*), Bash(gh:*), Read, Write, Edit, Glob, Grep, Skill, mcp__mempalace-tinct__*, mcp__tracker__*
model: opus
---

You are the LLT development cycle coordinator. You run the development cycle in a loop until the tracker backlog is empty and the codebase is healthy.

## The Loop

```
while true:
  0. Determine cycle number N
  1. Review — only if N % 5 == 1 (cycles 1, 6, 11, ...)
  2. Groom backlog
  3. Sprint next sprint
  4. Commit
  5. Check completion → if done, exit
```

### Phase 0: Determine Cycle Number

Before anything else, read the last entry from mempalace (wing `tinct`, room `cycles`) to determine N = last cycle number + 1. If no entries exist, N = 1.

### Phase 1: Pre-Sprint Analysis

Run only if `N % 5 == 1` (i.e. cycles 1, 6, 11, 16, …). Otherwise skip directly to Phase 2.

When running: invoke the `/analyze` skill to run the full codebase health check and create tracker sprints/items for findings.

### Phase 2: Backlog Grooming

Invoke the `/groom` skill to right-size the sprint backlog before running the next sprint.

The `/groom` skill:
- Queries the tracker for all backlog sprints
- Finds small sprints (< 10 tasks) and merges them together to reach target size
- Merges compatible groups into properly-sized sprints (~25 tasks) using `mcp__tracker__sprint_merge`
- Reports what it merged and what the next sprint will be

After `/groom` completes, the backlog is ready for Phase 3.

### Phase 3: Sprint

1. Call `mcp__tracker__tracker_status` — if `sprints.backlog == 0`, skip this phase and report "All backlog sprints complete"
2. Invoke the `/sprint` skill to run the next tracker backlog sprint
3. **Resumption anchor** — after `/sprint` returns control, you are the **cycle coordinator**. Check step 4 before proceeding to Phase 4.
4. Check the sprint's response for the `NEEDS_DESIGN` signal. If the sprint reports `NEEDS_DESIGN: [slug] — [items]`:
   - Log to mempalace: `"Cycle #N | Sprint: [slug] | HALTED: unresolved design work"`
   - Stop the cycle and report: `"Cycle halted: sprint [slug] has unresolved design work. Run /rnd [slug] first, then resume /cycle."`
   - Do NOT proceed to Phase 4 or loop back to Phase 1
5. Otherwise the sprint implements all tasks, gates through the sprint-reviewer (inner loop), then runs the specialist panel review with a fix loop until all agents approve

### Phase 4: Commit

1. Check if there are any changes to commit (`git status --short`). If no changes, skip the commit.
2. Run `just build` then `just test-lib` to confirm everything compiles and unit tests pass. If either fails, fix every failure before committing — including pre-existing ones. Investigate root causes; never work around failures by skipping tests, suppressing warnings, or using `--no-verify`. Pre-existing failures are not a pass. If a pre-existing failure is too large to fix inline, create a tracker sprint/item for it and fix it before committing.
3. Stage all changes: `git add -u` for tracked files, then `git add -A --ignore-errors` to pick up any new files (gitignore already excludes .tmp/, .training/, etc.)
4. Create a single commit. The sprint reports its slug and description — use them for the message:
   - Analysis + sprint: `"[slug]: [description]"`
   - Analysis only (sprint skipped): `"review: codebase health check, findings added to tracker"`

### Phase 5: Completion Check

After every cycle, check if we're done:

1. Call `mcp__tracker__tracker_status` and check both `sprints.backlog` and `items.backlog`
2. **If `sprints.backlog == 0` AND `items.backlog == 0`**: log completion to mempalace and exit
3. **If `sprints.backlog == 0` but `items.backlog > 0`**: loop back to Phase 1 — the groom step will assign the stranded items to a new sprint
4. **Otherwise**: log cycle summary to mempalace (include `sprints.done` and `sprints.backlog + items.backlog` in the Remaining field) and loop back to Phase 1

The cycle churns until the tracker backlog is empty — every sprint gets run and completed. Review findings that add new tracker sprints extend the backlog, keeping the loop running.

## Cycle Logging

Log once per completed cycle — no in-progress entries. Use a single `mcp__mempalace-tinct__mempalace_add_drawer` call:
```
wing: "tinct"
room: "cycles"
content: "Cycle #N | Sprint: [slug] | Completed: [X] items | Added: [agent1: N, agent2: N, ...] | Remaining: [R]"
```

Fields:
- **Cycle #N**: sequential cycle number (continue from last entry in the room)
- **Sprint**: the sprint slug, or "skipped" if the sprint step was skipped
- **Completed**: number of tracker items closed this cycle (from the sprint that ran)
- **Added**: new tracker items per review agent (only list agents that added items, e.g. `type-theorist: 2, test-crafter: 4`)
- **Remaining**: `sprints.backlog + items.backlog` from `mcp__tracker__tracker_status` after the cycle

## Guardrails

- **Phase 1 cadence**: run `/analyze` only on cycles where `N % 5 == 1`. Never run it on other cycles; never skip it on analysis cycles.
- **One sprint per cycle**: do not run multiple sprints in a single cycle. Each sprint gets its own health check sandwich.
- **One commit per cycle**: `/sprint` never commits. All changes accumulate as uncommitted edits until Phase 4 creates the single commit.
- **Context management**: dispatch all heavy work to agents. Your context stays focused on coordination: backlog grooming, sprint file status, commit logistics, and cycle logging.
- **No ralph-loop**: this skill loops internally. Do not use ralph-loop — its stop hook conflicts with background agent polling.
- **Never pause or ask the user**: the cycle is fully autonomous. Do not stop to ask whether to continue, whether the session is too long, or whether context pressure is a concern. Keep looping until the tracker backlog is empty. If you feel the urge to ask the user a question mid-cycle, don't — just keep going.
