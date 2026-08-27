#!/usr/bin/env python3
"""Deterministic OKF corpus generator for the format benchmark.

Emits a self-contained OKF bundle on disk: N Markdown concepts with valid
YAML frontmatter (``type``/``title``/``description``/``tags``), a realistic
multi-paragraph body, resolvable internal links, one external link per file,
and provenance frontmatter on a fixed fraction of files.

The generator is fully deterministic: the same ``--count``/``--seed`` always
produce byte-identical output, so a benchmark run is reproducible. Every
concept's on-disk path is a pure function of its global index, which is what
lets internal links point at other concepts that are guaranteed to exist.

A machine-readable ``manifest.json`` is written alongside the bundle so the
orchestrator (``run_bench.sh``) can drive representative queries without
hard-coding any generation detail: it carries the exact expected row counts
for the type/tag filters, a traversal seed concept, and the FTS query.

Example
-------
    python3 generate_corpus.py --count 12000 --out /tmp/corpus --seed 1337
"""

from __future__ import annotations

import argparse
import json
import os
import random
import sys
from pathlib import Path

# --- Corpus shape -----------------------------------------------------------

# Directory categories double as concept types (1:1), so "filter by type" and
# "filter by directory" select the same rows and their counts are predictable.
CATEGORIES: tuple[str, ...] = (
    "services",
    "runbooks",
    "references",
    "dashboards",
    "guides",
    "policies",
    "datasets",
    "pipelines",
)
TYPES: tuple[str, ...] = (
    "Service",
    "Runbook",
    "Reference",
    "Dashboard",
    "Guide",
    "Policy",
    "Dataset",
    "Pipeline",
)

# Files per leaf directory, so no single directory holds tens of thousands of
# entries (a realistic bundle is a tree, not one flat folder).
BUCKET_SIZE: int = 250

# One tag applied to a knowable fraction (every third concept) so the GIN tag
# filter has stable, non-trivial selectivity.
SELECTIVE_TAG: str = "postgres"
SELECTIVE_TAG_STRIDE: int = 3

# Full-text query whose two stemmed terms are planted together in every
# service and runbook body (categories 0 and 1), giving the FTS leg real hits.
FTS_QUERY: str = "replication failover"
FTS_CATEGORY_INDEXES: frozenset[int] = frozenset({0, 1})

# The type whose count the benchmark reports for the filtered-scan leg.
FILTER_TYPE: str = "Runbook"

# Provenance frontmatter is emitted on this fraction of files (sparse, per the
# provenance projection's "only concepts carrying such frontmatter" contract).
PROVENANCE_MODULUS: int = 10
PROVENANCE_THRESHOLD: int = 3  # i % 10 in {0,1,2} -> 30%

# Extra tags drawn to give each concept 2-4 tags of realistic variety.
TAG_VOCAB: tuple[str, ...] = (
    "database", "observability", "incident-response", "oncall", "storage",
    "networking", "security", "capacity", "latency", "throughput", "backup",
    "replication", "indexing", "analytics", "streaming", "batch", "sre",
    "platform", "reliability", "compliance",
)

# Technical vocabulary for prose bodies; stemmed forms feed FTS realistically.
BODY_VOCAB: tuple[str, ...] = (
    "replication", "failover", "latency", "throughput", "partition", "vacuum",
    "checkpoint", "index", "buffer", "connection", "pool", "saturation",
    "standby", "primary", "recovery", "archive", "segment", "transaction",
    "commit", "rollback", "isolation", "snapshot", "cluster", "node", "shard",
    "query", "planner", "statistics", "cardinality", "selectivity", "cache",
    "eviction", "workload", "concurrency", "contention", "deadlock", "lock",
    "sequence", "trigger", "constraint", "schema", "migration", "rollout",
    "canary", "rollback", "dashboard", "alert", "threshold", "signal",
    "metric", "histogram", "percentile", "aggregate", "ingest", "pipeline",
    "dataset", "lineage", "provenance", "freshness", "retention", "compaction",
)

SENTENCE_STARTERS: tuple[str, ...] = (
    "Operators should",
    "The service must",
    "During an incident the team will",
    "Under sustained load the system may",
    "To keep the catalog healthy we",
    "Downstream consumers expect the pipeline to",
    "When capacity is tight the platform can",
    "For steady-state operation the runbook recommends that we",
)

SENTENCE_VERBS: tuple[str, ...] = (
    "monitor", "rebalance", "throttle", "reindex", "promote", "drain",
    "reconcile", "checkpoint", "compact", "replicate", "validate", "escalate",
)


def rel_path(index: int) -> str:
    """Bundle-relative Markdown path for a concept, purely from its index."""
    category = CATEGORIES[index % len(CATEGORIES)]
    bucket = index // BUCKET_SIZE
    return f"{category}/{bucket:04d}/c{index:06d}.md"


def concept_id(index: int) -> str:
    """OKF concept ID (path without the ``.md`` suffix) for an index."""
    return rel_path(index)[: -len(".md")]


def link_targets(index: int, count: int) -> list[int]:
    """Deterministic, self-excluding set of existing target indices."""
    candidates = [
        (index + 1) % count,
        (index * 7 + 13) % count,
        (index * 31 + 7) % count,
    ]
    seen: list[int] = []
    for candidate in candidates:
        if candidate != index and candidate not in seen:
            seen.append(candidate)
    return seen


def build_tags(index: int, rng: random.Random) -> list[str]:
    """2-4 tags; the selective tag lands on a fixed fraction of concepts."""
    tags: list[str] = []
    if index % SELECTIVE_TAG_STRIDE == 0:
        tags.append(SELECTIVE_TAG)
    extra = rng.sample(TAG_VOCAB, rng.randint(2, 3))
    for tag in extra:
        if tag not in tags:
            tags.append(tag)
    return tags


def build_sentence(rng: random.Random) -> str:
    """One prose sentence assembled from the technical vocabulary."""
    starter = rng.choice(SENTENCE_STARTERS)
    verb = rng.choice(SENTENCE_VERBS)
    nouns = rng.sample(BODY_VOCAB, rng.randint(3, 6))
    clause = ", ".join(nouns[:-1]) + f" and {nouns[-1]}" if len(nouns) > 1 else nouns[0]
    return f"{starter} {verb} the {clause}."


def build_body(
    index: int,
    title: str,
    targets: list[int],
    rng: random.Random,
) -> str:
    """A realistic markdown body of a few hundred tokens with links."""
    category_index = index % len(CATEGORIES)
    paragraphs: list[str] = []

    intro = " ".join(build_sentence(rng) for _ in range(3))
    paragraphs.append(intro)

    # Plant the FTS terms together in service/runbook bodies so the full-text
    # query has a deterministic, non-empty result set.
    if category_index in FTS_CATEGORY_INDEXES:
        paragraphs.append(
            "Streaming replication keeps a hot standby ready, and the failover "
            "procedure promotes that standby when the primary is lost. "
            + build_sentence(rng)
        )

    for _ in range(rng.randint(3, 5)):
        paragraphs.append(" ".join(build_sentence(rng) for _ in range(rng.randint(3, 5))))

    # Resolvable internal links (root-relative, so they resolve regardless of
    # the source document's own directory) plus one deliberately unresolved
    # internal link on a small fraction, exercising the resolved/unresolved
    # distinction the link projection records.
    related = ["## Related concepts", ""]
    for target in targets:
        related.append(f"- See [{concept_id(target)}](/{rel_path(target)}) for related detail.")
    if index % 17 == 0:
        related.append("- A [pending capacity note](/pending/not-yet-authored.md) is not written yet.")
    paragraphs.append("\n".join(related))

    # Exactly one external link per file (never a graph edge).
    paragraphs.append(
        "## External reference\n\n"
        f"Upstream documentation: [PostgreSQL manual](https://www.postgresql.org/docs/current/index-{index % 97}.html)."
    )

    return f"# {title}\n\n" + "\n\n".join(paragraphs) + "\n"


def frontmatter_lines(index: int, rng: random.Random, tags: list[str]) -> list[str]:
    """YAML frontmatter as ``key: <json>`` lines (JSON is a valid YAML subset)."""
    concept_type = TYPES[index % len(TYPES)]
    title = f"{concept_type} {index:06d}"
    description = (
        f"Operational {concept_type.lower()} reference number {index} in the "
        "synthetic OKF benchmark catalog."
    )

    fields: list[tuple[str, object]] = [
        ("type", concept_type),
        ("title", title),
        ("description", description),
        ("tags", tags),
    ]

    # Sparse provenance/trust/lifecycle frontmatter on a fixed fraction.
    if index % PROVENANCE_MODULUS < PROVENANCE_THRESHOLD:
        fields.append(("status", rng.choice(["stable", "beta", "deprecated"])))
        fields.append((
            "generated",
            {"by": f"catalog-agent/{1 + index % 3}.{index % 10}", "at": "2026-07-01T12:00:00Z"},
        ))
        if index % 2 == 0:
            fields.append((
                "verified",
                [{"by": "process:automated-drill"}, {"by": "human:oncall-lead"}],
            ))
            fields.append(("verification_method", "quarterly-drill"))
        else:
            fields.append(("verified", bool(index % 4)))
        fields.append((
            "sources",
            [{"id": f"source-{index % 500}", "url": f"https://example.test/source/{index % 500}"}],
        ))

    return [f"{key}: {json.dumps(value)}" for key, value in fields]


def render_concept(index: int, count: int, seed: int) -> tuple[str, str]:
    """Return ``(relative_path, file_contents)`` for one concept."""
    rng = random.Random((seed << 20) ^ index)
    tags = build_tags(index, rng)
    concept_type = TYPES[index % len(TYPES)]
    title = f"{concept_type} {index:06d}"
    targets = link_targets(index, count)

    body = build_body(index, title, targets, rng)
    frontmatter = "\n".join(frontmatter_lines(index, rng, tags))
    contents = f"---\n{frontmatter}\n---\n\n{body}"
    return rel_path(index), contents


def build_manifest(count: int, seed: int) -> dict[str, object]:
    """Query-driving facts derived deterministically from the corpus shape."""
    type_index = TYPES.index(FILTER_TYPE)
    type_expected = sum(1 for i in range(count) if i % len(TYPES) == type_index)
    tag_expected = sum(1 for i in range(count) if i % SELECTIVE_TAG_STRIDE == 0)
    fts_expected = sum(
        1 for i in range(count) if (i % len(CATEGORIES)) in FTS_CATEGORY_INDEXES
    )
    return {
        "count": count,
        "seed": seed,
        "seed_concept_id": concept_id(0),
        "type_value": FILTER_TYPE,
        "type_expected_count": type_expected,
        "tag_value": SELECTIVE_TAG,
        "tag_expected_count": tag_expected,
        "fts_query": FTS_QUERY,
        "fts_expected_count": fts_expected,
    }


def generate(count: int, out_dir: Path, seed: int, manifest_path: Path) -> dict[str, object]:
    """Write the whole bundle and its manifest; return the manifest dict."""
    written = 0
    made_dirs: set[Path] = set()
    for index in range(count):
        relative, contents = render_concept(index, count, seed)
        target = out_dir / relative
        parent = target.parent
        if parent not in made_dirs:
            parent.mkdir(parents=True, exist_ok=True)
            made_dirs.add(parent)
        target.write_text(contents, encoding="utf-8")
        written += 1
        if written % 2000 == 0:
            print(f"  ... wrote {written}/{count} concepts", file=sys.stderr)

    manifest = build_manifest(count, seed)
    manifest["files_written"] = written
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    return manifest


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Generate a deterministic OKF benchmark corpus.")
    parser.add_argument("--count", type=int, default=12000, help="Number of concepts to generate.")
    parser.add_argument("--out", type=Path, required=True, help="Bundle root output directory.")
    parser.add_argument("--seed", type=int, default=1337, help="Deterministic RNG seed.")
    parser.add_argument(
        "--manifest",
        type=Path,
        default=None,
        help="Manifest JSON path (default: <out>.manifest.json alongside the bundle).",
    )
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    if args.count < 1:
        print("--count must be positive", file=sys.stderr)
        return 2

    out_dir: Path = args.out
    out_dir.mkdir(parents=True, exist_ok=True)
    manifest_path: Path = args.manifest or out_dir.parent / (out_dir.name + ".manifest.json")

    print(
        f"Generating {args.count} concepts into {out_dir} (seed={args.seed})",
        file=sys.stderr,
    )
    manifest = generate(args.count, out_dir, args.seed, manifest_path)
    print(
        f"Done: {manifest['files_written']} files; manifest at {manifest_path}",
        file=sys.stderr,
    )
    # Echo the manifest on stdout so callers can capture it directly too.
    print(json.dumps(manifest))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
