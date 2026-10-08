# SkillSpector integration plan for Pantheon

SkillSpector is NVIDIA's Apache-2.0 scanner for agent skills. It answers
one question: is this skill safe to install. The static stage covers 71
patterns across 17 categories with regex, YARA, and OSV.dev CVE lookup.
The LLM stage adds behavioral AST, taint tracking, and MCP tool
poisoning, and needs a provider key.

Pantheon's seam is `crates/pantheon-exec/src/skills.rs`. Every import
lands in `<data_dir>/skills/<name>/` through six functions. Scan there,
record the verdict on disk, quarantine the executables, and surface it in
`skills doctor`.

## The 71 patterns

The README table lists 73 rule ids, of which 71 are active detection
rules. The count is off by the supply chain row, which says 10+ but the
table omits SC7: a 17th category, SSRF (SSRF1-SSRF3), exists in the
source (`static_patterns_ssrf.py`) but is not in the README's summary
list. The full table is authoritative.

| Category | IDs | Count |
| --- | --- | --- |
| Prompt injection | P1-P5, P9 | 6 |
| Anti-refusal | AR1-AR3 | 3 |
| Data exfiltration | E1-E4 | 4 |
| Privilege escalation | PE1-PE3 | 3 |
| Supply chain | SC1-SC6, SC8-SC10 | 9 |
| Excessive agency | EA1-EA5 | 5 |
| Output handling | OH1-OH3 | 3 |
| System prompt leakage | P6-P8 | 3 |
| Memory poisoning | MP1-MP3 | 3 |
| Tool misuse | TM1-TM4 | 4 |
| Rogue agent | RA1-RA2 | 2 |
| Trigger abuse | TR1-TR3 | 3 |
| Behavioral AST | AST1-AST9 | 9 |
| Taint tracking | TT1-TT5 | 5 |
| YARA signatures | YR1-YR4 | 4 |
| MCP least privilege | LP1-LP4 | 4 |
| MCP tool poisoning | TP1-TP4 | 4 |

The rule ids the static analyzers emit today:

- `static_patterns_prompt_injection.py`: P1 instruction override,
  P2 hidden instructions (HTML comments, markdown comments, zero-width
  chars, bidi controls, base64 data URIs), P3 exfiltration commands,
  P4 behavior manipulation, P9 whitespace padding.
- `static_patterns_harmful_content.py`: P5 harmful content.
- `static_patterns_data_exfiltration.py`: E1-E5.
- `static_patterns_privilege_escalation.py`: PE1-PE5.
- `static_patterns_supply_chain.py`: SC1-SC10, with SC4 querying
  OSV.dev live and falling back to a bundled list when offline.
- `static_patterns_excessive_agency.py`: EA1-EA5.
- `static_patterns_output_handling.py`: OH1-OH3.
- `static_patterns_system_prompt_leakage.py`: P6-P8.
- `static_patterns_memory_poisoning.py`: MP1-MP3.
- `static_patterns_tool_misuse.py`: TM1-TM4.
- `static_patterns_rogue_agent.py`: RA1-RA2.
- `static_patterns_anti_refusal.py`: AR1-AR3.
- `static_yara.py`: YR1-YR4, four bundled rule sets (malware, webshells,
  cryptominers, hacktools) in `src/skillspector/yara_rules/`.
- `mcp_least_privilege.py`: LP1-LP4.
- `mcp_tool_poisoning.py`: TP1-TP4.
- `static_patterns_ssrf.py`: SSRF1-SSRF3 (missing from the README table).

The LLM analyzers add the higher-value semantic checks: `behavioral_ast`
(known as AST1-AST9 in the README), `behavioral_taint_tracking`
(TT1-TT5), plus intent/quality/security discovery. Those need a provider
key, so the import gate skips them and runs `--no-llm`.

## MCP interface

The server is one FastMCP tool, `scan_skill`, in
`src/skillspector/mcp_server.py`.

```python
async def scan_skill(
    target: str,
    use_llm: bool = True,
    output_format: str = "json",
) -> dict
```

`target` accepts a Git URL, a file URL, a zip, a `.md` file, or a local
directory. The tool returns `risk_score` (0-100), `severity`,
`recommendation`, `safe_to_install`, the finding list, the rendered
report, and honest LLM accounting (`llm_requested`, `llm_available`,
`llm_used`, `scan_mode`).

`safe_to_install` is fail-closed:

```python
safe_to_install = (
    risk_score <= RISK_THRESHOLD          # 50, constants.py
    and execution_successful
    and entirely_uninspected == 0
    and partially_inspected == 0
    and analysis_requirement_met
)
```

Partial coverage fails the gate. A missing referenced artifact
(`REFERENCE_MISSING`, a path the skill writes at runtime) is the one
exception that does not fail the gate, because every bundled byte was
still inspected. An ambiguous reference (`REFERENCE_UNRESOLVED`, matching
more than one bundled artifact) fails it.

The transport model is strict. `skillspector mcp` starts the server.
stdio is for local agents and allows local paths. HTTP binds to
loopback only (`127.0.0.1` or `::1`), rejects wildcard or routable
binds, and the server refuses local filesystem targets on the HTTP
transport. A remote caller can only scan a Git URL or a zip URL. No
auth, so anything that can reach the port can call the tool.

Pantheon already speaks MCP as a client in `crates/pantheon-mcp/`.
Registering SkillSpector as an MCP server is possible, but Pantheon's
MCP gate is approval-store based: it fingerprints the server binary by
name + version + content hash before any tool call, and a version bump
or a content change lapses the consent. That is the right posture for a
code-bearing server, and SkillSpector's binary does change. Two paths:

1. Ship SkillSpector as a Pantheon bundled MCP recipe
   (`crates/pantheon-mcp/src/bundled.rs`), so the operator's enabled
   flag is the only gate, not the consent store. The scan tool becomes
   a model-callable tool on the registry.
2. Skip MCP entirely for the import gate. The import path is a headless
   CLI op with no model in the loop, and the Rust `Command` spawn of
   the CLI binary is cheaper and has no approval-store churn. The MCP
   server stays available as a model-facing tool: Pantheon can
   register `scan_skill` through `pantheon-mcp` so the agent can
   re-scan an already-imported skill on demand.

Recommended: path 2 for the gate, path 1 as a second commit for the
model-facing tool.

## CLI interface

Three commands under the `skillspector` entry point
(`pyproject.toml: skillspector = skillspector.cli:app`).

```
skillspector scan <TARGET> [flags]
skillspector mcp [--transport stdio|http] [--host 127.0.0.1] [--port 8000]
skillspector baseline <TARGET> [-o BASELINE] [--no-llm] [--reason TEXT]
```

`scan` flags relevant to a gate:

- `--format json|terminal|markdown|sarif` (default terminal)
- `--no-llm` for the static stage only
- `--output FILE` to write the report to a file
- `--exclude GLOB` for explicit caller exclusions
- `--fail-on-incomplete` exits 1 when analysis is partial
- `--fail-on-findings` exits 1 on any active finding
- `--min-coverage PCT` exits 1 when canonical coverage is below PCT
- `--baseline FILE` suppresses baseline-matched findings
- `--use-shipped-baseline` applies the skill author's own baseline
  (off by default, on purpose)

Exit codes: 0 means the scan completed and the score is 50 or under
and no strict gate fired. 1 means a gate fired (score over 50, a
finding under `--fail-on-findings`, incomplete coverage, or a
`--min-coverage` miss). 2 means the scan itself failed.

## How Pantheon gates installs

Pantheon's skills are data. Discovery parses `SKILL.md` and registers
`skills_list` and `skill_read` as read-only tools. The executable
surface is `skill_exec`, the tool that runs a skill's declared
`exec:` entries. The gate has two layers:

1. Import-time scan. Before any import writes to disk, or immediately
   after it writes to a staging dir and moves on success, run the static
   scan. The scan reads the skill's bytes and only reports. It does not
   execute them.
2. Execution-time quarantine. A `CAUTION` verdict quarantines the
   skill's executables: `skill_exec` refuses to run them until the
   operator clears the quarantine. The skill body stays readable,
   because reading is low-risk. The quarantine is the middle state
   between allowed and removed.

The verdict is policy, not a raw score. SkillSpector's README gives a
three-way mapping: `SAFE` allow, `CAUTION` prompt or warn,
`DO_NOT_INSTALL` block. Pantheon's install gate uses the same mapping
with one tightening. `CAUTION` does not just warn. It imports the
skill, records the verdict, and quarantines the executables. A CAUTION
skill whose only executable is a harmless reader still runs, because
`skill_exec` re-checks the verdict and the quarantine flag on every
call.

## Step-by-step plan

### Step 1. Add the scanner module

New file `crates/pantheon-exec/src/skills_scan.rs`. One module, three
items:

- `scan_skill_dir(dir) -> Result<ScanVerdict, PantheonError>`
- `record_verdict(dir, outcome)` writes
  `<skill_dir>/.skillspector.json`
- `GateOutcome` enum: `Blocked`, `Quarantined`, `Allowed`,
  `Unavailable`

The module spawns `skillspector scan <dir> --no-llm --format json
--fail-on-incomplete`. The binary path comes from
`PANTHEON_SKILLSPECTOR_BIN`, falling back to the PATH lookup. When the
binary is missing, `scan_skill_dir` returns `SKILL_SCAN_BIN` and the
caller decides fail-open or fail-closed from
`PANTHEON_SKILL_SCAN_UNAVAILABLE` (default open).

```rust
pub fn scan_skill_dir(dir: &Path) -> Result<ScanVerdict, PantheonError> {
    let bin = skillspector_bin()?;
    let out = Command::new(&bin)
        .args([
            "scan",
            dir.to_str().ok_or_else(|| serr("SKILL_SCAN_DIR", "skill dir is not UTF-8"))?,
            "--no-llm",
            "--format",
            "json",
            "--fail-on-incomplete",
        ])
        .output()
        .map_err(|e| serr("SKILL_SCAN_SPAWN", e.to_string()))?;
    if out.status.code() == Some(2) {
        return Err(serr("SKILL_SCAN_FAILED", "skillspector exit 2 (scan error)"));
    }
    parse_verdict(&out.stdout, dir)
}
```

`--fail-on-incomplete` is the load-bearing flag: a partial scan exits 1
with a report that names the uninspected files, and the gate treats a
partial verdict as a block, not an allow.

### Step 2. Wire the import gate

In `crates/pantheon-exec/src/skills.rs`, add one call at the end of
each import path, after the bundle is on disk:

```rust
// import_skill_from_url, after the write
let outcome = gate_imported_skill(data_dir, &name)?;

// import_skill_from_clawhub, after write_bundle
let outcome = gate_imported_skill(data_dir, &name)?;

// import_skill_from_hermes, after write_bundle
let outcome = gate_imported_skill(data_dir, &name)?;

// import_skill, after the write
let outcome = gate_imported_skill(data_dir, &skill.meta.name)?;

// import_skill_dir, after copy_tree
let outcome = gate_imported_skill(data_dir, name)?;

// import_skill_dirs, per skill
let outcome = gate_imported_skill(data_dir, &name)?;
```

`gate_imported_skill` is the single policy point:

```rust
pub fn gate_imported_skill(data_dir: &Path, name: &str) -> Result<GateOutcome, PantheonError> {
    let skill_dir = data_dir.join("skills").join(name);
    let verdict = match scan_skill_dir(&skill_dir) {
        Ok(v) => v,
        Err(e) => {
            let block = std::env::var("PANTHEON_SKILL_SCAN_UNAVAILABLE")
                .ok()
                .map(|v| v.eq_ignore_ascii_case("block"))
                .unwrap_or(false);
            if block {
                let _ = std::fs::remove_dir_all(&skill_dir);
                return Ok(GateOutcome::Blocked(e.code));
            }
            return Ok(GateOutcome::Unavailable(e.code));
        }
    };
    let rec = verdict.recommendation.to_uppercase();
    match rec.as_str() {
        "DO_NOT_INSTALL" => {
            let _ = std::fs::remove_dir_all(&skill_dir);
            record_verdict(&skill_dir, &verdict, "blocked");
            Ok(GateOutcome::Blocked(rec))
        }
        "CAUTION" => {
            set_skill_quarantined(data_dir, name, true)?;
            record_verdict(&skill_dir, &verdict, "quarantined");
            Ok(GateOutcome::Quarantined(rec))
        }
        _ => {
            record_verdict(&skill_dir, &verdict, "allowed");
            Ok(GateOutcome::Allowed(rec))
        }
    }
}
```

A `DO_NOT_INSTALL` dir is removed before `record_verdict` writes, so the
verdict file lands in the parent skills dir under a different name
(`<data_dir>/skills/.quarantine/<name>.skillspector.json`). The quarantine
dir is the durable record for a removed skill, because the removed dir
has nowhere to hold its own verdict.

### Step 3. Quarantine `skill_exec`

In `crates/pantheon-exec/src/skill_exec.rs`, read the quarantine list
on the same seam that reads `disabled_skill_names`. The skill is
quarantined when its name is in `<data_dir>/skills/quarantined.json`
and its verdict is `CAUTION`.

```rust
// skill_exec.rs, at the top of the exec path, before resolve_skill_exec_target
if quarantined_skill_names(data_dir).contains(&skill.meta.name) {
    return Err(serr(
        "SKILL_QUARANTINED",
        format!(
            "skill '{}' is quarantined by a SkillSpector CAUTION verdict; \
             clear it with `pantheon skills clear-quarantine {}`",
            skill.meta.name, skill.meta.name
        ),
    ));
}
```

`quarantined.json` and `set_skill_quarantined` live next to
`disabled.json` in `skills.rs`, with the same shape: read-tolerant,
atomic write.

### Step 4. CLI verbs

In `crates/pantheon-tui/src/skills.rs`, two new commands:

```rust
/// `pantheon skills scan [name]` re-runs the scanner on one skill or
/// every loaded skill and prints the verdict table.
pub fn cmd_skills_scan(args: &[String]) { ... }

/// `pantheon skills clear-quarantine <name>` is the operator override.
/// It writes a `SkillQuarantineCleared` ledger event with a required
/// reason, then clears the quarantine flag. The override is explainable
/// in `explain`, which is the whole point.
pub fn cmd_skills_clear_quarantine(args: &[String]) { ... }
```

Dispatch in the `"skills" =>` arm of `crates/pantheon-tui/src/terminal.rs`:

```rust
match args[2].as_str() {
    "list" => crate::skills::cmd_skills_list(&args),
    "import" => crate::skills::cmd_skills_import(&args[2..]),
    "doctor" => crate::skills::cmd_skills_doctor(&args),
    "scan" => crate::skills::cmd_skills_scan(&args[2..]),
    "clear-quarantine" => crate::skills::cmd_skills_clear_quarantine(&args[2..]),
    _ => {
        eprintln!("usage: pantheon skills <list|import|doctor|scan|clear-quarantine> ...");
        std::process::exit(2);
    }
}
```

Update `usage()` at line 363 to the new subverb list.

### Step 5. Doctor

`cmd_skills_doctor` already walks `scan_skills_ext`. For each skill,
read its `.skillspector.json` verdict and print one row.

```
ok        pdf-skill (bundled)   skillspector 0/100 SAFE
quarantined pdf-tools (hermes)  skillspector 42/100 CAUTION (E2, SC3)
unscanned legacy-skill (ext)    no skillspector verdict
```

`doctor` stays diagnostic. A CAUTION verdict is printed, not a failure.
Exit 1 is still reserved for a broken or shadowed `SKILL.md`.

### Step 6. Install the scanner

Pantheon does not vendor the scanner. `install.sh` and
`pantheon setup` offer two install paths:

```bash
# uv tool (Linux/macOS, no Docker)
command -v uv >/dev/null && uv tool install skillspector

# Docker image (headless hosts)
command -v docker >/dev/null && docker build -t skillspector:pantheon /path/to/SkillSpector
# then PANTHEON_SKILLSPECTOR_BIN=<path> or the docker-run wrapper
```

The static stage needs no LLM key. SC4 reaches OSV.dev for CVE data
and degrades to a bundled list when offline.

### Step 7. Tests

- Fixture with a P1 HTML comment and a `curl | sh` line: assert
  `recommendation == "DO_NOT_INSTALL"`, the dir is removed, and the
  import returns `SKILL_SCAN_BLOCKED` with the top rule ids.
- CAUTION fixture (one MEDIUM finding): assert the dir is kept, the
  name lands in `quarantined.json`, and a `skill_exec` call returns
  `SKILL_QUARANTINED`.
- `PANTHEON_SKILLSPECTOR_BIN=/nonexistent` with
  `PANTHEON_SKILL_SCAN_UNAVAILABLE=block`: import fails.
- `clear-quarantine` without a reason: exit 2. With a reason: the
  ledger carries the event.
- `skills scan` prints the verdict table; exit 0 on SAFE, exit 1 on a
  DO_NOT_INSTALL re-scan.

## What is not in scope

- The LLM semantic stage. It needs a provider key and a running
  provider, and the import gate is sync and headless. A later commit
  can add a `skills scan --llm` that shells out to
  `skillspector scan --format json` with a configured provider, for
  the operator who wants a second opinion before clearing a quarantine.
- Bundling the scanner into the binary. SkillSpector is a large
  Python package (LangGraph, semgrep, YARA). It is a host dependency,
  not a vendored one.
- The MCP server as a Pantheon bundled recipe. That is a second
  commit, for the model-facing `scan_skill` tool, with the bundled
  recipe's enabled flag as its only gate.
