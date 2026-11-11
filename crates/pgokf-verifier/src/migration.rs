//! Deterministic migration graph and conservative executable SQL safety checks.
use crate::sql::{self, Kind, Token};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Parse migration filenames without accepting ambiguous or duplicate edges.
/// # Errors
/// Rejects malformed migration filenames and duplicate graph edges.
pub fn parse_edges(names: &[String]) -> Result<Vec<(String, String)>, String> {
    let mut edges = BTreeSet::new();
    for name in names {
        let Some(inner) = name
            .strip_prefix("pgokf--")
            .and_then(|v| v.strip_suffix(".sql"))
        else {
            continue;
        };
        let parts: Vec<_> = inner.split("--").collect();
        if parts.len() == 1 {
            continue;
        }
        if parts.len() != 2 || parts.iter().any(|s| s.is_empty()) {
            return Err(format!("malformed migration {name}"));
        }
        if !edges.insert((parts[0].to_owned(), parts[1].to_owned())) {
            return Err(format!("duplicate migration {name}"));
        }
    }
    Ok(edges.into_iter().collect())
}

/// Validate reachability, a unique final version and absence of every cycle.
/// # Errors
/// Rejects disconnected, cyclic, terminal-source or missing install graphs.
pub fn check_graph(names: &[String], version: &str) -> Result<(), String> {
    let edges = parse_edges(names)?;
    let mut reached = BTreeSet::from(["0.1.0".to_owned()]);
    loop {
        let before = reached.len();
        for (from, to) in &edges {
            if reached.contains(from) {
                reached.insert(to.clone());
            }
        }
        if before == reached.len() {
            break;
        }
    }
    if !reached.contains(version) {
        return Err(format!("no upgrade path to {version}"));
    }
    let mut indegree = BTreeMap::<String, usize>::new();
    for (from, to) in &edges {
        if !reached.contains(from) {
            return Err(format!("unreachable source {from}"));
        }
        if from == version {
            return Err(format!("final release {version} is a source"));
        }
        indegree.entry(from.clone()).or_default();
        *indegree.entry(to.clone()).or_default() += 1;
    }
    let mut ready: BTreeSet<_> = indegree
        .iter()
        .filter(|(_, n)| **n == 0)
        .map(|(v, _)| v.clone())
        .collect();
    let mut visited = 0;
    while let Some(node) = ready.pop_first() {
        visited += 1;
        let next: Vec<_> = edges
            .iter()
            .filter(|(a, _)| *a == node)
            .map(|(_, b)| b)
            .collect();
        if next.is_empty() && node != version {
            return Err(format!("unexpected terminal {node}"));
        }
        for to in next {
            let n = indegree.get_mut(to).ok_or("missing graph node")?;
            *n -= 1;
            if *n == 0 {
                ready.insert(to.clone());
            }
        }
    }
    if visited != indegree.len() {
        return Err("migration cycle".into());
    }
    if !version.contains('-') && !names.contains(&format!("pgokf--{version}.sql")) {
        return Err("committed install script missing".into());
    }
    Ok(())
}

/// The unique consecutive development chain from the last published release.
/// # Errors
/// Rejects forks, gaps, cycles and development versions on a different line.
pub fn development_chain(names: &[String], version: &str) -> Result<Vec<(String, String)>, String> {
    let edges = parse_edges(names)?;
    let base = version.split('-').next().ok_or("missing version")?;
    let mut at = "0.3.1".to_owned();
    let mut chain = Vec::new();
    let mut seen = BTreeSet::new();
    while at != version {
        if !seen.insert(at.clone()) {
            return Err("development cycle".into());
        }
        let next: Vec<_> = edges.iter().filter(|(a, _)| *a == at).collect();
        let expected = format!("{base}-dev{}", chain.len() + 1);
        if next.len() != 1 || next[0].1 != expected {
            return Err(format!(
                "nonconsecutive or ambiguous development edge at {at}"
            ));
        }
        chain.push(next[0].clone());
        at.clone_from(&next[0].1);
    }
    Ok(chain)
}

fn words(tokens: &[Token]) -> Vec<String> {
    tokens
        .iter()
        .map(|t| {
            if matches!(t.kind, Kind::Word) {
                t.text.to_ascii_uppercase()
            } else {
                t.text.clone()
            }
        })
        .collect()
}
fn receipt(tokens: &[Token]) -> bool {
    words(tokens)
        == [
            "SELECT",
            "PGOKF_PRIVATE",
            ".",
            "REGISTER_DUMP_RELATIONS",
            "(",
            ")",
        ]
}

/// Require exactly the immutable dump-registration receipt.
/// # Errors
/// Rejects additional executable SQL, including nested bodies.
pub fn check_receipt(source: &str) -> Result<(), String> {
    let statements = sql::statements(source)?;
    if statements.len() != 1 || !receipt(&statements[0]) {
        return Err("finalization receipt must be exactly dump registration".into());
    }
    Ok(())
}

/// Conservatively inspect executable routine/DO bodies and CTEs as well as DDL.
/// Unknown dynamic execution is refused, never treated as inert text.
/// # Errors
/// Rejects destructive SQL, unguarded retirements and malformed final receipts.
pub fn check_development_edge(source: &str, retired: &[&str]) -> Result<(), String> {
    let statements = sql::statements(source)?;
    if statements.last().is_none_or(|s| !receipt(s))
        || statements.iter().filter(|s| receipt(s)).count() != 1
    {
        return Err("development edge must end with exactly one dump registration".into());
    }
    let allowed: Vec<_> = retired
        .iter()
        .map(|signature| {
            sql::lex(&format!("DROP FUNCTION IF EXISTS {signature} RESTRICT")).map(|v| words(&v))
        })
        .collect::<Result<_, _>>()?;
    let mut guard = false;
    for statement in &statements {
        let w = words(statement);
        if w.first().is_some_and(|v| v == "DROP") {
            if !allowed.contains(&w) {
                return Err("destructive unlisted retirement".into());
            }
            if !guard {
                return Err("retirement requires preceding identity guard".into());
            }
        } else {
            safe_tokens(statement, 0)?;
        }
        if w.first().is_some_and(|v| v == "DO")
            && statement.iter().any(|t| {
                t.kind == Kind::Dollar
                    && sql::quoted_body(t)
                        .and_then(|body| sql::lex(&body))
                        .is_ok_and(|tokens| {
                            tokens.windows(3).any(|w| {
                                w[0].text.eq_ignore_ascii_case("raise")
                                    && w[1].text.eq_ignore_ascii_case("exception")
                                    && w[2].kind == Kind::String
                                    && w[2]
                                        .text
                                        .contains("reconciliation refuses retired function")
                            })
                        })
            })
        {
            guard = true;
        }
    }
    Ok(())
}

fn safe_tokens(tokens: &[Token], depth: usize) -> Result<(), String> {
    if depth > 32 {
        return Err("SQL nesting bound exceeded".into());
    }
    let w = words(tokens);
    if w.windows(2).enumerate().any(|(i, p)| {
        p[0] == "LANGUAGE"
            && tokens[i].kind == Kind::Word
            && matches!(tokens[i + 1].kind, Kind::Word | Kind::Identifier)
            && (i == 0 || w[i - 1] != ".")
            && !["SQL", "PLPGSQL", "C"]
                .contains(&p[1].trim_matches('"').to_ascii_uppercase().as_str())
    }) {
        return Err("unsupported executable language".into());
    }
    for (i, token) in tokens.iter().enumerate() {
        let at = w[i].as_str();
        let prev = i.checked_sub(1).and_then(|n| w.get(n)).map(String::as_str);
        if matches!(token.kind, Kind::Dollar | Kind::String)
            && (sql::is_do_body(tokens, i)? || prev == Some("AS"))
        {
            safe_tokens(&sql::lex(&sql::quoted_body(token)?)?, depth + 1)?;
        }
        if token.kind != Kind::Word {
            continue;
        }
        if at == "CASCADE" && !(prev == Some("DELETE") && i >= 2 && w[i - 2] == "ON") {
            return Err("unsafe CASCADE".into());
        }
        if at == "TRUNCATE" || at == "DROP" {
            return Err("destructive SQL".into());
        }
        if at == "DELETE" && w.get(i + 1).is_some_and(|s| s == "FROM") {
            return Err("destructive DELETE".into());
        }
        if matches!(at, "UPDATE" | "INSERT" | "MERGE") {
            let offset = usize::from(matches!(at, "INSERT" | "MERGE"));
            if w.get(i + 1 + offset).is_some_and(|s| {
                s.trim_matches('"').eq_ignore_ascii_case("pg_catalog")
                    || s.trim_matches('"').to_ascii_lowercase().starts_with("pg_")
            }) {
                return Err("system catalog DML".into());
            }
            if w[..i].iter().any(|v| v == "WITH") {
                return Err("data modifying CTE".into());
            }
        }
        if at == "EXECUTE"
            && !matches!(prev, Some("GRANT" | "REVOKE" | ":"))
            && w.get(i + 1).is_none_or(|s| s != "ON")
        {
            let next = tokens.get(i + 1).ok_or("missing dynamic SQL")?;
            if next.kind == Kind::String {
                if w.get(i + 2)
                    .is_some_and(|s| s != ";" && s != "USING" && s != "INTO")
                {
                    return Err("unproven dynamic SQL concatenation".into());
                }
                safe_tokens(&sql::lex(&sql::quoted_body(next)?)?, depth + 1)?;
            } else {
                // The shipped ownership operation uses regrole's quoted SQL
                // rendering, with one substitution at a complete identifier.
                let tail = &w[i + 1..];
                let exact = ["PG_CATALOG", ".", "FORMAT", "("];
                let template = tokens
                    .get(i + 5)
                    .filter(|t| t.kind == Kind::String)
                    .map(sql::quoted_body)
                    .transpose()?;
                let safe_owner = tail.starts_with(&exact.map(str::to_owned))
                    && template.as_ref().is_some_and(|s| {
                        s.starts_with("ALTER FUNCTION pgokf.")
                            && s.ends_with(" OWNER TO %s")
                            && s.matches('%').count() == 1
                    })
                    && tail.get(5..13).is_some_and(|s| {
                        s.len() == 8
                            && s[0] == ","
                            && s[2..] == [":", ":", "PG_CATALOG", ".", "REGROLE", ")"]
                    });
                if !safe_owner {
                    return Err("unproven dynamic SQL".into());
                }
                safe_tokens(
                    &sql::lex(
                        &template
                            .ok_or("missing owner template")?
                            .replace("%s", "pgokf_reader"),
                    )?,
                    depth + 1,
                )?;
            }
        }
    }
    Ok(())
}

/// Validate the shipped graph, frozen reviewed edges and current development SQL.
/// # Errors
/// Rejects filesystem errors, rewritten history and unsafe migrations.
pub fn check_repository(root: &Path, version: &str) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    let dir = root.join("crates/extension/sql");
    let mut names = std::fs::read_dir(&dir)
        .map_err(|e| e.to_string())?
        .map(|e| {
            e.map(|v| v.file_name().to_string_lossy().into_owned())
                .map_err(|e| e.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    names.sort();
    check_graph(&names, version)?;
    for (name, expected) in [
        (
            "pgokf--0.3.1--0.4.0-dev1.sql",
            "dc100491d7a399fbc17be237ce36b84e10128b0858360f96d1880cd74f4dbad5",
        ),
        (
            "pgokf--0.4.0-dev1--0.4.0-dev2.sql",
            "c69eeb709bde20858e762acb37fcdff24c70521f4d9c1fb69c8846472f18f022",
        ),
        (
            "pgokf--0.4.0-dev2--0.4.0-dev3.sql",
            "48e28a0776fe337e5ba0001abc1b3b95ac1ed5516ec427416187c10b5459f2ef",
        ),
    ] {
        let bytes = std::fs::read(dir.join(name)).map_err(|e| e.to_string())?;
        if Sha256::digest(bytes)
            .iter()
            .fold(String::new(), |mut result, byte| {
                let _ = write!(result, "{byte:02x}");
                result
            })
            != expected
        {
            return Err(format!("reviewed edge {name} rewritten"));
        }
    }
    let read = |name: &str| std::fs::read_to_string(dir.join(name)).map_err(|e| e.to_string());
    check_receipt(&read("pgokf--0.3.0--0.3.1.sql")?)?;
    check_receipt(&read("pgokf--0.4.0-dev3--0.4.0.sql")?)?;
    if version.contains('-') {
        for (from, to) in development_chain(&names, version)? {
            let retired: &[&str] = if to == "0.4.0-dev2" {
                &[
                    "PGOKF.CONCEPT_SEARCH_SEMANTIC(REAL[], BIGINT, INTEGER)",
                    "PGOKF.CONCEPT_SEARCH_HYBRID(TEXT, REAL[], BIGINT, INTEGER)",
                    "PGOKF.BM25_HITS(TEXT, BIGINT, BIGINT, TEXT, TEXT, TEXT[], TEXT, TEXT, REAL, BIGINT, TEXT)",
                ]
            } else {
                &[]
            };
            check_development_edge(&read(&format!("pgokf--{from}--{to}.sql"))?, retired)
                .map_err(|e| format!("{from} -> {to}: {e}"))?;
        }
    } else {
        let edges = parse_edges(&names)?;
        let final_edges: Vec<_> = edges.iter().filter(|(_, to)| to == version).collect();
        if final_edges.len() != 1 {
            return Err("release requires exactly one finalization edge".into());
        }
        check_receipt(&read(&format!(
            "pgokf--{}--{version}.sql",
            final_edges[0].0
        ))?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const RECEIPT: &str = "SELECT pgokf_private.register_dump_relations();";
    #[test]
    fn executable_nested_safety() {
        for bad in [
            "DO $$ BEGIN DELETE FROM pgokf.skills; END $$;",
            "DO $a$ BEGIN DO $b$ BEGIN TRUNCATE pgokf.skills; END $b$; END $a$;",
            "WITH gone AS (DELETE FROM pgokf.skills RETURNING name) SELECT name FROM gone;",
            "UPDATE pg_catalog.pg_class SET relname='x';",
            "UPDATE pg_class SET relname='x';",
            "DO $$ BEGIN WITH changed AS (UPDATE pgokf.skills SET name='x' RETURNING name) SELECT name FROM changed; END $$;",
            "ALTER TABLE pgokf.skills ADD COLUMN id int REFERENCES pgokf.concepts(id) ON UPDATE CASCADE;",
            "DO $$ BEGIN EXECUTE 'DROP TABLE pgokf.skills'; END $$;",
            "DO $$ BEGIN EXECUTE command; END $$;",
            "ALTER TABLE pgokf.skills DROP COLUMN name CASCADE;",
            "CREATE FUNCTION f() RETURNS void LANGUAGE sql AS 'DELETE FROM pgokf.skills';",
        ] {
            assert!(
                check_development_edge(&format!("{bad}{RECEIPT}"), &[]).is_err(),
                "{bad}"
            );
        }
        assert!(
            check_development_edge(
                &format!("/* DELETE */ ALTER TABLE pgokf.skills ADD COLUMN sample text;{RECEIPT}"),
                &[]
            )
            .is_ok()
        );
    }
    #[test]
    fn receipt_is_exact_and_last() {
        for bad in [
            String::new(),
            format!("{RECEIPT}{RECEIPT}"),
            format!("{RECEIPT}SELECT 1;"),
        ] {
            assert!(check_development_edge(&bad, &[]).is_err());
        }
    }
    #[test]
    fn cycles_and_third_branch_refused() {
        let names = [
            "pgokf--0.1.0--0.3.1.sql",
            "pgokf--0.3.1--0.4.0-dev1.sql",
            "pgokf--0.4.0-dev1--0.4.0-dev2.sql",
            "pgokf--0.4.0-dev2--0.4.0-dev1.sql",
        ]
        .map(str::to_owned);
        assert!(check_graph(&names, "0.4.0-dev2").is_err());
        assert!(development_chain(&names, "0.4.0-dev3").is_err());
    }
}

#[cfg(test)]
mod graph_controls {
    use super::*;
    fn names() -> Vec<String> {
        [
            "pgokf--0.1.0--0.3.1.sql",
            "pgokf--0.3.1--0.4.0-dev1.sql",
            "pgokf--0.4.0-dev1--0.4.0-dev2.sql",
            "pgokf--0.4.0-dev2--0.4.0-dev3.sql",
        ]
        .map(str::to_owned)
        .to_vec()
    }
    #[test]
    fn graph_reachability_finalization_and_install() {
        let original = names();
        assert!(check_graph(&original, "0.4.0-dev3").is_ok());
        assert_eq!(development_chain(&original, "0.4.0-dev3").unwrap().len(), 3);
        for extra in [
            "pgokf--0.4.0-dev3--0.4.0.sql",
            "pgokf--0.3.1--orphan.sql",
            "pgokf--unreachable--0.4.0-dev3.sql",
        ] {
            let mut changed = original.clone();
            changed.push(extra.into());
            assert!(check_graph(&changed, "0.4.0-dev3").is_err());
        }
        let mut missing = original.clone();
        missing.remove(1);
        assert!(check_graph(&missing, "0.4.0-dev3").is_err());
        let mut stable = original.clone();
        stable.push("pgokf--0.4.0-dev3--0.4.0.sql".into());
        assert!(check_graph(&stable, "0.4.0").is_err());
        stable.push("pgokf--0.4.0.sql".into());
        assert!(check_graph(&stable, "0.4.0").is_ok());
        let mut fork = original;
        fork.push("pgokf--0.3.1--0.4.0-dev2.sql".into());
        assert!(development_chain(&fork, "0.4.0-dev3").is_err());
    }
    #[test]
    fn cycle_without_current_source_is_refused() {
        let names = [
            "pgokf--0.1.0--0.3.1.sql",
            "pgokf--0.3.1--0.4.0-dev1.sql",
            "pgokf--0.4.0-dev1--0.3.1.sql",
            "pgokf--0.4.0-dev1--0.4.0-dev3.sql",
        ]
        .map(str::to_owned);
        assert!(check_graph(&names, "0.4.0-dev3").is_err());
    }
    #[test]
    fn named_guard_and_exact_restrict_retirement() {
        let signature = "PGOKF.F(INTEGER)";
        let guard =
            "DO $$ BEGIN RAISE EXCEPTION 'reconciliation refuses retired function'; END $$;";
        let drop = "DROP FUNCTION IF EXISTS pgokf.f(integer) RESTRICT;";
        let receipt = "SELECT pgokf_private.register_dump_relations();";
        assert!(check_development_edge(&format!("{guard}{drop}{receipt}"), &[signature]).is_ok());
        for bad in [
            format!("{drop}{receipt}"),
            format!("{drop}{guard}{receipt}"),
            format!("{guard}{}{receipt}", drop.replace("RESTRICT", "CASCADE")),
            format!("{guard}{}{receipt}", drop.replace("integer", "text")),
        ] {
            assert!(check_development_edge(&bad, &[signature]).is_err());
        }
        assert!(check_development_edge(&format!("{guard}{drop}{receipt}"), &[]).is_err());
    }
    #[test]
    fn frozen_history_and_release_receipt_cannot_change() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let temporary = tempfile::tempdir().unwrap();
        let target = temporary.path().join("crates/extension/sql");
        std::fs::create_dir_all(&target).unwrap();
        for file in std::fs::read_dir(root.join("crates/extension/sql")).unwrap() {
            let file = file.unwrap();
            std::fs::copy(file.path(), target.join(file.file_name())).unwrap();
        }
        let frozen = target.join("pgokf--0.3.1--0.4.0-dev1.sql");
        let original = std::fs::read_to_string(&frozen).unwrap();
        std::fs::write(&frozen, format!("-- rewritten history\n{original}")).unwrap();
        assert!(check_repository(temporary.path(), env!("CARGO_PKG_VERSION")).is_err());
        std::fs::write(&frozen, original).unwrap();
        // The current release's own finalization edge, read from the shipped
        // graph, so the control holds for every release without an edit.
        let version = env!("CARGO_PKG_VERSION");
        let names: Vec<String> = std::fs::read_dir(&target)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        let (from, _) = parse_edges(&names)
            .unwrap()
            .into_iter()
            .find(|(_, to)| to == version)
            .expect("the current release has a finalization edge");
        let final_edge = target.join(format!("pgokf--{from}--{version}.sql"));
        let receipt = "SELECT pgokf_private.register_dump_relations();";
        std::fs::write(&final_edge, receipt).unwrap();
        check_repository(temporary.path(), version).unwrap();
        std::fs::write(&final_edge, "SELECT 1;").unwrap();
        assert!(check_repository(temporary.path(), version).is_err());
        std::fs::write(&final_edge, receipt).unwrap();
        // A past finalization receipt stays an exact receipt.
        let historical = target.join("pgokf--0.4.0-dev3--0.4.0.sql");
        let historical_original = std::fs::read_to_string(&historical).unwrap();
        std::fs::write(&historical, "SELECT 1;").unwrap();
        assert!(check_repository(temporary.path(), version).is_err());
        std::fs::write(&historical, historical_original).unwrap();
        check_repository(temporary.path(), version).unwrap();
        std::fs::write(
            target.join(format!("pgokf--0.4.0-dev2--{version}.sql")),
            receipt,
        )
        .unwrap();
        assert!(check_repository(temporary.path(), version).is_err());
    }
    #[test]
    fn shipped_graph_and_frozen_history() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        check_repository(root, env!("CARGO_PKG_VERSION")).unwrap();
    }
}

pub(crate) fn check_executable_body(source: &str) -> Result<(), String> {
    safe_tokens(&sql::lex(source)?, 0)
}
