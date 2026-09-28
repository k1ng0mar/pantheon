#!/usr/bin/env python3
"""Render docs/*.md into web/*.html using web/layout.html.

Single source of truth is the markdown tree. Regenerate after edits:

    python3 web/build.py

Pages marked internal (developer/tui.md, developer/decisions/) are
deliberately skipped: they ship in the repo, not on the site.
"""
import html
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DOCS = ROOT / "docs"
WEB = ROOT / "web"

# md path (relative to docs/) -> html file
PAGES = {
    "index.md": "docs.html",
    "getting-started.md": "getting-started.html",
    "user-guide/sessions.md": "sessions.html",
    "user-guide/agents.md": "agents.html",
    "user-guide/memory.md": "memory.html",
    "user-guide/runs.md": "runs.html",
    "user-guide/channels.md": "channels.html",
    "user-guide/providers.md": "providers.html",
    "user-guide/extensions.md": "extensions.html",
    "reference/terminal.md": "terminal.html",
    "reference/configuration.md": "configuration.html",
    "reference/troubleshooting.md": "troubleshooting.html",
    "developer/architecture.md": "architecture.html",
    "developer/contributing.md": "contributing.html",
}

MD_TARGET = re.compile(r"\(([^)\s\]]+?\.md)(#[^)]*)?\)")
DIR_TARGET = re.compile(r"\(([^)\s\]]+?/)\)")
GITHUB_DOCS = "https://github.com/k1ng0mar/pantheon/blob/master/docs/"
GITHUB_TREE = "https://github.com/k1ng0mar/pantheon/tree/master/docs/"


def slugify(raw: str) -> str:
    """GitHub-style anchor slug for a heading's raw markdown text."""
    s = re.sub(r"\*\*(.+?)\*\*", r"\1", raw)
    s = s.replace("`", "").strip().lower()
    s = re.sub(r"\s+", "-", s)
    s = re.sub(r"[^a-z0-9\-_]", "", s)
    return s or "section"


def md_link_to_html(match: re.Match, src: str) -> str:
    target, anchor = match.group(1), match.group(2) or ""
    src_dir = str(Path(src).parent)
    resolved = str(Path(src_dir, target).as_posix())
    # normalize ../ segments without touching the filesystem
    parts: list[str] = []
    for p in resolved.split("/"):
        if p == "..":
            if parts:
                parts.pop()
        elif p != ".":
            parts.append(p)
    key = "/".join(parts)
    if key in PAGES:
        return f"({PAGES[key]}{anchor})"
    # Internal-only pages have no site mirror; point at the repo file so the
    # link resolves instead of 404ing.
    return f"({GITHUB_DOCS}{key}{anchor})"


def inline(text: str) -> str:
    text = html.escape(text)
    text = re.sub(r"\*\*(.+?)\*\*", r"<strong>\1</strong>", text)
    text = re.sub(r"`([^`]+?)`", r"<code>\1</code>", text)
    text = re.sub(r"\[([^\]]+?)\]\(([^)]+?)\)", r'<a href="\2">\1</a>', text)
    return text


def render(md: str, src: str) -> tuple[str, str]:
    md = MD_TARGET.sub(lambda m: md_link_to_html(m, src), md)

    def _dir(m: re.Match) -> str:
        target = m.group(1)
        if re.match(r"^(https?://|/|#|mailto:)", target):
            return m.group(0)
        src_dir = str(Path(src).parent)
        resolved = str(Path(src_dir, target).as_posix())
        parts: list[str] = []
        for p in resolved.split("/"):
            if p == "..":
                if parts:
                    parts.pop()
            elif p != ".":
                parts.append(p)
        return f"({GITHUB_TREE}{'/'.join(parts)})"

    md = DIR_TARGET.sub(_dir, md)
    out: list[str] = []
    in_code = False
    in_list = False
    in_table = False
    title = "Docs"
    seen_slugs: dict[str, int] = {}

    def heading_id(raw: str) -> str:
        sid = slugify(raw)
        seen_slugs[sid] = seen_slugs.get(sid, 0) + 1
        return sid if seen_slugs[sid] == 1 else f"{sid}-{seen_slugs[sid] - 1}"

    def close_list() -> None:
        nonlocal in_list
        if in_list:
            out.append("</ul>")
            in_list = False

    def close_table() -> None:
        nonlocal in_table
        if in_table:
            out.append("</tbody></table>")
            in_table = False

    for raw in md.splitlines():
        line = raw.rstrip()
        if line.startswith("```"):
            close_list()
            close_table()
            in_code = not in_code
            out.append("<pre><code>" if in_code else "</code></pre>")
            continue
        if in_code:
            out.append(html.escape(raw))
            continue
        if not line.strip():
            close_list()
            close_table()
            continue
        if line.strip() == "---":
            close_list()
            close_table()
            out.append('<div class="hairline my-8"></div>')
            continue
        m = re.match(r"^(#{1,3})\s+(.*)", line)
        if m:
            close_list()
            close_table()
            level, text = len(m.group(1)), inline(m.group(2).strip())
            sid = heading_id(m.group(2).strip())
            if level == 1:
                title = m.group(2).strip()
                out.append(
                    f'<h1 id="{sid}" class="text-3xl font-bold tracking-tight mb-2">{text}</h1>'
                    '<div class="hairline my-6"></div>'
                )
            elif level == 2:
                out.append(
                    f'<h2 id="{sid}" class="text-xl font-bold tracking-tight mt-10 mb-3">{text}</h2>'
                )
            else:
                out.append(f'<h3 id="{sid}" class="font-bold mt-6 mb-2">{text}</h3>')
            continue
        if line.startswith("> "):
            close_list()
            close_table()
            out.append(f"<blockquote>{inline(line[2:])}</blockquote>")
            continue
        if re.match(r"^(\||\|?\s*:?-{3,})", line) and "|" in line:
            close_list()
            cells = [c.strip() for c in line.strip().strip("|").split("|")]
            if re.fullmatch(r"[:\-\s|]+", line):
                continue  # separator row
            is_header = not in_table
            if is_header:
                out.append(
                    '<table class="my-6"><thead><tr>'
                    + "".join(f"<th>{inline(c)}</th>" for c in cells)
                    + "</tr></thead><tbody>"
                )
                in_table = True
            else:
                out.append(
                    "<tr>" + "".join(f"<td>{inline(c)}</td>" for c in cells) + "</tr>"
                )
            continue
        if re.match(r"^[-*]\s+", line):
            close_table()
            if not in_list:
                out.append('<ul class="list-disc pl-6 my-3 space-y-1">')
                in_list = True
            out.append(f"<li>{inline(line[2:])}</li>")
            continue
        if re.match(r"^\d+\.\s+", line):
            close_list()
            close_table()
            out.append(f"<p>{inline(line)}</p>")
            continue
        close_list()
        close_table()
        out.append(f'<p class="my-3 leading-relaxed">{inline(line)}</p>')

    close_list()
    close_table()
    body = "\n".join(out)
    return title, (
        '<main class="doc max-w-3xl mx-auto px-6 pt-24 pb-16">\n' + body + "\n</main>"
    )


def main() -> int:
    layout = (WEB / "layout.html").read_text()
    for src, page in PAGES.items():
        md = (DOCS / src).read_text()
        title, body = render(md, src)
        page_title = html.escape(title)
        if not page_title.lower().startswith("pantheon"):
            page_title = f"Pantheon: {page_title}"
        html_page = layout.replace("%BODY%", body).replace(
            "<title>Pantheon docs</title>",
            f"<title>{page_title}</title>",
        )
        (WEB / page).write_text(html_page)
        print(f"  {src} -> {page}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
