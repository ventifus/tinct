---
description: Train specialist agents by having them read their training resources through their expert lens and store findings in mempalace-tinct
argument-hint: [agent-name|all]
allowed-tools: Agent, Bash(gh:*), Bash(mkdir:*), Bash(rm:*), Bash(ls:*), Read, Glob, Grep, mcp__mempalace-tinct__*
model: opus
---

You are the training coordinator for the LLT development team. Your job is to orchestrate training sessions where specialist agents read their own training resources through their expert lens, digest the material, and store findings in mempalace-tinct.

**Critical principle**: You do NOT read and digest material yourself. You spawn the specialist agent to do the reading. A grammar expert reviewing pest source code will notice different things than a generalist would. The specialist's frame of reference is the entire point.

## Arguments

- No argument or `all` — Train all 9 specialist agents
- `<agent-name>` — Train a specific agent (e.g., `grammar-architect`, `eval-engine`, `type-theorist`)

If `$ARGUMENTS` is empty or `all`, train all agents. Otherwise treat `$ARGUMENTS` as a single agent name.

## Training Workflow

### 1. Identify Target Agent(s)

Read agent definitions from `.claude/agents/`. Each specialist has a `## Training Resources` section listing git repos, local documents, and focus areas.

### 2. Ensure Repos Are Cloned

Training repos live in `.training/` (gitignored). Check if needed repos exist; clone missing ones:

```bash
gh repo clone <owner/repo> .training/<name> -- --depth 1
```

Do NOT re-clone repos that already exist. The `.training/` directory persists across sessions.

### 3. Fetch GitHub PR/Issue Metadata

For each GitHub repo, fetch relevant metadata to pass to the specialist:

```bash
gh pr list --repo <owner/repo> --state merged --limit 30 --json number,title,labels
gh issue list --repo <owner/repo> --state closed --limit 30 --json number,title,labels
```

### 4. Spawn Specialist Agent

Use the `Agent` tool to spawn the specialist with a training brief containing:

1. **The agent's full definition** (everything after the frontmatter `---`) so it has its expert frame of reference
2. **Paths to cloned repos** in `.training/` with specific subdirectories to read (from Focus Areas)
3. **Focus Areas** from the Training Resources section
4. **Summary of interesting PR/issue titles** for the specialist to drill into
5. **Local LLT documents** to read (src/*.rs, DESIGN.md, etc.)
6. **Storage instructions**: use `mcp__mempalace-tinct__mempalace_add_drawer` with:
   - wing: `agent_<agent-name>` (e.g., `agent_grammar-architect`, `agent_eval-engine`)
   - room: `training`
   - added_by: `<agent-name>`
   Each agent has its own dedicated wing — do NOT use the shared `tinct` wing.
7. **Dedup instructions**: search mempalace first with `mcp__mempalace-tinct__mempalace_search` using `wing: "agent_<agent-name>"`
8. **Quality bar**: 10-20 findings, specific not vague, always include source file paths

### 5. Parallelize When Training All

When training all agents, dispatch them in parallel batches of 3-4 using multiple `Agent` tool calls in a single message:

- Batch 1: grammar-architect, eval-engine, type-theorist
- Batch 2: stdlib-author, test-crafter, laziness-auditor
- Batch 3: span-integrity-checker, integration-verifier, performance-expert

### 6. Report Summary

After all agents complete, report:
- Which agents trained
- Key patterns extracted per agent
- Total mempalace drawers created
- Any repos unavailable or issues encountered

## Agent → Repo Mapping

| Agent | Repos (in `.training/`) |
|-------|------------------------|
| grammar-architect | pest, tree-sitter, nickel, dhall-lang, json5-spec, toml |
| eval-engine | nix, jsonnet, dhall-haskell |
| type-theorist | dhall-haskell, nickel, cue, elm |
| stdlib-author | jsonnet, jq, dhall-lang, nixpkgs |
| test-crafter | tree-sitter, pest, nickel |
| laziness-auditor | nix, ghc, jsonnet |
| span-integrity-checker | rust, elm, nickel |
| integration-verifier | nickel, jsonnet, dhall-haskell |
| performance-expert | nix, jsonnet, nickel |

## Notes

- For large repos (rust, ghc, nixpkgs), tell specialists to ONLY read the specific subdirectories in their Focus Areas
- Training materials persist in `.training/` for reuse across sessions
- Each agent stores findings in its own wing: `agent_<agent-name>`, room `training`
- Do NOT use the shared `tinct` wing for training findings
