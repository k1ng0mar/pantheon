---
name: reverse-engineer-anything
description: "Reverse engineer binaries, applications, and runtime behavior with agents. Connects to Hopper, Ghidra, or IDA Pro for native binary analysis, and supports JavaScript/Electron app inspection, .NET assembly analysis, and website investigation. Use when you need to understand how a feature works without source code, analyze a binary or app, or investigate runtime behavior."
allowed-tools: Bash, Read, Write, Edit, Glob, Grep, AskUserQuestion, Agent, WebFetch
---

# Reverse Engineer Anything (REA)

See a feature you like. Understand how it works, down to the binary level.

REA connects your agent to tools for inspecting native binaries, JavaScript and Electron apps, .NET assemblies, and websites. Analysis runs locally, and results include the evidence and limitations behind each conclusion.

## Quick Start

### Check readiness
```bash
rea doctor --json
```

### Analyze a JavaScript/Electron app (no engine needed)
```bash
rea analyze-javascript-application /absolute/path/to/app --json
```

### Analyze a native binary (requires Hopper, Ghidra, or IDA)
```bash
rea analyze /path/to/binary --provider ghidra --json
```

### Open a binary in an analysis engine
```bash
rea open-binary /path/to/binary --provider hopper
```

## Supported Analysis Types

| Target | Provider | What it does |
|--------|----------|--------------|
| Native binaries (PE, ELF, Mach-O) | Hopper, Ghidra, IDA Pro | Disassembly, decompilation, symbol recovery, string extraction |
| JavaScript/Electron apps | None (static) | Extract and analyze JS bundles, ASAR archives, Electron app structure |
| .NET assemblies | Ghidra (with plugin) | Decompile and analyze .NET assemblies |
| Websites | None (static) | Analyze website structure, behavior, and content |
| Android APKs | JADX (headless) | Static APK analysis without emulator |
| Firmware | Binwalk, Unblob | Firmware region inspection and extraction |

## Analysis Workflow

1. **Identify the target**: What are you investigating? A binary, an app, a website?
2. **Check readiness**: Run `rea doctor --json` to see what engines are available
3. **Run analysis**: Use the appropriate `rea` command for your target type
4. **Review evidence**: REA returns evidence, recovered graphs, limitations, and unknowns
5. **Build understanding**: Use the evidence to explain or compatibly recreate observed behavior

## Key Principles

- **Evidence-based**: Every conclusion includes evidence and limitations
- **Local analysis**: No hosted service; all analysis runs on your machine
- **Provider selection**: When multiple engines support a target, REA asks you to choose one
- **Session persistence**: Analysis sessions can be resumed after interruption

## Integration with Pantheon

REA can be used as:
- **CLI tool**: Shell out to `rea` commands from Pantheon tools
- **MCP server**: Connect Pantheon's MCP client to REA's MCP server for tool-based analysis
- **Skill**: This skill provides the investigation workflow and context

## Supported Agents

REA supports Claude Code, Claude Desktop, Codex, Cursor, Gemini CLI, Windsurf, Devin, OpenCode, Antigravity, GitHub Copilot CLI, Command Code, and VS Code. Any agent that can run a local MCP server can use the manual configuration.

## Requirements

- Node.js 22.x (>=22.19), 24.x (>=24.11), or 26+
- npm
- For native analysis: Hopper, Ghidra, or IDA Pro (bring your own)
- For Android analysis: JADX JAR and full JDK
- For firmware analysis: Binwalk and Unblob on Linux

## License

MIT
