#!/usr/bin/env bash
#
# run_bench.sh - end-to-end YAML vs PostgreSQL vs Parquet benchmark for the OKF
# catalog. Produces REAL, measured numbers on the host it runs on.
#
# Three legs:
#   (a) YAML     : parse every .md through okf_parser (cargo example parse_all),
#                  plus the corpus on-disk size.
#   (b) Postgres : install the pgokf extension, spin up a scratch PG18 cluster,
#                  register the corpus (bulk-load timing), then time a type
#                  filter, a GIN tag filter, native FTS, and a graph traversal;
#                  capture catalog table+index disk footprint.
#   (c) Parquet  : export the catalog to Parquet (timing + file sizes), then a
#                  filtered read of concepts.parquet.
#
# Everything scratch lives under /tmp/claude-1000 and is removed on exit (the
# Parquet reader build is cached there to keep re-runs fast). The installed
# extension is left in place (installing it is part of the benchmark).
#
# Usage:  scripts/bench/run_bench.sh
# Env overrides: OKF_BENCH_COUNT (default 12000), OKF_BENCH_SEED (1337),
#                OKF_BENCH_PORT (54329), PGOKF_PG_CONFIG (PG18 pg_config),
#                OKF_BENCH_QUERY_REPEATS (6).

set -euo pipefail

# --------------------------------------------------------------------------
# Configuration
# --------------------------------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

PG_CONFIG="${PGOKF_PG_CONFIG:-/usr/lib/postgresql/18/bin/pg_config}"
CORPUS_COUNT="${OKF_BENCH_COUNT:-12000}"
SEED="${OKF_BENCH_SEED:-1337}"
PGPORT="${OKF_BENCH_PORT:-54329}"
QUERY_REPEATS="${OKF_BENCH_QUERY_REPEATS:-6}"
DB="okfbench"

WORK_BASE="/tmp/claude-1000"
RUN_ID="okf-bench-$$"
WORK="$WORK_BASE/$RUN_ID"
SOCK="$WORK_BASE/okf-bs-$$"           # short socket dir (< 107-byte limit)
CORPUS="$WORK/corpus"
PGDATA="$WORK/pgdata"
EXPORT_DIR="$WORK/parquet"
MANIFEST="$WORK/corpus.manifest.json"
PGLOG="$WORK/pg.log"
PQREADER_DIR="$WORK_BASE/okf-bench-pqreader"   # cached across runs
RESULTS_FILE="$WORK_BASE/okf-bench-last-results.txt"

PG_BINDIR="$("$PG_CONFIG" --bindir)"
INITDB="$PG_BINDIR/initdb"
PG_CTL="$PG_BINDIR/pg_ctl"
PSQL_BIN="$PG_BINDIR/psql"
PSQL=("$PSQL_BIN" -X -h "$SOCK" -p "$PGPORT" -v ON_ERROR_STOP=1)

mkdir -p "$WORK" "$SOCK"

# --------------------------------------------------------------------------
# Cleanup (trap EXIT): stop the scratch cluster and remove scratch data.
# --------------------------------------------------------------------------
cleanup() {
    local status=$?
    if [[ -d "$PGDATA" ]] && "$PG_CTL" -D "$PGDATA" status >/dev/null 2>&1; then
        "$PG_CTL" -D "$PGDATA" -m immediate -w stop >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK" "$SOCK"
    exit "$status"
}
trap cleanup EXIT

log()  { printf '\n=== %s ===\n' "$*"; }
note() { printf '    %s\n' "$*"; }

# --------------------------------------------------------------------------
# Small numeric helpers
# --------------------------------------------------------------------------
median() {  # numbers on stdin -> median on stdout
    sort -g | awk '{a[NR]=$1} END{
        if (NR==0){print "0"; exit}
        if (NR%2){printf "%.3f\n", a[(NR+1)/2]}
        else     {printf "%.3f\n", (a[NR/2]+a[NR/2+1])/2}
    }'
}

scalar() {  # scalar() <sql> -> trimmed single value from $DB
    "${PSQL[@]}" -d "$DB" -t -A -c "$1" | tr -d '[:space:]'
}

# Time one SQL statement server-side via psql \timing; echo milliseconds.
timed_ms() {
    local sql="$1" out
    out="$("${PSQL[@]}" -d "$DB" <<SQL 2>&1
\timing on
$sql
SQL
)"
    printf '%s\n' "$out" | awk '/^Time:/{v=$2} END{print v+0}'
}

# Run a query QUERY_REPEATS times (first run is warm-up, discarded); echo the
# median milliseconds of the remaining runs.
bench_query() {
    local sql="$1" i
    { for ((i = 1; i <= QUERY_REPEATS; i++)); do
          local ms; ms="$(timed_ms "$sql")"
          [[ $i -gt 1 ]] && echo "$ms"
      done
    } | median
}

write_if_changed() {  # write_if_changed <path> <<<content
    local path="$1" tmp
    tmp="$(mktemp)"
    cat > "$tmp"
    if [[ ! -f "$path" ]] || ! cmp -s "$tmp" "$path"; then
        mv "$tmp" "$path"
    else
        rm -f "$tmp"
    fi
}

# --------------------------------------------------------------------------
# Environment banner
# --------------------------------------------------------------------------
HOST_NPROC="$(nproc)"
HOST_MEM="$(free -h | awk '/^Mem:/{print $2}')"
PG_VERSION="$("$PG_CONFIG" --version)"
RUSTC_VERSION="$(rustc --version)"

log "Host / environment"
note "host cores      : $HOST_NPROC"
note "host memory     : $HOST_MEM"
note "postgres        : $PG_VERSION"
note "rustc           : $RUSTC_VERSION"
note "corpus concepts : $CORPUS_COUNT (seed $SEED)"
note "repo HEAD        : $(cd "$REPO_ROOT" && git rev-parse --short HEAD 2>/dev/null || echo unknown)"

# --------------------------------------------------------------------------
# Corpus generation
# --------------------------------------------------------------------------
log "Generating corpus"
python3 "$SCRIPT_DIR/generate_corpus.py" \
    --count "$CORPUS_COUNT" --out "$CORPUS" --seed "$SEED" \
    --manifest "$MANIFEST" >/dev/null
FILE_COUNT="$(find "$CORPUS" -name '*.md' -type f | wc -l | tr -d ' ')"
note "markdown files  : $FILE_COUNT"
if (( FILE_COUNT < 10000 )); then
    echo "ERROR: corpus has $FILE_COUNT files (< 10000)" >&2
    exit 1
fi

# Read the query-driving facts from the manifest.
json_get() { python3 -c "import json,sys;print(json.load(open('$MANIFEST'))['$1'])"; }
SEED_CONCEPT="$(json_get seed_concept_id)"
TYPE_VALUE="$(json_get type_value)"
TYPE_EXPECTED="$(json_get type_expected_count)"
TAG_VALUE="$(json_get tag_value)"
TAG_EXPECTED="$(json_get tag_expected_count)"
FTS_QUERY="$(json_get fts_query)"

CORPUS_BYTES="$(du -sb "$CORPUS" | awk '{print $1}')"
CORPUS_HUMAN="$(du -sh "$CORPUS" | awk '{print $1}')"
note "corpus on disk  : $CORPUS_HUMAN ($CORPUS_BYTES bytes)"

# ==========================================================================
# LEG (a): YAML parse-all
# ==========================================================================
log "Leg A: YAML parse-all (okf_parser)"
cargo build --release --example parse_all -p okf-parser --manifest-path "$REPO_ROOT/Cargo.toml" >/dev/null 2>&1
PARSE_BIN="$REPO_ROOT/target/release/examples/parse_all"
PARSE_OUT="$("$PARSE_BIN" "$CORPUS")"
printf '%s\n' "$PARSE_OUT" | sed 's/^/    /'
yaml_get() { printf '%s\n' "$PARSE_OUT" | awk -v k="$1" '$1==k{print $2}'; }
YAML_ELAPSED_S="$(yaml_get elapsed_seconds)"
YAML_ELAPSED_MS="$(awk -v s="$YAML_ELAPSED_S" 'BEGIN{printf "%.1f", s*1000}')"
YAML_FILES_PER_SEC="$(yaml_get files_per_sec)"
YAML_PEAK_RSS="$(yaml_get peak_rss_mib)"

# ==========================================================================
# LEG (b): PostgreSQL
# ==========================================================================
log "Leg B: PostgreSQL - install extension"
( cd "$REPO_ROOT/crates/extension" &&
  cargo pgrx install --no-default-features --features pg18 \
      --pg-config "$PG_CONFIG" --sudo --release >/dev/null 2>&1 )
note "pgokf installed into $($PG_CONFIG --pkglibdir)"

log "Leg B: initdb + start scratch cluster"
"$INITDB" -D "$PGDATA" --locale=C.UTF-8 -A trust >/dev/null 2>&1
"$PG_CTL" -D "$PGDATA" -l "$PGLOG" \
    -o "-p $PGPORT -k $SOCK -c listen_addresses=''" -w start >/dev/null
note "cluster started on port $PGPORT (socket $SOCK)"

"${PSQL[@]}" -d postgres -c "CREATE DATABASE $DB" >/dev/null
"${PSQL[@]}" -d "$DB" -c "CREATE EXTENSION pgokf" >/dev/null
note "extension version: $(scalar "SELECT extversion FROM pg_extension WHERE extname='pgokf'")"

log "Leg B: register bundle (bulk load)"
REG_OUT="$("${PSQL[@]}" -d "$DB" <<SQL 2>&1
\timing on
SELECT added, updated, removed, unchanged, total FROM pgokf.register_bundle('$CORPUS');
SQL
)"
printf '%s\n' "$REG_OUT" | sed 's/^/    /'
REGISTER_MS="$(printf '%s\n' "$REG_OUT" | awk '/^Time:/{v=$2} END{print v+0}')"
BUNDLE_ID="$(scalar "SELECT id FROM pgokf.bundles ORDER BY id LIMIT 1")"
CONCEPT_ROWS="$(scalar "SELECT count(*) FROM pgokf.concepts")"
LINK_ROWS="$(scalar "SELECT count(*) FROM pgokf.links")"
PROV_ROWS="$(scalar "SELECT count(*) FROM pgokf.concept_provenance")"
META_ROWS="$(scalar "SELECT count(*) FROM pgokf.concept_metadata")"
note "bundle_id=$BUNDLE_ID concepts=$CONCEPT_ROWS links=$LINK_ROWS provenance=$PROV_ROWS metadata=$META_ROWS"

# Make plans representative (a DBA would analyze after a bulk load).
"${PSQL[@]}" -d "$DB" -c "ANALYZE pgokf.concepts; ANALYZE pgokf.links; ANALYZE pgokf.concept_metadata; ANALYZE pgokf.concept_provenance" >/dev/null

log "Leg B: query timings (median of $((QUERY_REPEATS-1)) runs, ms)"
TYPE_ROWS="$(scalar "SELECT count(*) FROM pgokf.concepts WHERE type = '$TYPE_VALUE'")"
TYPE_MS="$(bench_query "SELECT count(*) FROM pgokf.concepts WHERE type = '$TYPE_VALUE';")"
note "type filter    (type='$TYPE_VALUE')       rows=$TYPE_ROWS (expected $TYPE_EXPECTED)  median=${TYPE_MS} ms"

TAG_ROWS="$(scalar "SELECT count(*) FROM pgokf.concepts WHERE tags @> ARRAY['$TAG_VALUE']")"
TAG_MS="$(bench_query "SELECT count(*) FROM pgokf.concepts WHERE tags @> ARRAY['$TAG_VALUE'];")"
note "tag filter GIN (tags @> '{$TAG_VALUE}')   rows=$TAG_ROWS (expected $TAG_EXPECTED)  median=${TAG_MS} ms"

FTS_ROWS="$(scalar "SELECT count(*) FROM pgokf.concept_search('$FTS_QUERY', NULL, 500)")"
FTS_MS="$(bench_query "SELECT count(*) FROM pgokf.concept_search('$FTS_QUERY', NULL, 500);")"
note "FTS concept_search('$FTS_QUERY')  rows=$FTS_ROWS  median=${FTS_MS} ms"

TRAV_ROWS="$(scalar "SELECT count(*) FROM pgokf.concept_neighbors('$SEED_CONCEPT', 3)")"
TRAV_MS="$(bench_query "SELECT count(*) FROM pgokf.concept_neighbors('$SEED_CONCEPT', 3);")"
note "traversal concept_neighbors('$SEED_CONCEPT', 3)  rows=$TRAV_ROWS  median=${TRAV_MS} ms"

log "Leg B: catalog disk footprint"
DISK_TABLE="$("${PSQL[@]}" -d "$DB" -c "
SELECT c.relname AS table,
       pg_size_pretty(pg_relation_size(c.oid))   AS heap,
       pg_size_pretty(pg_indexes_size(c.oid))    AS indexes,
       pg_size_pretty(pg_total_relation_size(c.oid)) AS total
FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = 'pgokf' AND c.relkind = 'r'
ORDER BY pg_total_relation_size(c.oid) DESC;")"
printf '%s\n' "$DISK_TABLE" | sed 's/^/    /'
PG_TOTAL_BYTES="$(scalar "SELECT COALESCE(sum(pg_total_relation_size(c.oid)),0)
FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
WHERE n.nspname='pgokf' AND c.relkind='r'")"
PG_TOTAL_HUMAN="$(scalar "SELECT pg_size_pretty(COALESCE(sum(pg_total_relation_size(c.oid)),0))
FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
WHERE n.nspname='pgokf' AND c.relkind='r'")"
note "catalog total (tables+indexes): $PG_TOTAL_HUMAN ($PG_TOTAL_BYTES bytes)"

# ==========================================================================
# LEG (c): Parquet
# ==========================================================================
log "Leg C: export_parquet"
mkdir -p "$EXPORT_DIR"
EXP_OUT="$("${PSQL[@]}" -d "$DB" <<SQL 2>&1
\timing on
SELECT concepts_rows, metadata_rows, links_rows, provenance_rows, bytes_written
FROM pgokf.export_parquet($BUNDLE_ID, '$EXPORT_DIR');
SQL
)"
printf '%s\n' "$EXP_OUT" | sed 's/^/    /'
EXPORT_MS="$(printf '%s\n' "$EXP_OUT" | awk '/^Time:/{v=$2} END{print v+0}')"
PARQUET_TOTAL_BYTES="$(du -sb "$EXPORT_DIR" | awk '{print $1}')"
PARQUET_TOTAL_HUMAN="$(du -sh "$EXPORT_DIR" | awk '{print $1}')"
log "Leg C: Parquet file sizes"
( cd "$EXPORT_DIR" && du -b -- *.parquet | sort -rn ) | sed 's/^/    /'
note "parquet total   : $PARQUET_TOTAL_HUMAN ($PARQUET_TOTAL_BYTES bytes)"
CONCEPTS_PARQUET="$EXPORT_DIR/concepts.parquet"

log "Leg C: Parquet filtered read (concepts.parquet)"
PARQUET_READ_STATUS="measured"
if ! command -v cargo >/dev/null 2>&1; then
    PARQUET_READ_STATUS="not measured (cargo unavailable)"
fi

if [[ "$PARQUET_READ_STATUS" == "measured" ]]; then
    mkdir -p "$PQREADER_DIR/src"
    write_if_changed "$PQREADER_DIR/Cargo.toml" <<'TOML'
[package]
name = "okf_bench_pqreader"
version = "0.0.0"
edition = "2021"
publish = false

[[bin]]
name = "pqread"
path = "src/main.rs"

[dependencies]
arrow = { version = "=59.2.0", default-features = false }
parquet = { version = "=59.2.0", default-features = false, features = ["arrow", "zstd"] }

[profile.release]
opt-level = 3
TOML
    write_if_changed "$PQREADER_DIR/src/main.rs" <<'RUST'
//! Throwaway filtered reader for the Parquet benchmark leg. Reads
//! concepts.parquet, times a full scan that counts rows whose `type` column
//! equals a target value, and prints rows/elapsed/peak-RSS.
use std::fs::File;
use std::time::Instant;

use arrow::array::{Array, StringArray};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

fn peak_rss_mib() -> Option<f64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let kib: f64 = rest.trim().trim_end_matches("kB").trim().parse().ok()?;
            return Some(kib / 1024.0);
        }
    }
    None
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: pqread <concepts.parquet> <type>");
    let target = args.next().expect("usage: pqread <concepts.parquet> <type>");

    let file = File::open(&path).expect("open parquet file");
    let builder = ParquetRecordBatchReaderBuilder::try_new(file).expect("open reader");
    let type_index = builder
        .schema()
        .index_of("type")
        .expect("concepts.parquet has a `type` column");
    let reader = builder.build().expect("build record batch reader");

    let start = Instant::now();
    let mut rows_total: u64 = 0;
    let mut rows_matched: u64 = 0;
    for batch in reader {
        let batch = batch.expect("read record batch");
        rows_total += batch.num_rows() as u64;
        let column = batch
            .column(type_index)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("type column is Utf8");
        for i in 0..column.len() {
            if column.is_valid(i) && column.value(i) == target {
                rows_matched += 1;
            }
        }
    }
    let elapsed = start.elapsed();

    println!("rows_total {rows_total}");
    println!("rows_matched {rows_matched}");
    println!("elapsed_seconds {:.4}", elapsed.as_secs_f64());
    match peak_rss_mib() {
        Some(mib) => println!("peak_rss_mib {mib:.1}"),
        None => println!("peak_rss_mib unavailable"),
    }
}
RUST
    if cargo build --release --manifest-path "$PQREADER_DIR/Cargo.toml" >/dev/null 2>&1; then
        PQREAD_OUT="$("$PQREADER_DIR/target/release/pqread" "$CONCEPTS_PARQUET" "$TYPE_VALUE")"
        printf '%s\n' "$PQREAD_OUT" | sed 's/^/    /'
        pq_get() { printf '%s\n' "$PQREAD_OUT" | awk -v k="$1" '$1==k{print $2}'; }
        PARQUET_READ_ROWS_TOTAL="$(pq_get rows_total)"
        PARQUET_READ_ROWS_MATCHED="$(pq_get rows_matched)"
        PARQUET_READ_S="$(pq_get elapsed_seconds)"
        PARQUET_READ_MS="$(awk -v s="$PARQUET_READ_S" 'BEGIN{printf "%.2f", s*1000}')"
    else
        PARQUET_READ_STATUS="not measured (pqreader build failed; no pyarrow/duckdb present)"
        note "$PARQUET_READ_STATUS"
    fi
fi

# ==========================================================================
# RESULTS
# ==========================================================================
{
printf '\n'
printf '%s\n' '========================================================================'
printf ' OKF catalog format benchmark - REAL measured results\n'
printf '%s\n' '========================================================================'
printf ' host        : %s cores, %s RAM\n' "$HOST_NPROC" "$HOST_MEM"
printf ' postgres    : %s\n' "$PG_VERSION"
printf ' rustc       : %s\n' "$RUSTC_VERSION"
printf ' dataset     : %s concepts (%s files, seed %s)\n' "$CORPUS_COUNT" "$FILE_COUNT" "$SEED"
printf ' rows        : concepts=%s links=%s provenance=%s metadata=%s\n' \
        "$CONCEPT_ROWS" "$LINK_ROWS" "$PROV_ROWS" "$META_ROWS"
printf '%s\n' '------------------------------------------------------------------------'
printf ' %-34s | %s\n' "Metric" "Value"
printf '%s\n' '------------------------------------------------------------------------'
printf ' %-34s | %s (%s bytes)\n'  "Disk: YAML corpus dir"        "$CORPUS_HUMAN" "$CORPUS_BYTES"
printf ' %-34s | %s (%s bytes)\n'  "Disk: PG catalog (tbl+idx)"   "$PG_TOTAL_HUMAN" "$PG_TOTAL_BYTES"
printf ' %-34s | %s (%s bytes)\n'  "Disk: Parquet (4 files)"      "$PARQUET_TOTAL_HUMAN" "$PARQUET_TOTAL_BYTES"
printf '%s\n' '------------------------------------------------------------------------'
printf ' %-34s | %s ms  (%s files/s, RSS %s MiB)\n' "YAML: parse-all (okf_parser)" "$YAML_ELAPSED_MS" "$YAML_FILES_PER_SEC" "$YAML_PEAK_RSS"
printf ' %-34s | %s ms\n' "PG: register_bundle (bulk load)" "$REGISTER_MS"
printf ' %-34s | %s ms  (%s rows)\n' "PG: filtered scan by type"   "$TYPE_MS" "$TYPE_ROWS"
printf ' %-34s | %s ms  (%s rows)\n' "PG: filtered scan by tag GIN" "$TAG_MS" "$TAG_ROWS"
printf ' %-34s | %s ms  (%s rows)\n' "PG: FTS concept_search"      "$FTS_MS" "$FTS_ROWS"
printf ' %-34s | %s ms  (%s rows)\n' "PG: graph traversal (3 hops)" "$TRAV_MS" "$TRAV_ROWS"
printf ' %-34s | %s ms\n' "Parquet: export_parquet"     "$EXPORT_MS"
if [[ "$PARQUET_READ_STATUS" == "measured" ]]; then
printf ' %-34s | %s ms  (%s/%s rows match/total)\n' "Parquet: filtered read (type)" "$PARQUET_READ_MS" "${PARQUET_READ_ROWS_MATCHED:-?}" "${PARQUET_READ_ROWS_TOTAL:-?}"
else
printf ' %-34s | %s\n' "Parquet: filtered read (type)" "$PARQUET_READ_STATUS"
fi
printf '%s\n' '========================================================================'
} | tee "$RESULTS_FILE"

note "results also written to $RESULTS_FILE"
