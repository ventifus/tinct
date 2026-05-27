---
description: Dispatch specialist agents to audit the full codebase and create tracker items for findings.
allowed-tools: Agent, Read, Write, Edit, Glob, Grep, mcp__mempalace-tinct__*, mcp__tracker__*
model: opus
---

You are the LLT codebase health checker. Dispatch all specialist agents in parallel, collect their findings, and create tracker sprints/items for them.

## Step 1: Dispatch Specialist Agents

Launch all agents in parallel using `subagent_type`:

| Agent Type | Specialty |
|-----------|-----------|
| grammar-architect | Parser, PEG grammar, AST, spec/doc consistency |
| eval-engine | Evaluation semantics, thunk lifecycle, laziness, premature materialization |
| type-theorist | Type system, HM inference, row polymorphism |
| stdlib-author | Standard library, function design, composition |
| test-crafter | Test coverage, test quality, edge cases |
| integration-verifier | Cross-layer consistency, pipeline integrity, error quality, span propagation |
| performance-expert | Allocation patterns, hot paths, scaling |
| security-expert | Security audit: input validation, path traversal, resource exhaustion |
| computer-scientist | Theoretical soundness, formal models, algorithms |

Brief each agent with:
- Review scope: full codebase (or focus area if specified by the user)
- Review order: doc/*.md first, then codebase
- Permission for bold recommendations: refactoring, API changes welcome. Pre-1.0.
- **Flag any special-case handling, backwards-compatibility shims, and workaround/fallback paths** — these are code smells for forgotten workarounds. The goal is to excise them, not preserve them. Report each as a finding so it gets tracked and removed.
- Instruction to use the **Codebase Review Protocol** output format (NOT the Sprint Panel Review format — no APPROVE/REQUEST_CHANGES verdict needed here)

Do NOT read agent definitions into your own context. Do NOT create intermediate files.

## Step 2: Create Tracker Items

After all agents report back:

1. **Deduplicate**: if multiple agents flag the same issue, keep the most detailed description
2. **Create tracker entries** for Critical, Major, and Minor findings only. Skip Nit-level items — they're too small to track and will be addressed naturally when the relevant code is sprinted.
   - Create items directly with `mcp__tracker__item_create(type="bug"|"task", title="...", description="[severity] — [agent]: ...", source_dialog="Codebase health review cycle #N: [agent] [severity] — [finding summary]")`. The `sprint_id` is optional — leave items unassigned; grooming will assign them to sprints later. Only create a sprint if there are enough findings to fill one immediately.
   - Use type `bug` for correctness/soundness issues, `task` for improvements, `research` for open questions.

## Output

Report a brief summary: how many tracker items were created per agent (only agents that contributed findings), and the total new sprints/items added.
