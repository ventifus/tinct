---
description: Self-looping development cycle — review, groom, sprint, commit, repeat until TODO is clear.
allowed-tools: Agent, Bash(just:*), Bash(git:*), Bash(gh:*), Read, Write, Edit, Glob, Grep, Skill, mcp__mempalace-tinct__*
model: opus
---

You are the LLT development cycle coordinator. You run the development cycle in a loop until all TODO items are complete and the codebase is healthy.

## The Loop

```
while true:
  0. Determine cycle number N
  1. Review — only if N % 5 == 1 (cycles 1, 6, 11, ...)
  2. Groom TODO.md
  3. Sprint next sprint
  4. Commit
  5. Check completion → if done, exit
```

### Phase 0: Determine Cycle Number

Before anything else, read the last entry from mempalace (wing `tinct`, room `cycles`) to determine N = last cycle number + 1. If no entries exist, N = 1.

### Phase 1: Pre-Sprint Analysis

Run only if `N % 5 == 1` (i.e. cycles 1, 6, 11, 16, …). Otherwise skip directly to Phase 2.

When running: invoke the `/analyze` skill to run the full codebase health check and update TODO.md with findings.

### Phase 2: TODO Grooming

After the review may have added new findings to TODO.md, groom the backlog before sprinting:

1. Read `TODO.md` in full
2. **Categorize**: ensure every item is in the correct phase. Move misplaced items
3. **Dependency order**: verify milestones and sprints are ordered so foundational work comes before dependent work
4. **Dedup**: merge duplicate or overlapping items that may have been added by different review agents
5. **Split oversized sprints**: if any unchecked sprint has more than 8 tasks, split it into smaller sprints (use new kebab-case slugs)

Only edit TODO.md — do not create other files. If no changes are needed, move on.

### Phase 3: Sprint

1. If no unchecked sprints remain in TODO.md, skip this phase and report "All TODO items complete"
2. Invoke the `/sprint` skill to run the next TODO sprint
3. **Resumption anchor** — after `/sprint` completes and returns control, you are the **cycle coordinator**. The sprint is done. Proceed to step 4, then Phase 4 (Commit).
4. Check the sprint's response for the `NEEDS_DESIGN` signal. If the sprint reports `NEEDS_DESIGN: [slug] — [items]`:
   - Log to mempalace: `"Cycle #N | Sprint: [slug] | HALTED: unresolved design work"`
   - Stop the cycle and report: `"Cycle halted: sprint [slug] has unresolved design work. Run /design [slug] first, then resume /cycle."`
   - Do NOT proceed to Phase 4 or loop back to Phase 1
5. Otherwise the sprint implements all tasks, gates through the sprint-reviewer (inner loop), then runs the specialist panel review with a fix loop until all agents approve

### Phase 4: Commit

1. Check if there are any changes to commit (`git status --short`). If no changes, skip the commit.
2. Run `just test` one final time to confirm everything is green
3. Stage all changes: `git add -u` for tracked files, then `git add -A --ignore-errors` to pick up any new files (gitignore already excludes .tmp/, .training/, etc.)
4. Create a single commit. The sprint reports its slug and description — use them for the message:
   - Analysis + sprint: `"[slug]: [description]"`
   - Analysis only (sprint skipped): `"review: update TODO with codebase health findings"`

### Phase 5: Completion Check

After every cycle, check if we're done:

1. Read `TODO.md` and count unchecked items (`- [ ]`)
2. **If zero unchecked items remain**: log completion to mempalace and exit
3. **Otherwise**: log cycle summary to mempalace and loop back to Phase 1

The cycle churns until TODO.md is empty — every unchecked item gets sprinted and completed. Review findings that add new items extend the backlog, keeping the loop running.

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
- **Completed**: number of TODO items checked off this cycle
- **Added**: new TODO items per review agent (only list agents that added items, e.g. `type-theorist: 2, test-crafter: 4`)
- **Remaining**: total unchecked items after the cycle

## Guardrails

- **Phase 1 cadence**: run `/analyze` only on cycles where `N % 5 == 1`. Never run it on other cycles; never skip it on analysis cycles.
- **One sprint per cycle**: do not run multiple sprints in a single cycle. Each sprint gets its own health check sandwich.
- **One commit per cycle**: `/sprint` never commits. All changes accumulate as uncommitted edits until Phase 4 creates the single commit.
- **Context management**: dispatch all heavy work to agents. Your context stays focused on coordination: TODO.md grooming, sprint file status, commit logistics, and cycle logging.
- **No ralph-loop**: this skill loops internally. Do not use ralph-loop — its stop hook conflicts with background agent polling.
