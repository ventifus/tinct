---
description: Review a completed whatif — sprint coverage, implementation verification, doc consistency. Ensures sprints are complete and properly scoped, implementation matches spec, feature doc is standalone, main docs are atemporal and complete.
argument-hint: <whatif-name>
allowed-tools: Agent, Read, Write, Edit, Glob, Grep, mcp__mempalace-tinct__*, mcp__tracker__*
model: opus
---

Full consistency review for a completed whatif. Run all checks, collect findings, report, then fix interactively.

## Doc Layer Model

| Layer | Location | Role | Temporal language? |
|---|---|---|---|
| Design history | `doc/whatif/` | Read-only historical artifact | Allowed |
| Deep-dive spec | `doc/feature/` | Optional standalone technical spec | No |
| Authoritative reference | `doc/*.md` | Complete, atemporal | Never |

Run Steps 1–5 in sequence collecting all findings, then present the full report in Step 6, fix interactively in Step 7, and update the tracker in Step 8.

## Step 1: Load the Whatif

1. Resolve the argument to a full path (search `completed/`, `doc/whatif/`, `abandoned/` for bare names)
2. Read the whatif in full. Extract:
   - **State** (`State:` header) — guard: Proposal → warn and invert checks; Superseded → verify successor; Accepted/Completed → proceed
   - **Supersession headers** — `**Replaces:**`, `**Superseded by:**`, `**Resolved by:** sprint-slug`
   - **Sprint slugs** — from Phased Adoption, Implementation Sprints, backtick sprint-name references
   - **Key identifiers** — type names (`Type::X`), builtins (`builtin-foo`), stdlib functions, struct fields, error codes, syntax forms, CLI flags
   - **Behavioral claims** — "when X, Y is returned", "Z is rejected with error"
   - **Target chapters** — `doc/*.md` references

## Step 2: Supersession Links

Verify all supersession headers point correctly in both directions. Record:
- `LINK-BROKEN: <path> missing back-reference`
- `LINK-WRONG: <path> points to wrong file`
- `STATE-STALE: <path> State: <value> should be Superseded`
- `LINK-MISSING: State is Superseded but no successor reference`

## Step 3: Sprint Coverage

**Find sprints** via `sprint_list` + `sprint_get`. Look for `**Whatif:** \`<name>\`` in context notes; fall back to name matching. Classify each: DONE / COMPLETE / IN PROGRESS / NOT STARTED / MISSING.

**Assess readiness** for each incomplete sprint — a sprint is ready for `/sprint` when ALL hold:
- No `decision` or `research` items in backlog
- No hedged titles (consider, optionally, might, could, possibly)
- Items reference source files (e.g. `` `src/file.rs` ``)
- At least one test task exists
- All `dependencies` are state `"done"`

Record: `SPRINT-GAP: <slug> — <gap>`, `SPRINT-MISSING: <slug>`, `SPRINT-ALIGN: <slug> — <misalignment>`

## Step 4: Implementation Verification

For each identifier from Step 1, verify in source:

| Identifier type | Search target |
|---|---|
| `Type::X`, `Expr::X`, `Value::X` | `src/*.rs` |
| Builtin `foo` | `src/builtins*.rs` string literals |
| Stdlib function | `stdlib/prelude.llt` |
| Struct field | relevant `src/*.rs` struct definitions |
| CLI flag | `src/main.rs` clap definitions |

Check for stubs: `unimplemented!()`, `todo!()`, `// TODO/DEFERRED/STUB`, `#[allow(dead_code)]` on active paths, `Type::Unknown`/`None` where specific values were promised.

Check for de-scoped features: tracker context notes with CANCELLED/removed, comments saying "was going to do X".

Spot-check 3–5 behavioral claims against source code.

Record: `PRESENT/MISSING: <identifier>`, `STUB: <file>:<line> — <desc>`, `DE-SCOPED: <loc> — <desc>`, `DIVERGENCE: <file>:<line> — expected: <X> / found: <Y>`

## Step 5: Doc Consistency

**Feature doc** (`doc/feature/<name>.md`): check for whatif references (VIOLATION), temporal language (TEMPORAL), content gaps vs whatif (GAP).

**Main doc coverage** — check relevant chapters for key identifiers:

| Feature touches... | Primary chapter |
|---|---|
| Syntax, parsing | `doc/02-syntax.md` |
| Data model, values | `doc/03-data-model.md` |
| Functions | `doc/04-functions.md` |
| Type annotations | `doc/05-type-annotations.md` |
| Type inference | `doc/06-type-inference.md` |
| BAS, row poly | `doc/07-type-extensions.md` |
| Evaluation, laziness | `doc/08-evaluation.md` |
| Documents, pipeline | `doc/09-documents.md` |
| Errors | `doc/10-errors.md` |
| Stdlib | `doc/11-stdlib.md` |
| Builtins | `doc/11a-builtins.md` |
| CLI, tooling | `doc/12-tooling.md` |
| Patterns, match | `doc/14-patterns.md` |

**Temporal language** — scan for: previously, now, as of, will be, planned, not yet, currently, for now, Phase N, see TODO.md. Record: `TEMPORAL: <file>:<line> — "<phrase>"`

→ After completing Steps 1–5, all findings are collected. Proceed to Step 6.

## Step 6: Report

```
## Whatif Review: <name>
**State:** <state>

### Supersession Links
[LINK-OK | LINK-BROKEN | LINK-WRONG | STATE-STALE | LINK-MISSING]

### Sprint Coverage
<sprint-slug> — DONE | COMPLETE | IN PROGRESS | NOT STARTED | MISSING
SPRINT-GAP: <slug> — <gap>
SPRINT-ALIGN: <slug> — <misalignment>

### Implementation
PRESENT/MISSING: <identifier>
STUB: <file>:<line> — <desc>
DE-SCOPED: <loc> — <desc>
DIVERGENCE: <file>:<line> — expected: <X> / found: <Y>

### Feature Doc
[EXISTS | MISSING] — VIOLATION/TEMPORAL/GAP entries

### Main Doc Coverage
<chapter>: <identifier> — OK | STALE | GAP
TEMPORAL: <file>:<line> — "<phrase>"

### Priority Actions
1. RESCHEDULE: <missing/diverged work needing tracker items>
2. FIX SPRINT: <readiness gaps>
3. FIX CODE: <stubs, divergences>
4. FIX DOC: <gaps, stale content, temporal language>
```

Ask: "Which would you like to address first?" Wait for user direction, then proceed to Step 7.

## Step 7: Apply Fixes (user-approved)

**Rescheduling**: for MISSING/DE-SCOPED/DIVERGENCE, propose a tracker item before creating it. Create as unassigned or assign to a related sprint.

**Sprint gaps**: add file references to items, rewrite hedged titles, add test items, remove unresolved decision items (route through `/rnd`). Show changes before applying.

**Doc fixes**: draft edits, show user, apply on approval. Priority: content gaps → stale content → temporal language → feature doc violations.

**Never**: modify `doc/whatif/` files (read-only). Never apply edits without showing the user first. Never write implementation code.

→ When all user-approved fixes are applied, proceed to Step 8.

## Step 8: Update Tracker

Create tracker items for rescheduled work. If a complete sprint is still showing backlog, call `sprint_complete`. Mark the review sprint item done when finished.

## Key Principles

- **Whatifs are read-only.** Never edit them.
- **Sprint coverage first.** Untracked work outranks undocumented work.
- **Implementation before docs.** MISSING/DIVERGENCE outranks any doc gap.
- **Reschedule, don't delete.** De-scoped features need a tracker item or a whatif note.
- **Feature docs are standalone.** No whatif references.
- **Main docs are atemporal.** Any phrase revealing when something was added is a violation.
