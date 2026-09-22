#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Compare two pgrx-generated install scripts by content, not by order.

pgrx assembles install SQL from a dependency graph whose iteration order is
unstable across invocations, so two generations of the same source - or the
committed snapshot versus a regeneration - differ by entity ordering alone.
This tool normalizes each `/* <begin connected objects> */` block (whitespace
collapsed, trailing spaces stripped) and compares the two files as
*multisets* of blocks: a reordered generation compares equal, while a dropped,
added, or altered statement does not.

Usage: compare-install-sql.py FILE_A FILE_B
Exit 0 when the contents are equivalent; exit 1 with the differing blocks
when they are not.
"""
import re
import sys
from collections import Counter
from pathlib import Path

BEGIN = '/* <begin connected objects> */'
END = '/* </end connected objects> */'


def blocks(text: str) -> Counter:
    """Split a pgrx install script into normalized connected-object blocks."""
    if BEGIN not in text or END not in text:
        raise ValueError('not a pgrx-generated install script (no connected-object blocks)')
    chunks = text.split(BEGIN)
    normalized = []
    for chunk in chunks[1:]:
        body = chunk.split(END, 1)[0]
        # Collapse all whitespace runs to single spaces and strip, so line
        # wrapping and trailing-space differences never masquerade as drift.
        body = re.sub(r'\s+', ' ', body).strip()
        if body:
            normalized.append(body)
    return Counter(normalized)


def compare(path_a: Path, path_b: Path) -> list[str]:
    """Return the blocks that differ between the two scripts ([] = equal)."""
    a, b = blocks(path_a.read_text()), blocks(path_b.read_text())
    delta = []
    for block, count in (a - b).items():
        delta.append(f'only in {path_a} (x{count}): {block[:200]}')
    for block, count in (b - a).items():
        delta.append(f'only in {path_b} (x{count}): {block[:200]}')
    return delta


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    delta = compare(Path(sys.argv[1]), Path(sys.argv[2]))
    if delta:
        print(f'install scripts differ ({len(delta)} block deltas):', file=sys.stderr)
        for line in delta[:20]:
            print('  ' + line, file=sys.stderr)
        return 1
    print(f'equivalent: {sum(blocks(Path(sys.argv[1]).read_text()).values())} blocks')
    return 0


if __name__ == '__main__':
    sys.exit(main())
