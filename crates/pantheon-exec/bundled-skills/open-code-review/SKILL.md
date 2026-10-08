---
name: open-code-review
description: "AI-powered code review with deterministic engineering and LLM hybrid. Reviews git diffs, scans entire files, and produces structured line-level comments. Use when reviewing code changes, auditing unfamiliar codebases, or performing security reviews. Supports delegation mode where the host agent performs the review itself."
allowed-tools: Bash, Read, Write, Edit, Glob, Grep, AskUserQuestion, Agent, WebFetch
---

# Open Code Review

AI-powered code review CLI tool from Alibaba. Combines deterministic engineering (hard constraints for steps that must not go wrong) with an LLM agent (for dynamic decisions and context retrieval).

## Quick Start

### Install
```bash
npm install -g @alibaba-group/open-code-review
```

### Configure LLM
```bash
ocr config provider  # Select a built-in provider or add a custom one
ocr config model     # Pick a model for the active provider
```

### Review code
```bash
# Workspace mode — review all staged, unstaged, and untracked changes
ocr review

# Branch range — review changes since divergence from main
ocr review --from main --to feature-branch

# Single commit
ocr review --commit abc123

# Full-file scan — review whole files instead of a diff
ocr scan
ocr scan --path internal/agent

# Save results to a file (recommended for AI host agents)
ocr review --format json --output result.json
```

### Delegation mode
```bash
# Let your AI coding agent perform the review itself
ocr delegate preview
ocr delegate rule src/main.go src/handler.go
```

## Core Design: Deterministic Engineering x Agent Hybrid

### Deterministic Engineering (Hard Constraints)
For review steps that must not go wrong:
- **Precise file selection**: Determines exactly which files need review
- **Smart file bundling**: Groups related files into a single review unit
- **Fine-grained rule matching**: Matches review rules to each file's characteristics
- **External positioning and reflection modules**: Improve location and content accuracy

### Agent (Dynamic Decision-Making)
For dynamic decisions and context retrieval:
- **Scenario-tuned prompts**: Optimized for code review
- **Scenario-tuned toolset**: Purpose-built for code review stability

## Review Rules

Open Code Review supports customizable review rules with path filtering and targeting. Rules can be defined for:
- Security vulnerabilities (XSS, SQL injection, etc.)
- Code quality issues
- Performance problems
- Thread safety
- Null pointer exceptions
- And more

## Integration with Pantheon

Open Code Review can be used as:
- **CLI tool**: Shell out to `ocr` commands from Pantheon tools
- **MCP server**: Connect Pantheon's MCP client to OCR's MCP server
- **Delegation mode**: Let Pantheon's agent perform the review itself

## Supported Platforms

- Windows, macOS, Linux
- Git >= 2.41
- OpenAI and Anthropic API compatible

## Benchmark

Compared to general-purpose agents, Open Code Review achieves significantly higher Precision and F1 with the same underlying model, while consuming only ~1/9 of the tokens and completing reviews faster.

## License

Apache-2.0
