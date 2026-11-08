//! Closed structural contracts for SQL generators with non-string inputs.
//! These compare Rust syntax, then derive output from the source's finite data.
use std::collections::BTreeMap;
use syn::visit::Visit;

pub(super) fn quoted_body(signature: &syn::Signature, block: &syn::Block) -> Option<String> {
    for (expected, body, value) in [
        (
            "fn quote_identifier(name: &str) -> String",
            r#"{ format!("\"{}\"", name.replace('"', "\"\"")) }"#,
            "\"pgokf_web\"\u{1f}\"users\"\u{1f}\"sessions\"",
        ),
        (
            "fn quote_literal(value: &str) -> String",
            r#"{ format!("E'{}'", value.replace('\\', "\\\\").replace('\'', "''")) }"#,
            "E'literal'",
        ),
        (
            "fn pgvector_schema() -> Result<Option<String>, CatalogError>",
            r#"{
    Spi::connect(|client| {
        let table = client
            .select(
                "SELECT pg_catalog.quote_ident(n.nspname)
                 FROM pg_catalog.pg_extension e
                 JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace
                 WHERE e.extname = 'vector'",
                Some(1),
                &[],
            )
            .map_err(spi_error("failed to resolve the pgvector schema"))?;
        if table.is_empty() {
            return Ok(None);
        }
        table
            .first()
            .get_one::<String>()
            .map_err(spi_error("failed to read the pgvector schema"))
    })
}"#,
            "\"pgokf_web\"\u{1f}\"users\"\u{1f}\"sessions\"",
        ),
    ] {
        if syn::parse_str::<syn::Signature>(expected).ok().as_ref() == Some(signature)
            && syn::parse_str::<syn::Block>(body).ok().as_ref() == Some(block)
        {
            return Some(value.into());
        }
    }
    None
}

const TABLE_BUILDER: &str = r#"{
    let select_list = spec
        .columns
        .iter()
        .map(|column| format!("{} AS {}", column.select_expr, column.name))
        .collect::<Vec<_>>()
        .join(", ");
    let key_names = spec.key_names();
    let keyset_predicate = if resume {
        let columns = key_names.join(", ");
        let placeholders = (0..key_names.len())
            .map(|offset| format!("${}", offset + 2))
            .collect::<Vec<_>>()
            .join(", ");
        format!(" AND ({columns}) > ({placeholders})")
    } else {
        String::new()
    };
    format!(
        "SELECT {select_list} FROM {table} WHERE bundle_id = $1{keyset_predicate} ORDER BY {order_by}",
        table = spec.table,
        order_by = key_names.join(", "),
    )
}"#;
const KEY_NAMES: &str = r"{
    self.key_indices
        .iter()
        .map(|&i| self.columns[i].name)
        .collect()
}";
fn field<'a>(value: &'a syn::ExprStruct, name: &str) -> Result<&'a syn::Expr, String> {
    value
        .fields
        .iter()
        .find(|f| matches!(&f.member, syn::Member::Named(id) if id == name))
        .map(|f| &f.expr)
        .ok_or_else(|| format!("missing static SQL field {name}"))
}
fn literal(value: &syn::Expr) -> Result<String, String> {
    if let syn::Expr::Lit(lit) = value
        && let syn::Lit::Str(text) = &lit.lit
    {
        return Ok(text.value());
    }
    Err("SQL table specification must contain only literal text".into())
}
fn array(value: &syn::Expr) -> Result<&syn::ExprArray, String> {
    let value = if let syn::Expr::Reference(r) = value {
        &*r.expr
    } else {
        value
    };
    if let syn::Expr::Array(array) = value {
        Ok(array)
    } else {
        Err("SQL table specification must contain a literal array".into())
    }
}

struct Constructors {
    count: usize,
    unsafe_code: bool,
}
impl<'ast> Visit<'ast> for Constructors {
    fn visit_expr_struct(&mut self, s: &'ast syn::ExprStruct) {
        if s.path
            .segments
            .last()
            .is_some_and(|s| s.ident == "TableSpec")
        {
            self.count += 1;
        }
        syn::visit::visit_expr_struct(self, s);
    }
    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        if matches!(&*item.self_ty, syn::Type::Path(p) if p.path.segments.last().is_some_and(|s| s.ident == "TableSpec" || s.ident == "ColumnSpec"))
        {
            let approved = item.trait_.is_none()
                && item.items.len() == 1
                && matches!(&item.items[0], syn::ImplItem::Fn(f) if syn::parse_str::<syn::Signature>("fn key_names(&self) -> Vec<&'static str>").ok().as_ref() == Some(&f.sig) && syn::parse_str::<syn::Block>(KEY_NAMES).ok().as_ref() == Some(&f.block));
            if !approved {
                self.unsafe_code = true;
            }
        }
        syn::visit::visit_item_impl(self, item);
    }
    fn visit_expr_assign(&mut self, item: &'ast syn::ExprAssign) {
        if matches!(&*item.left, syn::Expr::Field(f) if matches!(&f.member, syn::Member::Named(n) if ["table", "columns", "key_indices", "select_expr", "name"].contains(&n.to_string().as_str())))
        {
            self.unsafe_code = true;
        }
        syn::visit::visit_expr_assign(self, item);
    }
    fn visit_item_type(&mut self, item: &'ast syn::ItemType) {
        if matches!(&*item.ty, syn::Type::Path(p) if p.path.segments.last().is_some_and(|s| s.ident == "TableSpec" || s.ident == "ColumnSpec"))
        {
            self.unsafe_code = true;
        }
    }
    fn visit_expr_unsafe(&mut self, _: &'ast syn::ExprUnsafe) {
        self.unsafe_code = true;
    }
    fn visit_item_mod(&mut self, m: &'ast syn::ItemMod) {
        if m.attrs.iter().any(|a| {
            a.path().is_ident("cfg")
                && matches!(&a.meta,syn::Meta::List(l) if l.tokens.to_string()=="test")
        }) {
            return;
        }
        syn::visit::visit_item_mod(self, m);
    }
}

fn input_shapes(file: &syn::File) -> Result<(), String> {
    for expected in [
        "struct TableSpec { file_name: &'static str, table: &'static str, columns: &'static [ColumnSpec], key_indices: &'static [usize], }",
        "struct ColumnSpec { name: &'static str, pg_type: PgType, nullable: bool, select_expr: &'static str, }",
    ] {
        let expected = syn::parse_str::<syn::ItemStruct>(expected).map_err(|e| e.to_string())?;
        let actual = file
            .items
            .iter()
            .find_map(|i| match i {
                syn::Item::Struct(s) if s.ident == expected.ident => Some(s),
                _ => None,
            })
            .ok_or("missing table specification type")?;
        let mut actual = actual.clone();
        actual.attrs.retain(|a| !a.path().is_ident("doc"));
        for field in &mut actual.fields {
            field.attrs.retain(|a| !a.path().is_ident("doc"));
        }
        if actual != expected {
            return Err("unknown table specification shape".into());
        }
    }
    Ok(())
}

pub(super) fn tables(
    block: &syn::Block,
    file: &syn::File,
    functions: &BTreeMap<String, (syn::Signature, syn::Block)>,
) -> Result<Option<String>, String> {
    if syn::parse_str::<syn::Block>(TABLE_BUILDER).ok().as_ref() != Some(block) {
        return Ok(None);
    }
    if functions
        .get("key_names")
        .is_none_or(|(_, body)| syn::parse_str::<syn::Block>(KEY_NAMES).ok().as_ref() != Some(body))
    {
        return Err("unknown table ordering generator".into());
    }
    if functions.get("build_batch_query").is_none_or(|(s, _)| {
        syn::parse_str::<syn::Signature>(
            "fn build_batch_query(spec: &TableSpec, resume: bool) -> String",
        )
        .ok()
        .as_ref()
            != Some(s)
    }) {
        return Err("unknown table generator signature".into());
    }
    let private = file.items.iter().any(|i| matches!(i, syn::Item::Struct(s) if s.ident == "TableSpec" && matches!(s.vis,syn::Visibility::Inherited) && s.attrs.iter().all(|a| a.path().is_ident("doc"))));
    if !private {
        return Err("table generator input type must stay private and non-derived".into());
    }
    input_shapes(file)?;
    let mut constructors = Constructors {
        count: 0,
        unsafe_code: false,
    };
    constructors.visit_file(file);
    let mut tables = Vec::new();
    for item in &file.items {
        let syn::Item::Static(item) = item else {
            continue;
        };
        let syn::Expr::Struct(spec) = &*item.expr else {
            continue;
        };
        if !spec.path.is_ident("TableSpec") {
            continue;
        }
        if !matches!(item.mutability, syn::StaticMutability::None) {
            return Err("mutable SQL table specification".into());
        }
        let table = literal(field(spec, "table")?)?;
        let mut names = Vec::new();
        let mut projections = Vec::new();
        for column in &array(field(spec, "columns")?)?.elems {
            let syn::Expr::Struct(column) = column else {
                return Err("unknown column specification".into());
            };
            let name = literal(field(column, "name")?)?;
            projections.push(format!(
                "{} AS {name}",
                literal(field(column, "select_expr")?)?
            ));
            names.push(name);
        }
        let mut keys = Vec::new();
        for index in &array(field(spec, "key_indices")?)?.elems {
            let syn::Expr::Lit(index) = index else {
                return Err("nonliteral key index".into());
            };
            let syn::Lit::Int(index) = &index.lit else {
                return Err("noninteger key index".into());
            };
            keys.push(
                names
                    .get(index.base10_parse::<usize>().map_err(|e| e.to_string())?)
                    .ok_or("key index outside columns")?
                    .clone(),
            );
        }
        if keys.is_empty() {
            return Err("empty SQL ordering key".into());
        }
        let projections = projections.join(", ");
        let order = keys.join(", ");
        let placeholders = (2..keys.len() + 2)
            .map(|i| format!("${i}"))
            .collect::<Vec<_>>()
            .join(", ");
        tables.push([
            format!("SELECT {projections} FROM {table} WHERE bundle_id = $1 ORDER BY {order}"),
            format!("SELECT {projections} FROM {table} WHERE bundle_id = $1 AND ({order}) > ({placeholders}) ORDER BY {order}"),
        ]);
    }
    if constructors.unsafe_code || constructors.count != tables.len() || tables.is_empty() {
        return Err("table generator has non-static constructors or unsafe code".into());
    }
    Ok(Some(
        tables
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("\u{1f}"),
    ))
}

/// This trigger builds only quoted configuration-column assignments, then binds
/// NEW as a value. Token equality closes its variable SQL construction language.
pub(super) fn config_restore_body(source: &str) -> bool {
    const BODY: &str = r"
DECLARE assignments text;
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pgokf_private.config WHERE singleton) THEN
        RETURN NEW;
    END IF;
    SELECT string_agg(format('%I = ($1).%I', attname, attname), ', ' ORDER BY attnum)
      INTO assignments
      FROM pg_catalog.pg_attribute
     WHERE attrelid = 'pgokf_private.config'::regclass
       AND attnum > 0
       AND NOT attisdropped
       AND attname <> 'singleton';
    EXECUTE format('UPDATE pgokf_private.config SET %s WHERE singleton', assignments)
      USING NEW;
    RETURN NULL;
END
";
    let tokens = |sql| {
        crate::sql::lex(sql).map(|tokens| tokens.iter().map(super::normalized).collect::<Vec<_>>())
    };
    matches!((tokens(source),tokens(BODY)),(Ok(a),Ok(b)) if a == b)
}

pub(super) fn adoption_queries(source: &str) -> Result<Option<Vec<String>>, String> {
    use crate::sql::{self, Kind};
    let tokens = sql::lex(source)?;
    let w: Vec<_> = tokens.iter().map(super::normalized).collect();
    let prefix = [
        "for",
        "v_signature",
        "in",
        "select",
        "*",
        "from",
        "(",
        "values",
    ];
    let starts: Vec<_> = w
        .windows(prefix.len())
        .enumerate()
        .filter(|(_, s)| *s == prefix)
        .map(|(i, _)| i)
        .collect();
    if starts.len() != 1 {
        return Ok(None);
    }
    // The finite SQL values loop is its only write; calls cannot receive this
    // local variable through a procedure INOUT parameter in this language.
    for (i, word) in w.iter().enumerate() {
        if word == "call"
            || word == "v_signature"
                && ((w.get(i + 1).is_some_and(|s| s == ":")
                    && w.get(i + 2).is_some_and(|s| s == "=")
                    || w.get(i + 1).is_some_and(|s| s == "=")
                        && (i == 0
                            || !["when", "if", "elsif", "and", "or", "("]
                                .contains(&w[i - 1].as_str())))
                    || i > 0 && w[i - 1] == "into"
                    || i > 1 && w[i - 2] == "into")
        {
            return Err("unproven signature assignment".into());
        }
    }
    let mut at = starts[0] + prefix.len();
    let mut values = Vec::new();
    loop {
        if w.get(at).is_none_or(|s| s != "(")
            || tokens.get(at + 1).is_none_or(|t| t.kind != Kind::String)
            || w.get(at + 2).is_none_or(|s| s != ")")
        {
            return Err("nonliteral adoption signature".into());
        }
        values.push(sql::quoted_body(&tokens[at + 1])?);
        at += 3;
        if w.get(at).is_some_and(|s| s == ",") {
            at += 1;
        } else {
            break;
        }
    }
    if w.get(at..at + 7)
        .is_none_or(|s| s != [")", "as", "signatures", "(", "signature", ")", "loop"])
    {
        return Err("unknown signature loop".into());
    }
    let expected = [
        "execute",
        "pg_catalog",
        ".",
        "format",
        "(",
        "'ALTER EXTENSION pgokf ADD FUNCTION %s'",
        ",",
        "v_signature",
        ")",
    ];
    let executions: Vec<_> = w
        .iter()
        .enumerate()
        .filter(|(_, s)| *s == "execute")
        .map(|(i, _)| i)
        .collect();
    if executions.len() != 1
        || w.get(executions[0]..executions[0] + expected.len())
            .is_none_or(|s| s != expected)
    {
        return Err("unknown signature execution".into());
    }
    Ok(Some(
        values
            .into_iter()
            .map(|v| format!("ALTER EXTENSION pgokf ADD FUNCTION {v}"))
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn adoption_contract_refuses_writes_and_changed_execution() {
        let good = "BEGIN FOR v_signature IN SELECT * FROM (VALUES ('pgokf.f()')) AS signatures(signature) LOOP EXECUTE pg_catalog.format('ALTER EXTENSION pgokf ADD FUNCTION %s', v_signature); END LOOP; END";
        assert_eq!(
            adoption_queries(good).unwrap().unwrap(),
            ["ALTER EXTENSION pgokf ADD FUNCTION pgokf.f()"]
        );
        assert!(
            adoption_queries(
                &good.replace("LOOP EXECUTE", "LOOP v_signature := 'changed'; EXECUTE")
            )
            .is_err()
        );
        assert!(adoption_queries(&good.replace("ADD FUNCTION %s", "DROP FUNCTION %s")).is_err());
        assert!(!config_restore_body("BEGIN EXECUTE unknown_sql; END"));
    }
    #[test]
    fn quoting_contract_requires_signature() {
        let body =
            syn::parse_str::<syn::Block>(r#"{ format!("\"{}\"", name.replace('"', "\"\"")) }"#)
                .unwrap();
        let good =
            syn::parse_str::<syn::Signature>("fn quote_identifier(name: &str) -> String").unwrap();
        let bad =
            syn::parse_str::<syn::Signature>("fn quote_identifier(name: Evil) -> String").unwrap();
        assert!(quoted_body(&good, &body).is_some());
        assert!(quoted_body(&bad, &body).is_none());
    }
    #[test]
    fn constructor_contract_rejects_self_qualified_and_writes() {
        let original = include_str!("../../extension/src/catalog/export.rs");
        let check = |source: &str| {
            let file = syn::parse_file(source).unwrap();
            let funcs = super::super::functions(&file);
            tables(&funcs["build_batch_query"].1, &file, &funcs)
        };
        assert!(check(original).unwrap().is_some());
        assert!(check(&original.replace("table: &'static str", "table: Evil")).is_err());
        assert!(
            check(&original.replace(
                "spec: &TableSpec, resume: bool",
                "spec: &Evil, resume: bool"
            ))
            .is_err()
        );
        for extra in [
            r#"impl TableSpec { fn evil() -> Self { Self { file_name:"evil", table:"pgokf_web.users", columns:&[], key_indices:&[0] } } }"#,
            r#"fn evil() -> TableSpec { crate::TableSpec { file_name:"evil", table:"pgokf_web.users", columns:&[], key_indices:&[0] } }"#,
            r#"fn evil(mut spec: TableSpec) { spec.table = "pgokf_web.users"; }"#,
        ] {
            assert!(check(&format!("{original}\n{extra}")).is_err(), "{extra}");
        }
    }
}
