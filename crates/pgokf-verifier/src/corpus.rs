//! Reproducible benchmark corpus, retaining the original integer-seeded MT19937 stream.
use serde::{Deserialize, Serialize};
use std::{fmt::Write, path::Path};

const CATEGORIES: [&str; 8] = [
    "services",
    "runbooks",
    "references",
    "dashboards",
    "guides",
    "policies",
    "datasets",
    "pipelines",
];
const TYPES: [&str; 8] = [
    "Service",
    "Runbook",
    "Reference",
    "Dashboard",
    "Guide",
    "Policy",
    "Dataset",
    "Pipeline",
];
const TAGS: [&str; 20] = [
    "database",
    "observability",
    "incident-response",
    "oncall",
    "storage",
    "networking",
    "security",
    "capacity",
    "latency",
    "throughput",
    "backup",
    "replication",
    "indexing",
    "analytics",
    "streaming",
    "batch",
    "sre",
    "platform",
    "reliability",
    "compliance",
];
const NOUNS: [&str; 61] = [
    "replication",
    "failover",
    "latency",
    "throughput",
    "partition",
    "vacuum",
    "checkpoint",
    "index",
    "buffer",
    "connection",
    "pool",
    "saturation",
    "standby",
    "primary",
    "recovery",
    "archive",
    "segment",
    "transaction",
    "commit",
    "rollback",
    "isolation",
    "snapshot",
    "cluster",
    "node",
    "shard",
    "query",
    "planner",
    "statistics",
    "cardinality",
    "selectivity",
    "cache",
    "eviction",
    "workload",
    "concurrency",
    "contention",
    "deadlock",
    "lock",
    "sequence",
    "trigger",
    "constraint",
    "schema",
    "migration",
    "rollout",
    "canary",
    "rollback",
    "dashboard",
    "alert",
    "threshold",
    "signal",
    "metric",
    "histogram",
    "percentile",
    "aggregate",
    "ingest",
    "pipeline",
    "dataset",
    "lineage",
    "provenance",
    "freshness",
    "retention",
    "compaction",
];
const STARTERS: [&str; 8] = [
    "Operators should",
    "The service must",
    "During an incident the team will",
    "Under sustained load the system may",
    "To keep the catalog healthy we",
    "Downstream consumers expect the pipeline to",
    "When capacity is tight the platform can",
    "For steady-state operation the runbook recommends that we",
];
const VERBS: [&str; 12] = [
    "monitor",
    "rebalance",
    "throttle",
    "reindex",
    "promote",
    "drain",
    "reconcile",
    "checkpoint",
    "compact",
    "replicate",
    "validate",
    "escalate",
];

struct Random {
    state: [u32; 624],
    index: usize,
}
impl Random {
    fn new(seed: u128) -> Self {
        let mut keys = Vec::new();
        let mut seed = seed;
        loop {
            keys.push(u32::try_from(seed & u128::from(u32::MAX)).expect("masked seed word"));
            seed >>= 32;
            if seed == 0 {
                break;
            }
        }
        let mut r = Self {
            state: [0; 624],
            index: 624,
        };
        r.state[0] = 19_650_218;
        for i in 1..624 {
            r.state[i] = 1_812_433_253u32
                .wrapping_mul(r.state[i - 1] ^ (r.state[i - 1] >> 30))
                .wrapping_add(u32::try_from(i).expect("MT state index"));
        }
        let (mut i, mut j) = (1, 0);
        for _ in 0..624.max(keys.len()) {
            r.state[i] = (r.state[i]
                ^ (r.state[i - 1] ^ (r.state[i - 1] >> 30)).wrapping_mul(1_664_525))
            .wrapping_add(keys[j])
            .wrapping_add(u32::try_from(j).expect("MT key index"));
            i += 1;
            j += 1;
            if i >= 624 {
                r.state[0] = r.state[623];
                i = 1;
            }
            if j >= keys.len() {
                j = 0;
            }
        }
        for _ in 0..623 {
            r.state[i] = (r.state[i]
                ^ (r.state[i - 1] ^ (r.state[i - 1] >> 30)).wrapping_mul(1_566_083_941))
            .wrapping_sub(u32::try_from(i).expect("MT state index"));
            i += 1;
            if i >= 624 {
                r.state[0] = r.state[623];
                i = 1;
            }
        }
        r.state[0] = 0x8000_0000;
        r
    }
    fn word(&mut self) -> u32 {
        if self.index == 624 {
            for i in 0..624 {
                let y = (self.state[i] & 0x8000_0000) | (self.state[(i + 1) % 624] & 0x7fff_ffff);
                self.state[i] = self.state[(i + 397) % 624]
                    ^ (y >> 1)
                    ^ if y & 1 == 0 { 0 } else { 0x9908_b0df };
            }
            self.index = 0;
        }
        let mut y = self.state[self.index];
        self.index += 1;
        y ^= y >> 11;
        y ^= (y << 7) & 0x9d2c_5680;
        y ^= (y << 15) & 0xefc6_0000;
        y ^ (y >> 18)
    }
    fn below(&mut self, n: usize) -> usize {
        let bits = usize::BITS - n.leading_zeros();
        loop {
            let value = (self.word() >> (32 - bits)) as usize;
            if value < n {
                return value;
            }
        }
    }
    fn choose<'a>(&mut self, values: &[&'a str]) -> &'a str {
        values[self.below(values.len())]
    }
    fn sample<'a>(&mut self, values: &[&'a str], n: usize) -> Vec<&'a str> {
        let mut selected = Vec::new();
        let threshold = if n > 5 { 21 + 64 } else { 21 };
        if values.len() <= threshold {
            let mut pool = values.to_vec();
            for i in 0..n {
                let j = self.below(values.len() - i);
                selected.push(pool[j]);
                pool[j] = pool[values.len() - i - 1];
            }
        } else {
            let mut indexes = Vec::new();
            for _ in 0..n {
                let mut j = self.below(values.len());
                while indexes.contains(&j) {
                    j = self.below(values.len());
                }
                indexes.push(j);
                selected.push(values[j]);
            }
        }
        selected
    }
    fn sentence(&mut self) -> String {
        let start = self.choose(&STARTERS);
        let verb = self.choose(&VERBS);
        let n = 3 + self.below(4);
        let nouns = self.sample(&NOUNS, n);
        format!(
            "{start} {verb} the {} and {}.",
            nouns[..n - 1].join(", "),
            nouns[n - 1]
        )
    }
    fn paragraph(&mut self, n: usize) -> String {
        (0..n)
            .map(|_| self.sentence())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[must_use]
pub fn relative_path(index: usize) -> String {
    format!(
        "{}/{:04}/c{index:06}.md",
        CATEGORIES[index % 8],
        index / 250
    )
}
#[must_use]
pub fn concept_id(index: usize) -> String {
    relative_path(index).trim_end_matches(".md").into()
}
#[must_use]
pub fn link_targets(index: usize, count: usize) -> Vec<usize> {
    let mut result = Vec::new();
    if count == 0 || index >= count || count > 10_000_000 {
        return result;
    }
    for candidate in [
        (index + 1) % count,
        (index * 7 + 13) % count,
        (index * 31 + 7) % count,
    ] {
        if candidate != index && !result.contains(&candidate) {
            result.push(candidate);
        }
    }
    result
}

/// Render a document with byte-identical legacy seeded content.
/// # Panics
/// String serialization is infallible for the generated UTF-8 fields.
/// # Errors
/// Rejects invalid corpus bounds or arguments and propagates output I/O errors.
pub fn render(index: usize, count: usize, seed: i64) -> Result<(String, String), String> {
    if count == 0 || index >= count || count > 10_000_000 {
        return Err("invalid corpus index/count".into());
    }
    let mixed = (i128::from(seed) << 20) ^ index as i128;
    let mut rng = Random::new(mixed.unsigned_abs());
    let mut tags = Vec::new();
    if index.is_multiple_of(3) {
        tags.push("postgres");
    }
    let n = 2 + rng.below(2);
    tags.extend(rng.sample(&TAGS, n));
    let kind = TYPES[index % 8];
    let title = format!("{kind} {index:06}");
    let mut paragraphs = vec![rng.paragraph(3)];
    if index % 8 < 2 {
        paragraphs.push(format!("Streaming replication keeps a hot standby ready, and the failover procedure promotes that standby when the primary is lost. {}", rng.sentence()));
    }
    let n = 3 + rng.below(3);
    for _ in 0..n {
        let m = 3 + rng.below(3);
        paragraphs.push(rng.paragraph(m));
    }
    let mut related = String::from("## Related concepts\n");
    for target in link_targets(index, count) {
        write!(
            related,
            "\n- See [{}](/{}) for related detail.",
            concept_id(target),
            relative_path(target)
        )
        .map_err(|e| e.to_string())?;
    }
    if index.is_multiple_of(17) {
        related.push_str(
            "\n- A [pending capacity note](/pending/not-yet-authored.md) is not written yet.",
        );
    }
    paragraphs.push(related);
    paragraphs.push(format!("## External reference\n\nUpstream documentation: [PostgreSQL manual](https://www.postgresql.org/docs/current/index-{}.html).", index%97));
    let quote = |s: &str| serde_json::to_string(s).expect("string serialization");
    let mut fm = vec![
        format!("type: {}", quote(kind)),
        format!("title: {}", quote(&title)),
        format!(
            "description: {}",
            quote(&format!(
                "Operational {} reference number {index} in the synthetic OKF benchmark catalog.",
                kind.to_lowercase()
            ))
        ),
        format!(
            "tags: [{}]",
            tags.iter().map(|s| quote(s)).collect::<Vec<_>>().join(", ")
        ),
    ];
    if index % 10 < 3 {
        fm.push(format!(
            "status: {}",
            quote(rng.choose(&["draft", "stable", "deprecated"]))
        ));
        fm.push("stale_after: \"2027-01-01T00:00:00Z\"".into());
        fm.push(format!(
            "generated: {{\"by\": \"catalog-agent/{}.{}\", \"at\": \"2026-07-01T12:00:00Z\"}}",
            1 + index % 3,
            index % 10
        ));
        if index.is_multiple_of(2) {
            fm.push("verified: [{\"by\": \"process:automated-drill\", \"at\": \"2026-07-02T02:00:00Z\"}, {\"by\": \"human:oncall-lead\", \"at\": \"2026-07-03T09:30:00Z\"}]".into());
        }
        fm.push(format!("sources: [{{\"id\": \"source-{}\", \"resource\": \"https://example.test/source/{}\", \"author\": \"process:warehouse-catalog\", \"usage_count\": {}, \"last_modified\": \"2026-06-30T23:00:00Z\"}}]", index%500, index%500, 100+index%900));
    }
    Ok((
        relative_path(index),
        format!(
            "---\n{}\n---\n\n# {title}\n\n{}\n",
            fm.join("\n"),
            paragraphs.join("\n\n")
        ),
    ))
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub count: usize,
    pub seed: i64,
    pub seed_concept_id: String,
    pub type_value: String,
    pub type_expected_count: usize,
    pub tag_value: String,
    pub tag_expected_count: usize,
    pub fts_query: String,
    pub fts_expected_count: usize,
    pub files_written: usize,
}
impl Manifest {
    /// # Errors
    /// Rejects invalid corpus bounds or arguments and propagates output I/O errors.
    pub fn new(count: usize, seed: i64) -> Result<Self, String> {
        if count == 0 || count > 10_000_000 {
            return Err("count must be between 1 and 10000000".into());
        }
        Ok(Self {
            count,
            seed,
            seed_concept_id: concept_id(0),
            type_value: "Runbook".into(),
            type_expected_count: count / 8 + usize::from(count % 8 > 1),
            tag_value: "postgres".into(),
            tag_expected_count: count.div_ceil(3),
            fts_query: "replication failover".into(),
            fts_expected_count: count / 8 * 2 + (count % 8).min(2),
            files_written: count,
        })
    }
}

/// # Errors
/// Rejects invalid corpus bounds or arguments and propagates output I/O errors.
pub fn generate(
    count: usize,
    seed: i64,
    out: &Path,
    manifest_path: &Path,
) -> Result<Manifest, String> {
    let manifest = Manifest::new(count, seed)?;
    for index in 0..count {
        let (path, content) = render(index, count, seed)?;
        let path = out.join(path);
        std::fs::create_dir_all(path.parent().ok_or("missing parent")?)
            .map_err(|e| e.to_string())?;
        std::fs::write(path, content).map_err(|e| e.to_string())?;
    }
    std::fs::write(
        manifest_path,
        format!(
            "{}\n",
            serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?
        ),
    )
    .map_err(|e| e.to_string())?;
    Ok(manifest)
}

/// # Errors
/// Rejects invalid corpus bounds or arguments and propagates output I/O errors.
pub fn cli(args: &[String]) -> Result<(), String> {
    let (mut count, mut seed, mut out, mut manifest) = (12000, 1337, None, None);
    if !args.len().is_multiple_of(2) {
        return Err("corpus requires option/value pairs".into());
    }
    for pair in args.chunks_exact(2) {
        match pair[0].as_str() {
            "--count" => count = pair[1].parse().map_err(|_| "invalid count")?,
            "--seed" => seed = pair[1].parse().map_err(|_| "invalid seed")?,
            "--out" => out = Some(std::path::PathBuf::from(&pair[1])),
            "--manifest" => manifest = Some(std::path::PathBuf::from(&pair[1])),
            _ => return Err("unknown corpus option".into()),
        }
    }
    let out = out.ok_or("--out required")?;
    let manifest = manifest.unwrap_or_else(|| {
        out.with_file_name(format!(
            "{}.manifest.json",
            out.file_name().unwrap_or_default().to_string_lossy()
        ))
    });
    println!(
        "{}",
        serde_json::to_string(&generate(count, seed, &out, &manifest)?)
            .map_err(|e| e.to_string())?
    );
    Ok(())
}

/// Count the actual benchmark Markdown files and enforce its minimum dataset.
/// # Errors
/// Unreadable directories or an undersized corpus fail the benchmark gate.
pub fn benchmark_check(root: &Path) -> Result<usize, String> {
    fn count(root: &Path) -> Result<usize, String> {
        let mut total = 0;
        for entry in std::fs::read_dir(root).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let kind = entry.file_type().map_err(|e| e.to_string())?;
            if kind.is_dir() {
                total += count(&entry.path())?;
            } else if kind.is_file() && entry.path().extension().is_some_and(|s| s == "md") {
                total += 1;
            }
        }
        Ok(total)
    }
    let files = count(root)?;
    benchmark_minimum(files)?;
    Ok(files)
}

fn benchmark_minimum(files: usize) -> Result<(), String> {
    if files < 10_000 {
        return Err(format!("corpus has {files} files (< 10000)"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    #[test]
    fn benchmark_minimum_is_required() {
        assert!(
            benchmark_minimum(9_999).is_err(),
            "undersized benchmark accepted"
        );
        benchmark_minimum(10_000).unwrap();
        benchmark_minimum(12_000).unwrap();
    }
    #[test]
    fn benchmark_counts_actual_markdown() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("nested")).unwrap();
        std::fs::write(tmp.path().join("one.md"), "one").unwrap();
        std::fs::write(tmp.path().join("nested/two.md"), "two").unwrap();
        std::fs::write(tmp.path().join("manifest.json"), "{}").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(tmp.path().join("one.md"), tmp.path().join("link.md")).unwrap();
        let error = benchmark_check(tmp.path()).unwrap_err();
        assert_eq!(
            error, "corpus has 2 files (< 10000)",
            "benchmark Markdown enumeration changed"
        );
        assert!(benchmark_check(&tmp.path().join("missing")).is_err());
    }
    #[test]
    fn legacy_bytes() {
        for (index, count, seed, expected) in [
            (
                0,
                1,
                0,
                "37f9cdfce844c7632e44bee941248ee897a95c57dddfd5529c2582a0c9928bd3",
            ),
            (
                0,
                257,
                1337,
                "5b08cbaf436eee1347fd9d0d9a85d1c55f31509046352605871d0a033211c6e8",
            ),
            (
                17,
                257,
                1337,
                "4b56de8da234b49452cbce0864156137593bd67ffcc55739cad4a057e10e49e0",
            ),
            (
                250,
                257,
                1337,
                "6a46fa0c5c1d5f9466ab73e22f8400fc47bed7557987cc9888f2238cf71a2e66",
            ),
            (
                256,
                257,
                1337,
                "05163b2d1984c5d6913c3334a0fa6c79db03ed4407a73773c5fe40e2bea615d6",
            ),
            (
                4,
                19,
                -7,
                "56b1a95d772add63ef0be2da1cbd4cb61438456de1a9c120914c9f402ac748fa",
            ),
            (
                18,
                19,
                i64::MAX,
                "206f6c85be53db7484409ab1281a07fe32fe1decf1da9ffb2370853128c63524",
            ),
        ] {
            assert_eq!(
                Sha256::digest(render(index, count, seed).unwrap().1)
                    .iter()
                    .fold(String::new(), |mut s, b| {
                        write!(s, "{b:02x}").unwrap();
                        s
                    }),
                expected
            );
        }
    }
    #[test]
    fn legacy_paths() {
        for (index, expected) in [
            (0, "services/0000/c000000.md"),
            (17, "runbooks/0000/c000017.md"),
            (250, "references/0001/c000250.md"),
            (256, "services/0001/c000256.md"),
        ] {
            assert_eq!(render(index, 257, 1337).unwrap().0, expected);
        }
    }
    #[test]
    fn links_and_manifest_counts() {
        for count in [1, 2, 3, 8, 17, 251, 12000] {
            let m = Manifest::new(count, 1337).unwrap();
            assert_eq!(
                m.type_expected_count,
                (0..count).filter(|i| i % 8 == 1).count()
            );
            assert_eq!(
                m.tag_expected_count,
                (0..count).filter(|i| i % 3 == 0).count()
            );
            assert_eq!(
                m.fts_expected_count,
                (0..count).filter(|i| i % 8 < 2).count()
            );
            for i in 0..count {
                let links = link_targets(i, count);
                assert!(links.iter().all(|j| *j < count && *j != i));
                assert_eq!(
                    links.len(),
                    links
                        .iter()
                        .collect::<std::collections::BTreeSet<_>>()
                        .len()
                );
            }
        }
        assert!(Manifest::new(0, 0).is_err());
        assert!(render(1, 1, 0).is_err());
    }
    #[test]
    fn on_disk_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("manifest.json");
        let m = generate(2, 0, &tmp.path().join("bundle"), &path).unwrap();
        let parsed: Manifest = crate::json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(m, parsed);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("bundle").join(relative_path(1))).unwrap(),
            render(1, 2, 0).unwrap().1
        );
    }
}
