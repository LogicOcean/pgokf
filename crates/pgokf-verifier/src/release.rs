//! Release SQL equivalence and compatibility source identity.
use std::collections::BTreeMap;
use std::path::Path;

const BEGIN: &str = "/* <begin connected objects> */";
const END: &str = "/* </end connected objects> */";
pub type InstallEntities = BTreeMap<Vec<String>, usize>;
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallRegion {
    Outside(Vec<String>),
    Generated(InstallEntities),
}
pub type InstallInventory = Vec<InstallRegion>;

/// Compare consecutive generated entities as multisets; retain ordered outside regions.
/// Markers count only as complete top-level comments, never inside literals or
/// nested comments. Literal contents remain byte-exact through the SQL lexer.
///
/// # Errors
/// Rejects invalid SQL lexical structure or unbalanced generated markers.
pub fn install_entities(source: &str) -> Result<InstallInventory, String> {
    let tokens = crate::sql::lex(source)?;
    let quoted: BTreeMap<usize, usize> = tokens
        .iter()
        .filter_map(|token| {
            matches!(
                token.kind,
                crate::sql::Kind::String | crate::sql::Kind::Identifier | crate::sql::Kind::Dollar
            )
            .then_some((token.offset, token.offset + token.text.len()))
        })
        .collect();
    let bytes = source.as_bytes();
    let mut cursor = 0;
    let mut start = 0;
    let mut inside = false;
    let mut markers = 0;
    let mut entities = Vec::new();
    while cursor < bytes.len() {
        if let Some(end) = quoted.get(&cursor) {
            cursor = *end;
        } else if bytes[cursor..].starts_with(b"--") {
            cursor += bytes[cursor..]
                .iter()
                .position(|b| matches!(*b, b'\n' | b'\r'))
                .unwrap_or(bytes.len() - cursor);
        } else if bytes[cursor..].starts_with(b"/*") {
            let comment_start = cursor;
            cursor += 2;
            let mut depth = 1;
            while depth > 0 && cursor < bytes.len() {
                if bytes[cursor..].starts_with(b"/*") {
                    depth += 1;
                    cursor += 2;
                } else if bytes[cursor..].starts_with(b"*/") {
                    depth -= 1;
                    cursor += 2;
                } else {
                    cursor += 1;
                }
            }
            if depth != 0 {
                return Err("unterminated SQL comment".into());
            }
            let comment = &source[comment_start..cursor];
            if comment == BEGIN || comment == END {
                if (comment == BEGIN) == inside {
                    return Err("nested or unexpected generated entity marker".into());
                }
                add_region(&mut entities, &source[start..comment_start], inside)?;
                inside = comment == BEGIN;
                markers += 1;
                start = cursor;
            }
        } else {
            cursor += 1;
        }
    }
    if inside || markers == 0 {
        return Err("missing or unclosed generated entity".into());
    }
    add_region(&mut entities, &source[start..], false)?;
    Ok(entities)
}

fn add_region(regions: &mut InstallInventory, source: &str, inside: bool) -> Result<(), String> {
    let lexed = crate::sql::lex(source)?;
    let mut tokens = Vec::new();
    let mut previous: Option<&crate::sql::Token> = None;
    for token in &lexed {
        if let Some(before) = previous {
            let gap = &source[before.offset + before.text.len()..token.offset];
            // SQL continuation strings require a newline. Preserve this lexical
            // distinction rather than pretending all separators are equivalent.
            if before.kind == crate::sql::Kind::String && token.kind == crate::sql::Kind::String {
                tokens.push(
                    if gap.contains(['\r', '\n']) {
                        "separator:newline"
                    } else {
                        "separator:no-newline"
                    }
                    .to_owned(),
                );
            }
            // Conservative operator-boundary preservation. This is deliberately
            // not a full PostgreSQL operator or expression parser.
            if operator_token(before) && operator_token(token) {
                tokens.push(
                    if gap.is_empty() {
                        "operator:joined"
                    } else {
                        "operator:separate"
                    }
                    .to_owned(),
                );
            }
        }
        tokens.push(token.text.clone());
        previous = Some(token);
    }
    if !tokens.is_empty() {
        if inside {
            if !matches!(regions.last(), Some(InstallRegion::Generated(_))) {
                regions.push(InstallRegion::Generated(BTreeMap::new()));
            }
            if let Some(InstallRegion::Generated(entities)) = regions.last_mut() {
                *entities.entry(tokens).or_default() += 1;
            }
        } else {
            regions.push(InstallRegion::Outside(tokens));
        }
    }
    Ok(())
}
fn operator_token(token: &crate::sql::Token) -> bool {
    token.kind == crate::sql::Kind::Symbol
        && token
            .text
            .bytes()
            .all(|b| b"+-*/<>=~!@#%^&|?:".contains(&b))
}

/// Compare token inventories within generated regions; preserve the ordered
/// executable text outside those regions. This is not SQL semantic equivalence.
/// # Errors
/// Returns lexical/boundary failures or deterministic inventory differences.
pub fn compare_install_sql(expected: &str, actual: &str) -> Result<(), String> {
    let expected = install_entities(expected)?;
    let actual = install_entities(actual)?;
    if expected == actual {
        return Ok(());
    }
    Err(format!(
        "install SQL region inventory changed: expected {expected:?}; actual {actual:?}"
    ))
}

/// Read the control identity rather than searching for a matching substring.
/// # Errors
/// Missing, duplicate, malformed or historical version declarations fail.
pub fn compatibility_identity(control: &str, expected: &str) -> Result<String, String> {
    let mut versions = Vec::new();
    for line in control.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "default_version" {
            continue;
        }
        let value = value.trim();
        let Some(value) = value.strip_prefix('\'') else {
            return Err("version must be quoted".into());
        };
        let Some((version, tail)) = value.split_once('\'') else {
            return Err("unclosed control version".into());
        };
        if !tail.trim().is_empty() && !tail.trim().starts_with('#') {
            return Err("unexpected text after control version".into());
        }
        versions.push(version);
    }
    if versions.as_slice() != [expected] {
        return Err(format!(
            "exactly one current default_version {expected:?} required; got {versions:?}"
        ));
    }
    Ok(expected.to_owned())
}

/// # Errors
/// Returns source read errors and current-release identity failures.
pub fn compatibility_source(root: &Path) -> Result<String, String> {
    let path = root.join("crates/extension/pgokf.control");
    let control =
        std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    compatibility_identity(&control, env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn block(sql: &str) -> String {
        format!("{BEGIN}\n{sql}\n{END}")
    }
    #[test]
    fn outside_sql_is_not_lost() {
        let original = block("SELECT 1;");
        assert!(
            compare_install_sql(
                &original,
                &(original.clone() + "DELETE FROM pgokf.bundles;")
            )
            .is_err()
        );
    }
    #[test]
    fn literal_whitespace_is_significant() {
        assert!(compare_install_sql(&block("SELECT 'a  b';"), &block("SELECT 'a b';")).is_err());
    }
    #[test]
    fn order_and_source_comments_are_immaterial() {
        assert!(
            compare_install_sql(
                &(block("-- path:1\nSELECT 1;") + &block("SELECT 2;")),
                &(block("SELECT 2;") + &block("-- path:9\n SELECT   1;"))
            )
            .is_ok()
        );
    }
    #[test]
    fn incomplete_block_fails_closed() {
        assert!(install_entities(&(block("SELECT 1;") + BEGIN + "SELECT 2;")).is_err());
    }
    #[test]
    fn duplicate_entity_removal_is_detected() {
        assert!(
            compare_install_sql(
                &(block("SELECT 1;") + &block("SELECT 1;")),
                &block("SELECT 1;")
            )
            .is_err()
        );
    }
    #[test]
    fn markers_inside_literals_and_nested_comments_are_not_boundaries() {
        let source = block(&format!(
            "SELECT '{BEGIN}', $$ {END} $$; /* nested {BEGIN} */"
        ));
        assert!(install_entities(&source).is_ok());
        assert!(install_entities(&format!("SELECT '{BEGIN}', '{END}';")).is_err());
    }
    #[test]
    fn malformed_boundaries_refused() {
        for source in [
            format!("{END}{BEGIN}"),
            format!("{BEGIN}{BEGIN}{END}{END}"),
            String::new(),
        ] {
            assert!(install_entities(&source).is_err());
        }
    }
    #[test]
    fn identity_requires_one_exact_current_declaration() {
        assert_eq!(
            compatibility_identity("default_version = '0.4.0-dev3' # current", "0.4.0-dev3")
                .unwrap(),
            "0.4.0-dev3"
        );
        for source in [
            "",
            "# default_version = '0.4.0-dev3'",
            "default_version = '0.3.1'",
            "default_version = 0.4.0-dev3",
            "default_version = '0.4.0-dev3' trailing",
            "default_version = '0.4.0-dev3'\ndefault_version = '0.3.1'",
        ] {
            assert!(
                compatibility_identity(source, "0.4.0-dev3").is_err(),
                "{source}"
            );
        }
    }
}

#[cfg(test)]
mod hostile_regressions {
    fn block(sql: &str) -> String {
        format!("{}{}{}", super::BEGIN, sql, super::END)
    }
    #[test]
    fn carriage_return_cannot_hide_sql() {
        assert!(
            super::compare_install_sql(
                &block("SELECT 1; -- comment\rSELECT 2;\n"),
                &block("SELECT 1;")
            )
            .is_err()
        );
    }
    #[test]
    fn string_continuation_newline_is_significant() {
        assert!(
            super::compare_install_sql(&block("SELECT 'a'\n'b';"), &block("SELECT 'a' 'b';"))
                .is_err()
        );
    }
    #[test]
    fn operator_adjacency_is_significant() {
        assert!(
            super::compare_install_sql(&block("SELECT 1 >= 0;"), &block("SELECT 1 > = 0;"))
                .is_err()
        );
    }
    #[test]
    fn outside_execution_cannot_swap_with_generated_entity() {
        assert!(
            super::compare_install_sql(
                &(String::from("SET search_path = public;")
                    + &block("SET search_path = pg_catalog;")),
                &(String::from("SET search_path = pg_catalog;")
                    + &block("SET search_path = public;"))
            )
            .is_err()
        );
    }
    #[test]
    fn outside_execution_orders_generated_regions() {
        let first = format!(
            "{}SET search_path = public;{}",
            block("SELECT 1;"),
            block("SELECT 2;")
        );
        let changed = format!(
            "{}SET search_path = public;{}",
            block("SELECT 2;"),
            block("SELECT 1;")
        );
        assert!(super::compare_install_sql(&first, &changed).is_err());
    }
}

struct Versions(Vec<syn::ItemFn>);
impl<'ast> syn::visit::Visit<'ast> for Versions {
    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        if item.sig.ident == "version" {
            self.0.push(item.clone());
        }
    }
}

/// Verify the install script binds version exactly once to the product C wrapper.
/// # Errors
/// Rejects duplicate, SQL-language or substituted bindings and hardcoded Rust versions.
pub fn check_install_binding(source: &str, rust: &str) -> Result<(), String> {
    use crate::sql::{self, Kind};
    let mut definitions = Vec::new();
    for statement in sql::statements(source)? {
        let words: Vec<_> = statement
            .iter()
            .map(|t| match t.kind {
                Kind::Word => t.text.to_ascii_lowercase(),
                Kind::Identifier => t.text.trim_matches('"').to_owned(),
                _ => t.text.clone(),
            })
            .collect();
        if words.first().is_none_or(|s| s != "create") {
            continue;
        }
        if words
            .windows(6)
            .any(|w| w == ["function", "pgokf", ".", "version", "(", ")"])
        {
            definitions.push(words);
        }
    }
    if definitions.len() != 1 {
        return Err("exactly one pgokf.version definition required".into());
    }
    let definition = &definitions[0];
    if !definition.windows(2).any(|w| w == ["language", "c"])
        || !definition
            .windows(4)
            .any(|w| w == ["as", "'MODULE_PATHNAME'", ",", "'version_wrapper'"])
    {
        return Err("version must bind LANGUAGE c to MODULE_PATHNAME/version_wrapper".into());
    }
    let parsed = syn::parse_file(rust).map_err(|e| e.to_string())?;
    let mut versions = Versions(Vec::new());
    syn::visit::Visit::visit_file(&mut versions, &parsed);
    let functions = versions.0;
    if functions.len() != 1
        || !functions[0].sig.inputs.is_empty()
        || functions[0].block.stmts.len() != 1
    {
        return Err("version source is not the canonical environment binding".into());
    }
    let syn::Stmt::Expr(syn::Expr::Macro(value), None) = &functions[0].block.stmts[0] else {
        return Err("version source is not env!".into());
    };
    let arg: syn::LitStr = syn::parse2(value.mac.tokens.clone()).map_err(|e| e.to_string())?;
    if !value.mac.path.is_ident("env") || arg.value() != "CARGO_PKG_VERSION" {
        return Err("version source must return CARGO_PKG_VERSION".into());
    }
    Ok(())
}

#[cfg(test)]
mod install_binding_tests {
    use super::check_install_binding;
    const SQL: &str = "CREATE FUNCTION pgokf.\"version\"() RETURNS text LANGUAGE c AS 'MODULE_PATHNAME', 'version_wrapper';";
    const RUST: &str = "fn version() -> &'static str { env!(\"CARGO_PKG_VERSION\") }";
    #[test]
    fn exact_c_binding_and_source() {
        assert!(check_install_binding(SQL, RUST).is_ok());
        for bad in [
            format!("{SQL}{SQL}"),
            SQL.replace("LANGUAGE c", "LANGUAGE sql"),
            SQL.replace("version_wrapper", "other_wrapper"),
            SQL.replace("MODULE_PATHNAME", "other"),
            String::new(),
        ] {
            assert!(check_install_binding(&bad, RUST).is_err());
        }
        assert!(check_install_binding(SQL, "fn version() -> &'static str { \"0.2.0\" }").is_err());
        assert!(check_install_binding(SQL, &RUST.replace("CARGO_PKG_VERSION", "OTHER")).is_err());
    }
}
