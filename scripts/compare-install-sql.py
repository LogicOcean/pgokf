#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Compare two pgrx-generated install scripts by content, not by order.

pgrx assembles install SQL from a dependency graph whose iteration order is
unstable across invocations, so two generations of the same source - or the
committed snapshot versus a regeneration - differ by entity ordering alone.
This tool compares connected-object entities as multisets of SQL tokens,
including all executable text outside the generated markers. Source comments
and whitespace between tokens are ignored; quoted strings and dollar bodies
remain byte-exact. Reordering entities is allowed; dropping or altering SQL
is not. The live parity harness independently checks catalog semantics.

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


# SQL lexical tokens: comments can be discarded, but quoted strings (including
# dollar bodies) must remain byte-exact. Normalizing their whitespace changes
# semantics. Block comments may nest in PostgreSQL.
def tokens(text):
    result = []
    i = 0
    while i < len(text):
        if text[i].isspace():
            i += 1
        elif text.startswith('--', i):
            end = text.find('\n', i)
            i = len(text) if end < 0 else end + 1
        elif text.startswith('/*', i):
            depth = 1
            i += 2
            while depth:
                if i >= len(text):
                    raise ValueError('unterminated SQL comment')
                if text.startswith('/*', i):
                    depth += 1
                    i += 2
                elif text.startswith('*/', i):
                    depth -= 1
                    i += 2
                else:
                    i += 1
        elif text[i] in "'\"":
            start, quote = i, text[i]
            i += 1
            while i < len(text):
                if text[i] == '\\':
                    i += 2
                elif text[i] == quote:
                    i += 1
                    if i < len(text) and text[i] == quote:
                        i += 1
                    else:
                        break
                else:
                    i += 1
            else:
                raise ValueError('unterminated SQL quote')
            result.append(text[start:i])
        elif text[i] == '$' and (match := re.match(r'\$(?:[A-Za-z_][A-Za-z_0-9]*)?\$', text[i:])):
            delimiter = match[0]
            end = text.find(delimiter, i + len(delimiter))
            if end < 0:
                raise ValueError('unterminated SQL dollar quote')
            end += len(delimiter)
            result.append(text[i:end])
            i = end
        elif match := re.match(r'[A-Za-z_0-9]+', text[i:]):
            result.append(match[0])
            i += len(match[0])
        else:
            result.append(text[i])
            i += 1
    return tuple(result)


def blocks(text: str) -> Counter:
    """Entity multiset plus all SQL outside entities; never discard executable SQL."""
    if BEGIN not in text or END not in text:
        raise ValueError('not a pgrx-generated install script')
    parts = re.split('(' + re.escape(BEGIN) + '|' + re.escape(END) + ')', text)
    inside = False
    normalized = []
    for part in parts:
        if part == BEGIN:
            if inside:
                raise ValueError('nested generated entity')
            inside = True
        elif part == END:
            if not inside:
                raise ValueError('unexpected entity end')
            inside = False
        else:
            body = tokens(part)
            if body:
                normalized.append(body)
    if inside:
        raise ValueError('unclosed generated entity')
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
    entities = blocks(Path(sys.argv[1]).read_text())
    statements = sum(entity.count(';') * count for entity, count in entities.items())
    print(f'equivalent: {sum(entities.values())} SQL entities, {statements} statements')
    return 0


if __name__ == '__main__':
    sys.exit(main())
