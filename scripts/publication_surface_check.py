#!/usr/bin/env python3
"""Publication surface check — regex-based scan for private references.

Complements scripts/docs_rubric_assess.py: the rubric scores qualitative
documentation health, this script is a deterministic hard-fail gate for
things that must never appear in the public docs corpus (Gate 7 of the
public release checklist).

Exits 1 on any hit, 0 when clean, 2 on usage error.

Usage:
    python3 scripts/publication_surface_check.py
    python3 scripts/publication_surface_check.py --markdown --output /tmp/ps.md
    python3 scripts/publication_surface_check.py --corpus-root . --scope docs

Scope flags:
    --scope docs    — README, CHANGELOG, docs/manuals/**, docs/reference/**,
                      docs/adr/** (matches the rubric's corpus scope)
    --scope wide    — docs scope + scripts/**, justfile, deploy/ stays excluded

Pattern categories:
    hostname  — private FQDNs (*.internal.*, *.kellgari.*, prod-vm-*)
    ip        — RFC1918 10.x / 192.168.x / 172.16-31.x addresses
    path      — absolute /home/<user>/, /Users/<user>/, work/taskserver/ paths
    tool      — internal tooling references (teax, apollo-vm, etc.)
    url       — internal service URLs (gitea.internal, ci.internal, etc.)

Add or remove patterns in PATTERNS below. Each entry is deliberately
minimal — false positives are preferable to false negatives for a
hard-fail publication gate. Tighten with allowlist if noise becomes a
problem.
"""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass
from pathlib import Path

# Sibling module with the canonical docs corpus scope. Kept in sync
# with scripts/docs_rubric_assess.py via a shared import rather than
# two copies that drift.
sys.path.insert(0, str(Path(__file__).resolve().parent))
from _docs_corpus import (  # noqa: E402
    DOCS_CORPUS_GLOBS,
    EXCLUDE_PREFIXES as _SHARED_EXCLUDE_PREFIXES,
)


# (category, name, pattern, description)
# Patterns ordered by likelihood of being a real leak, most severe first.
PATTERNS: list[tuple[str, str, re.Pattern[str], str]] = [
    (
        "hostname",
        "internal_domain",
        re.compile(r"\b[\w-]+\.(?:internal|kellgari|lan)\.[\w.-]+\b"),
        "Private FQDN (*.internal.*, *.kellgari.*, *.lan.*)",
    ),
    (
        "hostname",
        "kellgari_tld",
        re.compile(r"\bkellgari\.(?:com\.au|com|net|org)\b"),
        "kellgari.com.au (or variant) as a top-level domain (emails, URLs)",
    ),
    (
        "hostname",
        "prod_vm_prefix",
        re.compile(r"\bprod-vm-[\w-]+"),
        "Internal VM naming convention (prod-vm-*)",
    ),
    (
        "hostname",
        "vm_ci_prefix",
        re.compile(r"\bvm-ci-[\w-]+"),
        "Internal CI runner naming convention (vm-ci-*)",
    ),
    (
        "url",
        "internal_gitea_url",
        re.compile(
            r"https?://[\w.-]*(?:gitea|ci|jenkins|woodpecker)\.[\w.-]*"
            r"\.(?:internal|kellgari|lan)\b"
        ),
        "Internal service URL (gitea/ci/woodpecker.*.internal)",
    ),
    (
        "url",
        "generic_internal_url",
        re.compile(r"https?://[\w.-]+\.(?:internal|kellgari|lan)\b[^\s)]*"),
        "Any URL pointing at *.internal / *.kellgari / *.lan",
    ),
    (
        "ip",
        "rfc1918_10",
        re.compile(r"\b10\.\d{1,3}\.\d{1,3}\.\d{1,3}\b"),
        "RFC1918 10.x.x.x address",
    ),
    (
        "ip",
        "rfc1918_192168",
        re.compile(r"\b192\.168\.\d{1,3}\.\d{1,3}\b"),
        "RFC1918 192.168.x.x address",
    ),
    (
        "ip",
        "rfc1918_172",
        re.compile(r"\b172\.(?:1[6-9]|2\d|3[01])\.\d{1,3}\.\d{1,3}\b"),
        "RFC1918 172.16-31.x.x address",
    ),
    (
        "path",
        "home_dir",
        # Match both '/home/singlis/...' and bare '/home/singlis' by
        # anchoring to either a trailing slash or a word boundary.
        re.compile(r"/home/[a-z][a-z0-9_-]{1,}(?:/|\b)"),
        "Absolute /home/<user>/ path",
    ),
    (
        "path",
        "users_dir",
        re.compile(r"/Users/[A-Za-z][A-Za-z0-9_-]+/"),
        "Absolute /Users/<user>/ path (macOS)",
    ),
    (
        "path",
        "work_taskserver",
        re.compile(r"\bwork/taskserver/"),
        "Author-local 'work/taskserver/' path",
    ),
    (
        "tool",
        "internal_gitea_cli",
        re.compile(r"\b(?:teax|tea issue|tea pr|tea repo)\b"),
        "Internal Gitea CLI reference (tea/teax)",
    ),
    (
        "tool",
        "apollo_vm",
        re.compile(r"\bapollo-(?:vm|tools)\b"),
        "Internal Proxmox tooling reference",
    ),
    (
        "tool",
        "internal_mcp",
        re.compile(r"\b(?:mcp-proxy|claude_ai_Gmail|claude_ai_Google)\b"),
        "Internal MCP plugin reference",
    ),
    (
        "reference",
        "claude_md",
        re.compile(r"\bCLAUDE\.md\b"),
        "Reference to internal CLAUDE.md (instructions file)",
    ),
    (
        "url",
        "slack_archive",
        re.compile(r"https?://[\w.-]*\.slack\.com/archives/\w+"),
        "Private Slack archive URL (leaks workspace name)",
    ),
    (
        "url",
        "internal_notion",
        re.compile(r"https?://[\w.-]*\.notion\.site/[\w-]+"),
        "Private Notion workspace URL",
    ),
    (
        "hostname",
        "tailscale_ts_net",
        re.compile(r"\b[\w-]+\.[\w-]+\.ts\.net\b"),
        "Tailscale MagicDNS hostname (leaks tailnet)",
    ),
]


@dataclass
class Hit:
    path: str
    line: int
    column: int
    category: str
    name: str
    description: str
    match_text: str
    snippet: str


@dataclass(frozen=True)
class Allowlist:
    """Parsed allowlist entries from an allowlist file.

    File format (one entry per line, # comments, blank lines ignored):

        path:<relative/path>              — skip this file entirely
        hit:<relative/path>:<line>:<pattern_name>  — suppress one exact hit
        pattern:<pattern_name>            — disable this pattern everywhere
                                            (requires --allow-pattern-disable)

    Prefer 'hit:' — it's line-pinned, so the suppression re-fires when
    surrounding content changes. 'path:' is acceptable for whole files
    that are inherently noisy. 'pattern:' is a sledgehammer — it
    silently disables a pattern across the entire tree with no audit
    trail, so it's disabled by default; pass --allow-pattern-disable to
    opt in.
    """

    paths: frozenset[str]
    patterns: frozenset[str]
    hits: frozenset[tuple[str, int, str]]

    @classmethod
    def empty(cls) -> "Allowlist":
        return cls(frozenset(), frozenset(), frozenset())

    @classmethod
    def load(cls, path: Path, allow_pattern_disable: bool = False) -> "Allowlist":
        paths: set[str] = set()
        patterns: set[str] = set()
        hits: set[tuple[str, int, str]] = set()
        for lineno, raw in enumerate(
            path.read_text(encoding="utf-8").splitlines(), start=1
        ):
            line = raw.split("#", 1)[0].strip()
            if not line:
                continue
            if line.startswith("path:"):
                paths.add(line[len("path:") :].strip())
            elif line.startswith("pattern:"):
                if not allow_pattern_disable:
                    raise ValueError(
                        f"{path}:{lineno}: 'pattern:' entry disables a "
                        "pattern globally, which is too powerful for "
                        "routine use. Pass --allow-pattern-disable to "
                        "opt in, or use a 'hit:' entry instead."
                    )
                patterns.add(line[len("pattern:") :].strip())
            elif line.startswith("hit:"):
                body = line[len("hit:") :].strip()
                parts = body.rsplit(":", 2)
                if len(parts) != 3:
                    raise ValueError(
                        f"{path}:{lineno}: 'hit:' entry must be "
                        f"'hit:<path>:<line>:<pattern_name>', got {body!r}"
                    )
                hit_path, line_str, pattern_name = parts
                try:
                    hit_line = int(line_str)
                except ValueError as exc:
                    raise ValueError(
                        f"{path}:{lineno}: line number in 'hit:' entry "
                        f"must be an integer, got {line_str!r}"
                    ) from exc
                hits.add((hit_path, hit_line, pattern_name))
            else:
                raise ValueError(
                    f"{path}:{lineno}: unrecognized allowlist entry "
                    f"(must start with 'path:', 'pattern:', or 'hit:'): "
                    f"{line!r}"
                )
        return cls(frozenset(paths), frozenset(patterns), frozenset(hits))

    def allows_file(self, rel_path: str) -> bool:
        return rel_path in self.paths

    def allows_pattern(self, pattern_name: str) -> bool:
        return pattern_name in self.patterns

    def allows_hit(self, hit: Hit) -> bool:
        return (hit.path, hit.line, hit.name) in self.hits


SCOPES: dict[str, tuple[str, ...]] = {
    # The 'docs' scope is the shared canonical set used by both this
    # tool and docs_rubric_assess.py — change it in _docs_corpus.py.
    "docs": DOCS_CORPUS_GLOBS,
    "wide": (
        *DOCS_CORPUS_GLOBS,
        "scripts/**/*.py",
        "scripts/**/*.sh",
        "justfile",
    ),
}

# Always excluded from scope: these are internal-by-design and don't ship
# in the public branch. The prepare-public-branch.sh script already drops
# them, so they don't need to pass the gate. The shared baseline comes
# from _docs_corpus; the publication-surface check adds a few more that
# only apply when scanning the 'wide' scope.
EXCLUDE_PREFIXES = (
    *_SHARED_EXCLUDE_PREFIXES,
    ".claude/",
    ".woodpecker/",
    "deploy/",
)

# This script contains verbatim pattern strings in PATTERNS and the help
# text — it would fire on every one of its own regexes. Excluding itself
# by exact relative path is the cleanest fix: transparent, one line, no
# pragma-comment shenanigans. If the patterns ever move to an external
# data file, drop this entry.
EXCLUDE_EXACT = frozenset({"scripts/publication_surface_check.py"})


def collect_files(
    corpus_root: Path,
    globs: tuple[str, ...],
    allowlist: Allowlist,
) -> list[tuple[Path, str]]:
    files: dict[str, Path] = {}
    for pattern in globs:
        for match in sorted(corpus_root.glob(pattern)):
            if match.is_symlink() or not match.is_file():
                continue
            rel = match.relative_to(corpus_root).as_posix()
            if rel.startswith(EXCLUDE_PREFIXES):
                continue
            if rel in EXCLUDE_EXACT:
                continue
            if allowlist.allows_file(rel):
                continue
            files.setdefault(rel, match)
    return [(p, r) for r, p in sorted(files.items())]


def _snippet(line: str, start: int, end: int, width: int = 80) -> str:
    line = line.rstrip("\n")
    if len(line) <= width:
        return line.strip()
    lo = max(0, start - width // 2)
    hi = min(len(line), end + width // 2)
    prefix = "…" if lo > 0 else ""
    suffix = "…" if hi < len(line) else ""
    return f"{prefix}{line[lo:hi].strip()}{suffix}"


def scan_file(path: Path, rel: str, allowlist: Allowlist) -> list[Hit]:
    hits: list[Hit] = []
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError as e:
        print(f"warning: could not read {rel}: {e}", file=sys.stderr)
        return hits
    for lineno, line in enumerate(text.splitlines(), start=1):
        for category, name, pattern, description in PATTERNS:
            if allowlist.allows_pattern(name):
                continue
            for m in pattern.finditer(line):
                hit = Hit(
                    path=rel,
                    line=lineno,
                    column=m.start() + 1,
                    category=category,
                    name=name,
                    description=description,
                    match_text=m.group(0),
                    snippet=_snippet(line, m.start(), m.end()),
                )
                if allowlist.allows_hit(hit):
                    continue
                hits.append(hit)
    return hits


def render_terse(hits: list[Hit]) -> str:
    lines = []
    for h in hits:
        lines.append(
            f"{h.path}:{h.line}:{h.column}: "
            f"[{h.category}/{h.name}] {h.match_text!r} — {h.snippet}"
        )
    return "\n".join(lines)


def render_markdown(hits: list[Hit], file_count: int, scope: str) -> str:
    lines = [
        "# Publication Surface Check",
        "",
        f"**Scope:** `{scope}` — {file_count} files scanned",
        "",
    ]
    if not hits:
        lines.append("**Result:** ✅ clean — no private references found.")
        return "\n".join(lines) + "\n"

    lines.append(f"**Result:** ❌ {len(hits)} hits across {len({h.path for h in hits})} files")
    lines.append("")

    # Summary by category
    by_cat: dict[str, int] = {}
    for h in hits:
        by_cat[h.category] = by_cat.get(h.category, 0) + 1
    lines.append("## Summary by category")
    lines.append("")
    for cat in sorted(by_cat):
        lines.append(f"- `{cat}`: {by_cat[cat]}")
    lines.append("")

    # Hits grouped by file
    lines.append("## Findings")
    lines.append("")
    current_file: str | None = None
    for h in hits:
        if h.path != current_file:
            if current_file is not None:
                lines.append("")
            lines.append(f"### `{h.path}`")
            lines.append("")
            current_file = h.path
        lines.append(
            f"- **{h.line}:{h.column}** `[{h.category}/{h.name}]` "
            f"`{h.match_text}` — {h.description}"
        )
        lines.append(f"  > {h.snippet}")
    lines.append("")
    return "\n".join(lines)


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(
        description=(
            "Scan the docs corpus for private references (private FQDNs, "
            "RFC1918 IPs, absolute local paths, internal tooling). Exits "
            "1 on any hit."
        ),
    )
    p.add_argument(
        "--corpus-root",
        type=Path,
        default=Path.cwd(),
        help="Repo root to scan (default: current directory).",
    )
    p.add_argument(
        "--scope",
        choices=sorted(SCOPES),
        default="docs",
        help=(
            "Glob scope to scan. 'docs' matches the rubric corpus; "
            "'wide' also includes scripts/ and justfile."
        ),
    )
    p.add_argument(
        "--markdown",
        action="store_true",
        help="Emit markdown report instead of terse grep-style output.",
    )
    p.add_argument(
        "--output",
        type=Path,
        help="Path to write the report (stdout is also written).",
    )
    p.add_argument(
        "--allowlist",
        type=Path,
        help=(
            "Path to an allowlist file. Each line is one of: "
            "'path:<relpath>' (skip file), "
            "'hit:<path>:<line>:<pattern_name>' (suppress one exact hit), "
            "or 'pattern:<name>' (disable pattern globally — requires "
            "--allow-pattern-disable). Blank lines and '#' comments are "
            "ignored."
        ),
    )
    p.add_argument(
        "--allow-pattern-disable",
        action="store_true",
        help=(
            "Permit 'pattern:' entries in the allowlist file. Off by "
            "default because global pattern disables are hard to audit "
            "and can silently hide regressions across the whole tree."
        ),
    )
    p.add_argument(
        "--list-patterns",
        action="store_true",
        help="Print the pattern table and exit.",
    )
    return p.parse_args()


def main() -> int:
    args = parse_args()

    if args.list_patterns:
        for category, name, pattern, description in PATTERNS:
            print(f"{category:10} {name:22} {pattern.pattern}")
            print(f"           {description}")
        return 0

    if args.allowlist is not None:
        if not args.allowlist.is_file():
            print(
                f"error: allowlist file not found: {args.allowlist}",
                file=sys.stderr,
            )
            return 2
        try:
            allowlist = Allowlist.load(
                args.allowlist,
                allow_pattern_disable=args.allow_pattern_disable,
            )
        except ValueError as e:
            print(f"error: {e}", file=sys.stderr)
            return 2
    else:
        allowlist = Allowlist.empty()

    files = collect_files(args.corpus_root, SCOPES[args.scope], allowlist)
    if not files:
        print(
            f"error: no files matched under {args.corpus_root} "
            f"(scope={args.scope})",
            file=sys.stderr,
        )
        return 2

    allowlist_summary_parts: list[str] = []
    if allowlist.paths:
        allowlist_summary_parts.append(f"{len(allowlist.paths)} paths")
    if allowlist.patterns:
        allowlist_summary_parts.append(f"{len(allowlist.patterns)} patterns")
    if allowlist.hits:
        allowlist_summary_parts.append(f"{len(allowlist.hits)} hits")
    allowlist_summary = (
        f", allowlist: {', '.join(allowlist_summary_parts)}"
        if allowlist_summary_parts
        else ""
    )
    print(
        f"scanning {len(files)} files under {args.corpus_root} "
        f"(scope={args.scope}{allowlist_summary})",
        file=sys.stderr,
    )

    all_hits: list[Hit] = []
    for path, rel in files:
        all_hits.extend(scan_file(path, rel, allowlist))

    if args.markdown:
        report = render_markdown(all_hits, len(files), args.scope)
    else:
        report = render_terse(all_hits)

    if report:
        print(report)

    if args.output:
        args.output.write_text(report + "\n", encoding="utf-8")
        print(f"wrote {args.output}", file=sys.stderr)

    # Summary
    by_cat: dict[str, int] = {}
    for h in all_hits:
        by_cat[h.category] = by_cat.get(h.category, 0) + 1
    if all_hits:
        print(
            f"\nFAIL: {len(all_hits)} hits across "
            f"{len({h.path for h in all_hits})} files",
            file=sys.stderr,
        )
        for cat in sorted(by_cat):
            print(f"  {cat}: {by_cat[cat]}", file=sys.stderr)
        return 1

    print("PASS: no private references found", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
