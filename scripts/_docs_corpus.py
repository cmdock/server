"""Shared documentation-corpus scope for the docs assessment tools.

Both `scripts/docs_rubric_assess.py` and `scripts/publication_surface_check.py`
grade or scan the same conceptual "public docs corpus" of cmdock/server:
the README, CHANGELOG, and the three canonical docs directories
(manuals, reference, adr).

Previously each script defined its own glob list. They happened to
match, but drift was inevitable — adding `docs/implementation/` or
similar to one tool silently missed it in the other. This module is the
single source of truth; both tools import from it.

Conventions:
- DOCS_CORPUS_GLOBS is a tuple, not a list, so it's immutable from
  callers and safe to share as a module constant.
- Globs are repo-root-relative. Tools resolve them against their
  `--corpus-root` argument.
- EXCLUDE_PREFIXES is the minimum set both tools agree on. Tools that
  want a broader scope (e.g., the publication-surface check's `wide`
  scope that also covers scripts/) layer their own additions on top.
- Everything here is stdlib-only so the publication-surface check
  stays dependency-free.
"""

from __future__ import annotations

# Canonical public-docs corpus. Anything a reader of the published repo
# would land on when browsing docs should match one of these globs.
DOCS_CORPUS_GLOBS: tuple[str, ...] = (
    "README.md",
    "CHANGELOG.md",
    "docs/manuals/**/*.md",
    "docs/reference/**/*.md",
    "docs/adr/**/*.md",
    # Implementation notes are linked from docs/manuals/index.md so they
    # are part of the public navigable surface. Excluding them caused
    # the rubric and publication-surface tools to report false-positive
    # "dead links" in the index during the initial Sonnet A/B run.
    "docs/implementation/**/*.md",
)

# Directories that are never part of the public surface — they're
# dropped by scripts/prepare-public-branch.sh during the branch split,
# so there's no value in grading or scanning them.
EXCLUDE_PREFIXES: tuple[str, ...] = (
    "docs/internal/",
    "docs/private/",
)
