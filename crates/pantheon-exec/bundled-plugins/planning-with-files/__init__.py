"""planning-with-files: persistent file-based planning for Pantheon.

Fires on pre_llm_call, pre_tool_call, post_tool_call, on_session_start,
and on_session_end hooks. Keeps task_plan.md, findings.md, and progress.md
on disk so the plan survives context loss, /clear, crashes, and compaction.

What it does:
  - pre_llm_call: injects plan context (Goal, Next Step, Current Phase, Phases)
  - pre_tool_call: checks plan exists, reminds to create one if missing
  - post_tool_call: reminds to update progress.md
  - on_session_start: restores plan state, injects recovery context
  - on_session_end: validates completion status

The plugin reads planning files from the project's .planning/ directory
or the legacy project root. It never writes to planning files directly;
it only injects context and reminders.
"""

import json
import os
import sys
from pathlib import Path

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

PLANNING_DIR = ".planning"
ACTIVE_PLAN_FILE = ".active_plan"
PLAN_FILES = ["task_plan.md", "findings.md", "progress.md"]

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def find_project_root() -> Path:
    """Find the project root by looking for .git or .planning."""
    cwd = Path.cwd()
    for parent in [cwd] + list(cwd.parents):
        if (parent / ".git").exists() or (parent / PLANNING_DIR).exists():
            return parent
    return cwd


def get_plan_dir(project_root: Path) -> Path:
    """Get the plan directory for the current session."""
    # Check for active plan pointer
    active_plan = project_root / PLANNING_DIR / ACTIVE_PLAN_FILE
    if active_plan.exists():
        plan_id = active_plan.read_text().strip()
        if plan_id:
            plan_dir = project_root / PLANNING_DIR / plan_id
            if plan_dir.exists():
                return plan_dir

    # Fall back to legacy project root
    if (project_root / "task_plan.md").exists():
        return project_root

    # Check for any plan in .planning/
    planning_dir = project_root / PLANNING_DIR
    if planning_dir.exists():
        plans = [d for d in planning_dir.iterdir() if d.is_dir() and d.name != "__pycache__"]
        if plans:
            # Use the most recently modified plan
            plans.sort(key=lambda d: d.stat().st_mtime, reverse=True)
            return plans[0]

    return project_root


def read_plan_file(plan_dir: Path, filename: str) -> str:
    """Read a planning file, returning empty string if missing."""
    path = plan_dir / filename
    if path.exists():
        return path.read_text(encoding="utf-8", errors="replace")
    return ""


def extract_plan_summary(plan_dir: Path) -> str:
    """Extract a concise summary of the plan for context injection."""
    task_plan = read_plan_file(plan_dir, "task_plan.md")
    progress = read_plan_file(plan_dir, "progress.md")

    if not task_plan:
        return ""

    # Extract key sections
    lines = task_plan.split("\n")
    sections = []
    current_section = None
    current_lines = []

    for line in lines:
        if line.startswith("## "):
            if current_section:
                sections.append((current_section, "\n".join(current_lines)))
            current_section = line[3:].strip()
            current_lines = [line]
        elif current_section:
            current_lines.append(line)

    if current_section:
        sections.append((current_section, "\n".join(current_lines)))

    # Build summary with key sections
    summary_parts = []
    key_sections = ["Goal", "Next Step", "Current Phase", "Phases"]

    for section_name, section_content in sections:
        if any(key.lower() in section_name.lower() for key in key_sections):
            # Truncate long sections
            content = section_content.strip()
            if len(content) > 500:
                content = content[:500] + "\n... (truncated)"
            summary_parts.append(content)

    if not summary_parts:
        # Fallback: just use the first 1000 chars
        summary_parts.append(task_plan[:1000] + "\n... (truncated)")

    return "\n\n".join(summary_parts)


def check_plan_exists(plan_dir: Path) -> bool:
    """Check if a plan exists."""
    return (plan_dir / "task_plan.md").exists()


def get_plan_status(plan_dir: Path) -> str:
    """Get a brief status of the plan."""
    task_plan = read_plan_file(plan_dir, "task_plan.md")
    if not task_plan:
        return "no_plan"

    # Count phases
    in_progress = task_plan.count("in_progress")
    complete = task_plan.count("complete")
    pending = task_plan.count("pending")

    return f"phases: {in_progress} in_progress, {complete} complete, {pending} pending"


# ---------------------------------------------------------------------------
# Hook handlers
# ---------------------------------------------------------------------------

def handle_pre_llm_call(input_data: dict) -> dict:
    """Inject plan context before LLM call."""
    project_root = find_project_root()
    plan_dir = get_plan_dir(project_root)

    if not check_plan_exists(plan_dir):
        return {}

    summary = extract_plan_summary(plan_dir)
    if not summary:
        return {}

    context = f"""[Planning Context]
{summary}

[Planning Status]
{get_plan_status(plan_dir)}

[Reminder]
Update progress.md after completing phases. Keep task_plan.md current."""

    return {"context": context}


def handle_pre_tool_call(input_data: dict) -> dict:
    """Check plan exists before tool call."""
    project_root = find_project_root()
    plan_dir = get_plan_dir(project_root)

    if not check_plan_exists(plan_dir):
        # Only remind for complex tasks (multiple tool calls)
        tool_name = input_data.get("tool_name", "")
        if tool_name in ["Bash", "Write", "Edit"]:
            return {
                "context": "[Planning] No task_plan.md found. Consider creating one for complex tasks."
            }

    return {}


def handle_post_tool_call(input_data: dict) -> dict:
    """Remind to update progress after tool call."""
    project_root = find_project_root()
    plan_dir = get_plan_dir(project_root)

    if not check_plan_exists(plan_dir):
        return {}

    # Only remind periodically (every 5th tool call)
    # This is a simple heuristic; the real plugin uses a turn marker
    return {
        "context": "[Planning] Update progress.md with what you just did. If a phase is now complete, update task_plan.md status."
    }


def handle_on_session_start(input_data: dict) -> dict:
    """Restore plan state on session start."""
    project_root = find_project_root()
    plan_dir = get_plan_dir(project_root)

    if not check_plan_exists(plan_dir):
        return {}

    summary = extract_plan_summary(plan_dir)
    if not summary:
        return {}

    context = f"""[Planning Recovery]
{summary}

[Planning Status]
{get_plan_status(plan_dir)}

Resume work from the Next Step in task_plan.md."""

    return {"context": context}


def handle_on_session_end(input_data: dict) -> dict:
    """Validate completion status on session end."""
    project_root = find_project_root()
    plan_dir = get_plan_dir(project_root)

    if not check_plan_exists(plan_dir):
        return {}

    task_plan = read_plan_file(plan_dir, "task_plan.md")
    if not task_plan:
        return {}

    # Check for incomplete phases
    in_progress = task_plan.count("in_progress")
    if in_progress > 0:
        return {
            "context": f"[Planning] Session ended with {in_progress} phase(s) still in_progress. Review task_plan.md."
        }

    return {}


# ---------------------------------------------------------------------------
# Main entry point
# ---------------------------------------------------------------------------

def main():
    """Main entry point for the plugin."""
    # Read input from stdin
    try:
        input_data = json.load(sys.stdin)
    except (json.JSONDecodeError, OSError):
        sys.exit(0)

    hook = input_data.get("hook", "")
    session_id = input_data.get("session_id", "")

    # Dispatch to handler
    handlers = {
        "pre_llm_call": handle_pre_llm_call,
        "pre_tool_call": handle_pre_tool_call,
        "post_tool_call": handle_post_tool_call,
        "on_session_start": handle_on_session_start,
        "on_session_end": handle_on_session_end,
    }

    handler = handlers.get(hook)
    if not handler:
        sys.exit(0)

    try:
        output = handler(input_data)
    except Exception:
        # Fail open: never break a turn
        sys.exit(0)

    if output:
        print(json.dumps(output))


if __name__ == "__main__":
    main()
