# Pantheon Harness Finish Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Get Pantheon's agent harness to a defensible finish state by fixing context layout, long-running handoffs, evaluation, orchestration, and security boundaries before the final release pass.

**Architecture:** Treat the harness as model plus context plus tools plus hooks plus memory plus sandbox plus interface. Keep static prefixes stable for cache reuse, push volatile state to the tail, give long runs durable task artifacts, separate the agent doing work from an evaluator judging it, and verify every change with repo tests plus harness evals.

**Tech Stack:** Rust workspace, Tauri desktop, SQLite ledger, Pantheon provider adapters, plugin hooks, `SKILL.md` skills, GalaxyMem style memory plane, Playwright MCP for UI grading, `eval/` suites.

**Spec:** Attached PDF video summaries, the citations in this plan, and the current source tree at `/home/ubuntu/projects/pantheon`.

## Global Constraints

- Repo path: `/home/ubuntu/projects/pantheon`, branch `main`, currently clean at `992b563`.
- Author on every commit: `Umar Saidu <k1ng0mar@users.noreply.github.com>`.
- Commit subjects only, lowercase, no type prefixes, no bodies.
- Keep changes isolated, commit incrementally, run relevant tests, never push unverified changes.
- Sandbox capability is unavailable on this EC2. Claims needing deployed host proof stay `needs_validation`.
- No unnecessary docs. Keep working notes outside the repo unless the plan file itself is the deliverable.
- Preserve the rebrand. No references to inspiration projects in names, comments, or docs.
- Do not weaken tests to make them pass. Tests must assert real behavior.

## Research basis

- Prompt engineering still matters, but it has moved into context engineering: choose the configuration of tokens most likely to produce the desired behavior, not just the right wording.[27][15]
- System prompts should be clear and use the minimal set of information that fully specifies behavior. More is not automatically better.[27][5]
- Caching works on prefixes. Put tools, system instructions, and other stable context first, mark the end of the reusable prefix, and let timestamps, git state, and the current message sit after the breakpoint.[14]
- Long running agents fail when they try to one shot a task or declare success after partial progress. An initializer pass plus a coding pass, a feature list, and a progress file fix both failure modes.[28]
- Context resets with a structured handoff can beat in place compaction because a reset gives the agent a clean slate without context anxiety.[29]
- Separating generator from evaluator is a strong lever. Evaluators should use explicit, calibrated criteria and, for UI work, click through the running app the way a user would.[29]
- Just in time retrieval and progressive disclosure let the agent discover only what it needs, while compaction, structured notes, and subagents handle long horizons.[27]
- Agent evals need task inputs, a real tool environment, and graders that check state and behavior, not just text. They expose regressions before users do.[26]
- Production agent primitives are versioned config, bounded environments, persistent sessions, and decoupled reasoning from execution. Credential vaults, permission interrupts, and scheduled triggers belong in the harness, not ad hoc provider code.[30]
- Guardrails, human handoff, and failure thresholds are required when agents act independently.[24]

## Current strengths to preserve

- Provider slots and overrides already match the "swappable model" idea, and app instructions already ride the cached system prompt for Claude family harnesses.[14]
- Skill discovery already exposes frontmatter first and reads full bodies through a gated tool. Keep that behavior, do not rebuild it.
- Plugin hooks already distinguish context, gate, transform, and observer classes, with gate hooks failing closed.
- `context_recovery.rs` already implements a one time handoff when a harness context is replaced. Extend it rather than adding a second mechanism.
- `architect/briefing.rs` already re-sends team rules only when changed, on fresh conversations, or on a periodic interval. Keep this as the model for deterministic re-injection.

---

### Task 1: Lock the finish gate

**Files:**
- Modify: this plan only
- Test: no code test

**Interfaces:**
- Produces: a written definition of done used by Tasks 2 through 7.

- [x] **Step 1: Write the acceptance checklist**

Create the checklist in the final release note, not as a new repo doc:

```
[ ] cargo test --workspace passes
[ ] cargo test --manifest-path desktop/src-tauri/Cargo.toml passes
[ ] cargo clippy --workspace --all-targets -- -D warnings passes
[ ] desktop npm tests pass
[ ] desktop playwright remote spec passes with known catalog failures listed
[ ] security audit doc lists every fix commit and caveat
[ ] manual TLS check recorded as done or needs_validation
[ ] two device Tailscale check recorded as done or needs_validation
```

- [x] **Step 2: Record the current baseline**

Run:

```bash
git status -sb
git rev-parse HEAD
git ls-remote origin refs/heads/main
```

Expected: clean tree, local equals remote.

- [x] **Step 3: Commit**

No commit needed for this task.

---

### Task 2: Make context layout explicit and cache safe

**Files:**
- Modify: `desktop/src-tauri/src/providers.rs:120-142`
- Modify: `desktop/src-tauri/src/providers.rs:2979-2985`
- Test: `desktop/src-tauri/src/providers/claude_stream_tests.rs`

**Interfaces:**
- Consumes: `RunConfig.instructions`, `RunConfig.resume_id`, `RunConfig.prompt`.
- Produces: `HarnessContext::static_prefix()` and `HarnessContext::dynamic_suffix()`.

- [x] **Step 1: Write the failing test**

In `providers/claude_stream_tests.rs`, add a test that asserts:

```rust
#[test]
fn volatile_context_never_enters_the_static_system_prefix() {
    let static_prefix = HarnessContext::static_prefix();
    assert!(!static_prefix.contains("git HEAD"));
    assert!(!static_prefix.contains("2026-"));
    let suffix = HarnessContext::dynamic_suffix("992b563", "2026-11-18");
    assert!(suffix.contains("git HEAD"));
    assert!(suffix.contains("2026-11-18"));
}
```

- [x] **Step 2: Run the test**

Run:

```bash
cargo test --manifest-path desktop/src-tauri/Cargo.toml volatile_context_never_enters_the_static_system_prefix
```

Expected: FAIL because `HarnessContext` does not exist.

- [x] **Step 3: Add the context struct**

In `providers.rs`, add:

```rust
#[derive(Clone, Debug, Default)]
pub struct HarnessContext {
    pub instructions: String,
    pub skills: Vec<String>,
    pub git_head: String,
    pub today: String,
    pub todos: Vec<String>,
}

impl HarnessContext {
    pub fn static_prefix(&self) -> String {
        let mut out = self.instructions.clone();
        for name in &self.skills {
            out.push_str("\nskill: ");
            out.push_str(name);
        }
        out
    }

    pub fn dynamic_suffix(&self) -> String {
        format!(
            "runtime context: git HEAD {}, date {}, todos {:?}",
            self.git_head, self.today, self.todos
        )
    }
}
```

- [x] **Step 4: Wire the provider command**

Keep `--append-system-prompt` bound to `HarnessContext::static_prefix()`. Append `dynamic_suffix()` to the first user turn only when `resume_id` is none. Do not append it to every message.

- [x] **Step 5: Run the tests**

Run:

```bash
cargo test --manifest-path desktop/src-tauri/Cargo.toml providers::claude_stream_tests
```

Expected: PASS.

- [x] **Step 6: Commit**

```bash
git add desktop/src-tauri/src/providers.rs desktop/src-tauri/src/providers/claude_stream_tests.rs
git commit -m "keep dynamic harness context out of cached prefix"
```

---

### Task 3: Add initializer and progress artifacts for long runs

**Files:**
- Modify: `crates/pantheon-runtime/src/orchestration_group.rs`
- Modify: `crates/pantheon-runtime/src/session.rs`
- Test: `crates/pantheon-runtime/tests/harness_contract.rs`

**Interfaces:**
- Consumes: initial user task text.
- Produces: `FeatureList` JSON and `progress.txt`.

- [x] **Step 1: Write the failing test**

Create `crates/pantheon-runtime/tests/harness_contract.rs` with:

```rust
#[test]
fn initializer_writes_feature_list_and_progress_file() {
    let dir = tempfile::tempdir().unwrap();
    let list = initialize_harness_artifacts(dir.path(), "build a small chat app").unwrap();
    assert!(list.features.len() >= 1);
    assert!(dir.path().join("feature_list.json").exists());
    assert!(dir.path().join("progress.txt").exists());
    assert!(list.features.iter().all(|f| !f.passes));
}
```

- [x] **Step 2: Run the test**

Run:

```bash
cargo test -p pantheon-runtime --test harness_contract
```

Expected: FAIL because `initialize_harness_artifacts` does not exist.

- [x] **Step 3: Implement the artifacts**

In `orchestration_group.rs`, add:

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Feature {
    pub description: String,
    pub passes: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FeatureList {
    pub features: Vec<Feature>,
}

pub fn initialize_harness_artifacts(
    root: &std::path::Path,
    task: &str,
) -> Result<FeatureList, String> {
    let list = FeatureList {
        features: vec![Feature {
            description: task.trim().to_string(),
            passes: false,
        }],
    };
    std::fs::write(
        root.join("feature_list.json"),
        serde_json::to_string_pretty(&list).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    std::fs::write(root.join("progress.txt"), "session start\n").map_err(|e| e.to_string())?;
    Ok(list)
}
```

- [x] **Step 4: Run the test**

Run:

```bash
cargo test -p pantheon-runtime --test harness_contract
```

Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add crates/pantheon-runtime/src/orchestration_group.rs crates/pantheon-runtime/tests/harness_contract.rs
git commit -m "add initializer harness artifacts"
```

---

### Task 4: Add structured reset and evaluator loop

**Files:**
- Modify: `desktop/src-tauri/src/context_recovery.rs`
- Modify: `desktop/src-tauri/src/context_recovery/tests.rs`
- Modify: `crates/pantheon-runtime/src/orchestration_group.rs`

**Interfaces:**
- Consumes: `FeatureList`, `progress.txt`, git HEAD, open approvals.
- Produces: `HarnessHandoff` consumed by the next fresh context.

- [x] **Step 1: Write the failing test**

In `context_recovery/tests.rs`, add:

```rust
#[test]
fn handoff_carries_task_state_not_only_transcript() {
    let handoff = build_handoff(
        "r1",
        "992b563",
        vec!["login flow".into()],
        vec!["oauth callback".into()],
        vec!["approve MCP write".into()],
    );
    assert!(handoff.contains("done: login flow"));
    assert!(handoff.contains("pending: oauth callback"));
    assert!(handoff.contains("approval: approve MCP write"));
    assert!(handoff.contains("992b563"));
}
```

- [x] **Step 2: Run the test**

Run:

```bash
cargo test --manifest-path desktop/src-tauri/Cargo.toml handoff_carries_task_state_not_only_transcript
```

Expected: FAIL because `build_handoff` does not exist.

- [x] **Step 3: Implement `build_handoff`**

In `context_recovery.rs`, add:

```rust
pub(crate) fn build_handoff(
    run_id: &str,
    git_head: &str,
    done: Vec<String>,
    pending: Vec<String>,
    approvals: Vec<String>,
) -> String {
    let mut out = format!("harness handoff for run {run_id}\ngit HEAD {git_head}\n");
    for item in done {
        out.push_str("done: ");
        out.push_str(&item);
        out.push('\n');
    }
    for item in pending {
        out.push_str("pending: ");
        out.push_str(&item);
        out.push('\n');
    }
    for item in approvals {
        out.push_str("approval: ");
        out.push_str(&item);
        out.push('\n');
    }
    out
}
```

- [x] **Step 4: Add the evaluator gate**

In `orchestration_group.rs`, add a `JudgeRequest` that carries `rubric: Vec<String>` and `evidence: Vec<String>`. The judge must return a score plus failing criteria. Do not let the generator grade its own sprint.

- [x] **Step 5: Run the tests**

Run:

```bash
cargo test -p pantheon-runtime
cargo test --manifest-path desktop/src-tauri/Cargo.toml context_recovery
```

Expected: PASS.

- [x] **Step 6: Commit**

```bash
git add desktop/src-tauri/src/context_recovery.rs desktop/src-tauri/src/context_recovery/tests.rs crates/pantheon-runtime/src/orchestration_group.rs
git commit -m "carry task state across harness resets"
```

---

### Task 5: Tighten orchestration and plugin lifecycle

**Files:**
- Modify: `crates/pantheon-runtime/src/orchestration_group.rs`
- Modify: `crates/pantheon-extensions/src/plugins.rs`
- Test: `crates/pantheon-extensions/src/plugins.rs` test module

**Interfaces:**
- Consumes: existing `delegate_budget`, `Hook`, `PythonPlugin`.
- Produces: `unload_plugin` that reverses registrations.

- [x] **Step 1: Write the failing test**

In `plugins.rs`, add:

```rust
#[test]
fn plugin_unload_removes_its_tools_and_hook_registrations() {
    let mut registry = test_registry_with_plugin("sample");
    assert!(registry.provides(Hook::PreLlmCall));
    registry.unload_plugin("sample").unwrap();
    assert!(!registry.provides(Hook::PreLlmCall));
    assert!(!registry.has_tool("sample_tool"));
}
```

- [x] **Step 2: Run the test**

Run:

```bash
cargo test -p pantheon-extensions plugin_unload_removes_its_tools_and_hook_registrations
```

Expected: FAIL because `unload_plugin` does not exist.

- [x] **Step 3: Implement tracked registration**

Record every tool and hook a plugin registers in a `PluginRegistration { tools: Vec<String>, hooks: Vec<Hook> }`. On unload, remove exactly those entries. Fail closed if a registration cannot be reversed and return an error instead of leaving inert surface behind.

- [x] **Step 4: Add router budget test**

Extend `delegate_budget` tests so a cheap router path cannot exceed `max_delegations` when 20 workers start at once. Use the existing atomic guard, do not add a second counter.

- [x] **Step 5: Run the tests**

Run:

```bash
cargo test -p pantheon-extensions
cargo test -p pantheon-runtime
```

Expected: PASS.

- [x] **Step 6: Commit**

```bash
git add crates/pantheon-extensions/src/plugins.rs crates/pantheon-runtime/src/orchestration_group.rs
git commit -m "revert plugin registrations on unload"
```

---

### Task 6: Build harness evals and cap reflection

**Files:**
- Modify: `eval/`
- Modify: `crates/pantheon-runtime/src/orchestration_group.rs`
- Test: `eval/tests/`

**Interfaces:**
- Consumes: `JudgeRequest`, feature list, run transcript.
- Produces: `EvalCase`, `EvalReport`.

- [x] **Step 1: Write the eval case struct**

In `eval/src/lib.rs`, add:

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EvalCase {
    pub name: String,
    pub prompt: String,
    pub rubric: Vec<String>,
    pub expected_files: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EvalReport {
    pub case: String,
    pub score: f64,
    pub failure: Option<String>,
}
```

- [x] **Step 2: Add three cases**

Create cases for:

1. a single turn tool call that must edit one file;
2. a multi turn task with one context reset and a handoff;
3. a UI task graded by Playwright that checks a visible button and API state.

- [x] **Step 3: Add grader commands**

Each case runs the harness in a temp workspace, then checks:

```bash
cargo test --manifest-path <temp>/Cargo.toml
test -f <expected file>
```

For the UI case, run the existing Playwright remote spec and assert the button click succeeds.

- [x] **Step 4: Cap reflection**

Add `MAX_JUDGE_ROUNDS = 3` and `MAX_BEFORE_DONE_CHECKS = 5` in `orchestration_group.rs`. Return a structured failure instead of looping when either cap is hit. This prevents harness bloat and reward hacking.[29]

- [x] **Step 5: Run the eval suite**

Run:

```bash
cargo test -p pantheon-eval
```

Expected: PASS.

- [x] **Step 6: Commit**

```bash
git add eval crates/pantheon-runtime/src/orchestration_group.rs
git commit -m "add harness eval cases and loop caps"
```

---

### Task 7: Close security boundaries and run final verification

**Files:**
- Modify: `crates/pantheon-exec/src/sandbox/runner.rs`
- Modify: `crates/pantheon-extensions/src/plugins.rs`
- Modify: `.github/workflows/ci.yml`
- Verify: `SECURITY-AUDIT.md` outside the repo

**Interfaces:**
- Consumes: existing sandbox, plugin tree hash, approval kind gate, trusted host gate.
- Produces: signed off finish state.

- [x] **Step 1: Add indirect write test**

In `sandbox/runner.rs` tests, add a case where a shell command writes outside the workspace through `tee` or redirection. Assert denial.

- [x] **Step 2: Keep read only rows rejected**

Re-run the fee API admin test that asserts read only rows are rejected. Do not relax it.

- [x] **Step 3: Re-run the five fixed leads**

Confirm these still pass: CI root test gate, approval kind separation, bind all trusted hosts, plugin tree hash, worker write scope.

- [x] **Step 4: Full verification**

Run:

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --manifest-path desktop/src-tauri/Cargo.toml
npm test --prefix desktop
npx playwright test --config desktop/playwright.config.ts --workers=1
```

Expected: green. Catalog failures from the remote spec must match the known baseline.

- [x] **Step 5: Update audit note**

Append the final verification results and keep every runtime claim that was not actually exercised marked `needs_validation`.

- [x] **Step 6: Commit and push**

```bash
git add -A
git commit -m "finish harness verification"
git push origin main
```

Expected: clean tree, local equals remote.

## Self review

- Spec coverage: prompt engineering, context engineering, long runs, resets, evaluator separation, orchestration, evals, caps, and security are all covered. The UI grading case uses the existing Playwright remote spec instead of a new duplicate case. The router budget is covered by the existing concurrent delegate test with 200 racing attempts.
- Placeholder scan: no TBD, TODO, or "implement later" remains.
- Type consistency: `FeatureList`, `HarnessContext`, `JudgeRequest`, `EvalCase`, and `EvalReport` keep the same names across tasks.
- Risk: Tasks 2 through 6 change behavior. Each has a failing test first and a narrow commit. Task 7 is the only release gate.

## Sources

[5] https://ai.google.dev/gemini-api/docs/prompting-strategies
[14] https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching
[15] https://docs.anthropic.com/en/docs/build-with-claude/prompt-engineering/overview
[24] https://openai.com/business/guides-and-resources/a-practical-guide-to-building-ai-agents
[26] https://www.anthropic.com/research/demystifying-evals-for-ai-agents
[27] https://www.anthropic.com/research/effective-context-engineering-for-ai-agents
[28] https://www.anthropic.com/research/effective-harnesses-for-long-running-agents
[29] https://www.anthropic.com/research/harness-design-long-running-apps
[30] https://www.anthropic.com/research/managed-agents
