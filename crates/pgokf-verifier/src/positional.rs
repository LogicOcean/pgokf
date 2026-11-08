//! Bounded source analysis for the two declared physical-order exceptions.
//!
//! SQL is tokenized before inspection; Rust is parsed as syntax, with literal,
//! constant, concat, format and checked include expansion. Unsupported dynamic
//! constructions involving a protected relation fail rather than earn proof.
use crate::sql::{self, Kind, Token};
#[path = "positional_builders.rs"]
mod builders;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use syn::parse::Parser;
use syn::visit::Visit;

pub const TABLES: [&str; 2] = ["pgokf_web.users", "pgokf_web.sessions"];
const ROOTS: [&str; 6] = [
    "crates",
    "deploy",
    "packaging",
    "skills",
    "templates",
    "examples",
];
const DYNAMIC: &str = "SELECT 1 FROM {table} LIMIT 0";

/// Reject positional SQL in the bounded supported syntax.
/// # Errors
/// Rejects unsupported expressions, whole rows, casts, stars and ordinals.
pub fn check_sql(source: &str) -> Result<(), String> {
    check_sql_inner(source, 0)
}
fn normalized(token: &Token) -> String {
    match token.kind {
        Kind::Word => token.text.to_ascii_lowercase(),
        Kind::Identifier => token.text.trim_matches('"').replace("\"\"", "\""),
        _ => token.text.clone(),
    }
}
fn relation_at(words: &[String], i: usize) -> bool {
    words.get(i).is_some_and(|s| s == "pgokf_web")
        && words.get(i + 1).is_some_and(|s| s == ".")
        && words
            .get(i + 2)
            .is_some_and(|s| matches!(s.as_str(), "users" | "sessions"))
}
#[allow(
    clippy::too_many_lines,
    reason = "Keep the bounded SQL grammar rules together for review"
)]
fn check_sql_inner(source: &str, depth: usize) -> Result<(), String> {
    if depth > 32 {
        return Err("SQL body nesting bound exceeded".into());
    }
    for tokens in sql::statements(source)? {
        let w: Vec<_> = tokens.iter().map(normalized).collect();
        // Routine bodies are executable even when the outer header does not
        // mention the relation. Ordinary strings and comments are inert.
        let definition =
            if w.get(1).is_some_and(|s| s == "or") && w.get(2).is_some_and(|s| s == "replace") {
                3
            } else {
                1
            };
        let routine = w.first().is_some_and(|s| s == "create")
            && w.get(definition)
                .is_some_and(|s| s == "function" || s == "procedure");
        if w.first().is_some_and(|s| s == "do")
            && w.windows(2)
                .any(|p| p[0] == "language" && p[1] != "plpgsql")
        {
            return Err("unsupported DO language".into());
        }
        if routine {
            let language = w
                .windows(2)
                .find(|p| p[0] == "language")
                .map(|p| p[1].as_str());
            if !matches!(language, Some("sql" | "plpgsql" | "c")) {
                return Err("unsupported routine language".into());
            }
        }
        for (i, token) in tokens.iter().enumerate() {
            let quoted = matches!(token.kind, Kind::Dollar | Kind::String);
            let executable = quoted
                && (sql::is_do_body(&tokens, i)?
                    || i > 0
                        && w[i - 1] == "as"
                        && routine
                        && !w.windows(2).any(|p| p == ["language", "c"]));
            if executable {
                check_sql_inner(&sql::quoted_body(token)?, depth + 1)?;
            }
            if token.kind == Kind::Word
                && w[i] == "execute"
                && i + 1 < tokens.len()
                && !["on", "function", "procedure"].contains(&w[i + 1].as_str())
            {
                let next = &tokens[i + 1];
                if next.kind == Kind::String {
                    let mut sql = sql::quoted_body(next)?;
                    let mut at = i + 2;
                    while tokens.get(at).is_some_and(|t| t.text == "|") {
                        if tokens.get(at + 1).is_none_or(|t| t.text != "|") {
                            return Err("unknown dynamic SQL operator".into());
                        }
                        let part = tokens.get(at + 2).ok_or("missing dynamic SQL part")?;
                        sql.push_str(&sql::quoted_body(part)?);
                        at += 3;
                    }
                    if tokens
                        .get(at)
                        .is_some_and(|t| !["using", "into"].contains(&normalized(t).as_str()))
                    {
                        return Err("unproven SQL expression".into());
                    }
                    check_sql_inner(&sql, depth + 1)?;
                } else if builders::config_restore_body(source) {
                    // The complete body contract derives quoted assignments.
                } else if let Some(queries) = builders::adoption_queries(source)? {
                    for query in queries {
                        check_sql_inner(&query, depth + 1)?;
                    }
                } else if crate::migration::check_executable_body(source).is_ok() {
                    let template = tokens
                        .get(i + 5)
                        .filter(|t| t.kind == Kind::String)
                        .ok_or("unknown dynamic format template")?;
                    check_sql_inner(
                        &sql::quoted_body(template)?.replace("%s", "pgokf_reader"),
                        depth + 1,
                    )?;
                } else {
                    return Err("unproven dynamic SQL body".into());
                }
            }
        }
        for pair in w.windows(2) {
            if ["from", "join", "into", "update", "copy", "table", "as"].contains(&pair[0].as_str())
                && ["users", "sessions"].contains(&pair[1].as_str())
            {
                return Err("unqualified protected relation/type".into());
            }
        }
        let relations: Vec<_> = (0..w.len()).filter(|i| relation_at(&w, *i)).collect();
        if relations.is_empty() {
            continue;
        }
        let query = w.iter().any(|s| {
            matches!(
                s.as_str(),
                "select" | "insert" | "update" | "delete" | "merge" | "copy" | "perform"
            )
        });
        for &i in &relations {
            let prev = i.checked_sub(1).and_then(|n| w.get(n)).map(String::as_str);
            let after = w.get(i + 3).map(String::as_str);
            if !matches!(
                prev,
                Some(
                    "from"
                        | "join"
                        | "into"
                        | "update"
                        | "copy"
                        | "table"
                        | "column"
                        | "references"
                        | "on"
                        | ","
                )
            ) && after != Some(".")
            {
                return Err("protected relation outside a named table context".into());
            }
            if matches!(prev, Some(":" | "as")) || matches!(after, Some("[" | "%")) || routine {
                return Err("protected composite type use".into());
            }
            if prev == Some("table")
                && w.first().is_some_and(|s| {
                    s != "create" && s != "alter" && s != "comment" && s != "grant" && s != "revoke"
                })
            {
                return Err("TABLE whole-row command".into());
            }
            if prev == Some("copy")
                || (prev == Some("into") && w.first().is_some_and(|s| s == "insert"))
            {
                let mut next = i + 3;
                if w.get(next).is_some_and(|s| s == "as") {
                    next += 2;
                }
                if w.get(next).is_none_or(|s| s != "(")
                    || w.get(next + 1).is_none_or(|s| {
                        matches!(s.as_str(), "select" | "values" | "with" | "table")
                    })
                {
                    return Err("positional INSERT/COPY".into());
                }
            }
        }
        if !query {
            continue;
        }
        let mut aliases = BTreeSet::new();
        for &i in &relations {
            aliases.insert(w[i + 2].clone());
            let mut j = i + 3;
            if w.get(j).is_some_and(|s| s == "as") {
                j += 1;
            }
            if let Some(candidate) = w.get(j)
                && (tokens[j].kind == Kind::Identifier
                    || tokens[j].kind == Kind::Word
                        && ![
                            "where",
                            "set",
                            "on",
                            "join",
                            "limit",
                            "order",
                            "group",
                            "returning",
                            "values",
                            "default",
                            "using",
                        ]
                        .contains(&candidate.as_str()))
            {
                aliases.insert(candidate.clone());
            }
        }
        for (i, word) in w.iter().enumerate() {
            if tokens[i].kind == Kind::String || tokens[i].kind == Kind::Dollar {
                continue;
            }
            if word == "*"
                && !(i >= 2
                    && w[i - 1] == "("
                    && w[i - 2] == "count"
                    && w.get(i + 1).is_some_and(|s| s == ")"))
            {
                return Err("star or record expansion".into());
            }
            if [
                "row",
                "row_to_json",
                "to_json",
                "to_jsonb",
                "json_populate_record",
                "jsonb_populate_record",
                "json_populate_recordset",
                "jsonb_populate_recordset",
                "hstore",
                "record_out",
            ]
            .contains(&word.as_str())
                && w.get(i + 1).is_some_and(|s| s == "(")
            {
                return Err("whole-row operation".into());
            }
            if word == "insert" && w.get(i + 1).is_some_and(|s| s == "values") {
                return Err("positional MERGE insert".into());
            }
            if word == "by" && i > 0 && matches!(w[i - 1].as_str(), "order" | "group") {
                let mut j = i + 1;
                let mut nesting = 0usize;
                while j < w.len() {
                    if nesting == 0
                        && ["limit", "offset", "having", "where", ")"].contains(&w[j].as_str())
                    {
                        break;
                    }
                    if w[j] == "(" {
                        nesting += 1;
                    }
                    if w[j] == ")" {
                        nesting = nesting.saturating_sub(1);
                    }
                    if w[j].chars().all(|c| c.is_ascii_digit()) {
                        return Err("ordinal ordering/grouping".into());
                    }
                    j += 1;
                }
            }
            if aliases.contains(word) {
                let declaration = relations
                    .iter()
                    .any(|r| i == r + 2 || i == r + 3 || i == r + 4 && w[r + 3] == "as");
                let field = w.get(i + 1).is_some_and(|s| s == ".");
                if !declaration && !field {
                    return Err("whole-row alias reference".into());
                }
            }
            if word == "{" || word == "}" {
                return Err("unresolved SQL template".into());
            }
        }
    }
    Ok(())
}

fn is_sql(value: &str) -> bool {
    let Ok(tokens) = sql::lex(value) else {
        return protected(value)
            && value
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("select");
    };
    tokens.first().is_some_and(|t| {
        [
            "values", "explain", "call", "execute", "select", "insert", "update", "delete", "with",
            "copy", "table", "merge", "create", "alter", "do", "grant", "revoke", "comment",
        ]
        .contains(&normalized(t).as_str())
    })
}
fn protected(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    TABLES.iter().any(|t| lower.contains(t))
        || lower.contains("{table}")
        || lower.contains("pgokf_web\"")
        || lower.ends_with("pgokf_web.")
}
fn normalize_sql(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Finite alternatives use an internal separator that source literals cannot contain.
/// Cartesian composition is bounded before allocating each resulting value.
fn combine(
    left: &str,
    right: &str,
    operation: impl Fn(&str, &str) -> String,
) -> Result<String, String> {
    let mut values = BTreeSet::new();
    let mut bytes = 0usize;
    for a in left.split('\u{1f}') {
        for b in right.split('\u{1f}') {
            if values.len() >= 256 || a.len().saturating_add(b.len()) > 4_000_000 {
                return Err("SQL alternative bound exceeded".into());
            }
            let value = operation(a, b);
            bytes = bytes.saturating_add(value.len());
            if bytes > 4_000_000 {
                return Err("SQL expansion size bound exceeded".into());
            }
            values.insert(value);
        }
    }
    Ok(values.into_iter().collect::<Vec<_>>().join("\u{1f}"))
}

#[derive(Clone)]
struct Strings<'a> {
    constants: BTreeMap<String, String>,
    module_constants: BTreeMap<String, String>,
    values: BTreeSet<String>,
    errors: Vec<String>,
    directory: &'a Path,
    root: &'a Path,
    protected_source: bool,
    functions: std::rc::Rc<BTreeMap<String, (syn::Signature, syn::Block)>>,
    parameters: BTreeSet<String>,
    sinks: BTreeMap<String, usize>,
    closures: BTreeMap<String, syn::ExprClosure>,
    numeric_fields: BTreeSet<(String, String)>,
    variable_types: BTreeMap<String, String>,
    arrays: BTreeMap<String, Vec<syn::Expr>>,
    unresolved: BTreeMap<String, String>,
    source_file: &'a syn::File,
}
impl Strings<'_> {
    #[allow(
        clippy::too_many_lines,
        reason = "Exhaustive closed expression grammar with explicit refusal default"
    )]
    fn evaluate(&self, expression: &syn::Expr, depth: usize) -> Result<String, String> {
        if depth > 32 {
            return Err("Rust constant recursion bound".into());
        }
        match expression {
            syn::Expr::Lit(value) => match &value.lit {
                syn::Lit::Str(s) => {
                    let value = s.value();
                    if value.contains(['\u{1f}', '\u{1d}']) {
                        return Err("reserved source control character".into());
                    }
                    Ok(value)
                }
                syn::Lit::Int(i) => Ok(i.base10_digits().to_owned()),
                syn::Lit::Float(f) => Ok(f.base10_digits().to_owned()),
                syn::Lit::Char(c) => Ok(c.value().to_string()),
                _ => Err("unsupported literal".into()),
            },
            syn::Expr::Path(path) => {
                if path.path.segments.len() > 1
                    && let Some(file) = self.qualified_file(&path.path)?
                {
                    let name = &path.path.segments.last().ok_or("empty path")?.ident;
                    for item in &file.items {
                        if let syn::Item::Const(c) = item
                            && c.ident == *name
                        {
                            return self.evaluate(&c.expr, depth + 1);
                        }
                    }
                }
                path.path
                    .get_ident()
                    .and_then(|i| self.constants.get(&i.to_string()))
                    .cloned()
                    .ok_or_else(|| {
                        format!(
                            "unresolved constant {}: {}",
                            path.path
                                .get_ident()
                                .map(ToString::to_string)
                                .unwrap_or_default(),
                            path.path
                                .get_ident()
                                .and_then(|id| self.unresolved.get(&id.to_string()))
                                .map_or("unknown input", String::as_str)
                        )
                    })
            }
            syn::Expr::Reference(e) => self.evaluate(&e.expr, depth + 1),
            syn::Expr::Paren(e) => self.evaluate(&e.expr, depth + 1),
            syn::Expr::Await(e) => self.evaluate(&e.base, depth + 1),
            syn::Expr::Try(e) => self.evaluate(&e.expr, depth + 1),
            syn::Expr::MethodCall(e)
                if ["to_owned", "to_string", "as_str", "into"]
                    .contains(&e.method.to_string().as_str())
                    && e.args.is_empty() =>
            {
                self.evaluate(&e.receiver, depth + 1)
            }
            syn::Expr::MethodCall(e)
                if ["prepare", "prepare_typed"].contains(&e.method.to_string().as_str()) =>
            {
                self.evaluate(e.args.first().ok_or("missing prepare input")?, depth + 1)
            }
            syn::Expr::Binary(e) if matches!(e.op, syn::BinOp::Add(_)) => {
                let left = self.evaluate(&e.left, depth + 1)?;
                let right = self.evaluate(&e.right, depth + 1)?;
                combine(&left, &right, |a, b| format!("{a}{b}"))
            }

            syn::Expr::Block(e) => self.evaluate_block(&e.block, depth + 1),
            syn::Expr::If(e) => {
                let left = self.evaluate_block(&e.then_branch, depth + 1)?;
                let right = self.evaluate(
                    &e.else_branch.as_ref().ok_or("SQL branch missing else")?.1,
                    depth + 1,
                )?;
                Ok(format!("{left}\u{1f}{right}"))
            }
            syn::Expr::Match(e) => {
                let values: Result<Vec<_>, _> = e.arms.iter().filter(|a| !diverges(&a.body) && !matches!(&*a.body, syn::Expr::Macro(m) if m.mac.path.is_ident("bail") || m.mac.path.is_ident("panic"))).map(|a| self.evaluate(&a.body, depth+1)).collect();
                let values = values?;
                if values.is_empty() {
                    return Err("no SQL-producing match arm".into());
                }
                Ok(values.join("\u{1f}"))
            }
            syn::Expr::MethodCall(e) if e.method == "replace" && e.args.len() == 2 => {
                let value = self.evaluate(&e.receiver, depth + 1)?;
                let from = self.evaluate(&e.args[0], depth + 1)?;
                let to = self.evaluate(&e.args[1], depth + 1)?;
                let mut results = BTreeSet::new();
                for pattern in from.split('\u{1f}') {
                    let expanded = combine(&value, &to, |v, t| v.replace(pattern, t))?;
                    results.extend(expanded.split('\u{1f}').map(str::to_owned));
                    if results.len() > 256 {
                        return Err("SQL alternative bound exceeded".into());
                    }
                }
                Ok(results.into_iter().collect::<Vec<_>>().join("\u{1f}"))
            }
            syn::Expr::MethodCall(e) if self.functions.contains_key(&e.method.to_string()) => {
                self.evaluate_function(&e.method.to_string(), e.args.iter().collect(), depth + 1)
            }
            syn::Expr::Call(e) => {
                let syn::Expr::Path(path) = &*e.func else {
                    return Err("unresolved SQL function".into());
                };
                let name = path
                    .path
                    .segments
                    .last()
                    .ok_or("empty function")?
                    .ident
                    .to_string();
                if path.path.segments.len() > 1
                    && let Some(imported) = self.qualified_file(&path.path)?
                {
                    let mut scope = self.clone();
                    scope.functions = std::rc::Rc::new(functions(&imported));
                    scope.module_constants.clear();
                    if let Some((signature, _)) = scope.functions.get(&name)
                        && let syn::ReturnType::Type(_, ty) = &signature.output
                        && numeric_result(ty)
                    {
                        return Ok("0".into());
                    }
                    for item in &imported.items {
                        if let syn::Item::Const(c) = item
                            && let Ok(value) = scope.evaluate(&c.expr, depth + 1)
                        {
                            scope.module_constants.insert(c.ident.to_string(), value);
                        }
                    }
                    return scope.evaluate_function(&name, e.args.iter().collect(), depth + 1);
                }
                self.evaluate_function(&name, e.args.iter().collect(), depth + 1)
            }
            syn::Expr::Field(e) => {
                let variable = expression_ident(&e.base).ok_or("unresolved field base")?;
                let ty = self
                    .variable_types
                    .get(&variable)
                    .ok_or("unknown field type")?;
                let syn::Member::Named(field) = &e.member else {
                    return Err("unsupported field".into());
                };
                if self
                    .numeric_fields
                    .contains(&(ty.clone(), field.to_string()))
                {
                    Ok("0".into())
                } else {
                    Err("non-numeric dynamic field".into())
                }
            }
            syn::Expr::MethodCall(e) if e.method == "collect" && e.args.is_empty() => {
                self.evaluate(&e.receiver, depth + 1)
            }
            syn::Expr::MethodCall(e)
                if e.method == "map"
                    && e.args.len() == 1
                    && matches!(&*e.receiver, syn::Expr::Paren(p) if matches!(&*p.expr,syn::Expr::Range(_))) =>
            {
                let name = expression_ident(&e.args[0]).ok_or("unknown numeric range closure")?;
                let closure = self.closures.get(&name).ok_or("unresolved range closure")?;
                let mut scope = self.clone();
                for parameter in &closure.inputs {
                    let syn::Pat::Type(t) = parameter else {
                        return Err("range closure needs explicit integer input".into());
                    };
                    if !numeric_type(&t.ty) {
                        return Err("non-integer range input".into());
                    }
                    let syn::Pat::Ident(id) = &*t.pat else {
                        return Err("unknown range parameter".into());
                    };
                    scope.constants.insert(id.ident.to_string(), "0".into());
                }
                let branch = scope.evaluate(&closure.body, depth + 1)?;
                for choice in branch.split('\u{1f}') {
                    check_sql(choice)?;
                    if !choice
                        .trim_start()
                        .to_ascii_lowercase()
                        .starts_with("select")
                        || choice.contains(';')
                    {
                        return Err("range builder must produce a SELECT branch".into());
                    }
                }
                Ok(format!("\u{1d}{branch}"))
            }
            syn::Expr::MethodCall(e) if e.method == "join" && e.args.len() == 1 => {
                let value = self.evaluate(&e.receiver, depth + 1)?;
                let separator = self.evaluate(&e.args[0], depth + 1)?;
                let branch = value
                    .strip_prefix('\u{1d}')
                    .ok_or("unproven repeated SQL collection")?;
                if !separator.trim().eq_ignore_ascii_case("UNION ALL") {
                    return Err("unsupported SQL repetition boundary".into());
                }
                combine(branch, branch, |a, b| format!("{a}{separator}{b}"))
            }
            syn::Expr::Binary(e)
                if matches!(
                    e.op,
                    syn::BinOp::Add(_)
                        | syn::BinOp::Sub(_)
                        | syn::BinOp::Mul(_)
                        | syn::BinOp::Div(_)
                        | syn::BinOp::Rem(_)
                ) =>
            {
                let left = self.evaluate(&e.left, depth + 1)?;
                let right = self.evaluate(&e.right, depth + 1)?;
                if left.parse::<i128>().is_ok() && right.parse::<i128>().is_ok() {
                    Ok("0".into())
                } else {
                    Err("non-integer dynamic arithmetic".into())
                }
            }
            syn::Expr::Macro(m) => {
                let name = m
                    .mac
                    .path
                    .segments
                    .last()
                    .ok_or("empty macro path")?
                    .ident
                    .to_string();
                let args =
                    syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated
                        .parse2(m.mac.tokens.clone())
                        .map_err(|e| e.to_string())?;
                if name == "concat" {
                    let mut result = String::new();
                    for arg in &args {
                        result = combine(&result, &self.evaluate(arg, depth + 1)?, |a, b| {
                            format!("{a}{b}")
                        })?;
                    }
                    return Ok(result);
                }
                if name == "include_str" {
                    if args.len() != 1 {
                        return Err("include requires one literal path".into());
                    }
                    let name = self.evaluate(&args[0], depth + 1)?;
                    let path = self
                        .directory
                        .join(name)
                        .canonicalize()
                        .map_err(|e| e.to_string())?;
                    if !path.starts_with(self.root) {
                        return Err("include escapes source root".into());
                    }
                    let metadata = std::fs::metadata(&path).map_err(|e| e.to_string())?;
                    if metadata.len() > 4_000_000 {
                        return Err("include size bound exceeded".into());
                    }
                    return std::fs::read_to_string(path).map_err(|e| e.to_string());
                }
                if name != "format" {
                    return Err(format!("unreviewed SQL macro {name}"));
                }
                let format =
                    self.evaluate(args.first().ok_or("format requires template")?, depth + 1)?;
                if normalize_sql(&format) == DYNAMIC && args.len() == 1 {
                    return Ok(format);
                }
                let mut named = self.constants.clone();
                let mut positional = Vec::new();
                for arg in args.iter().skip(1) {
                    if let syn::Expr::Assign(assign) = arg {
                        let syn::Expr::Path(path) = &*assign.left else {
                            return Err("unsupported format argument".into());
                        };
                        named.insert(
                            path.path
                                .get_ident()
                                .ok_or("unsupported named argument")?
                                .to_string(),
                            self.evaluate(&assign.right, depth + 1)?,
                        );
                    } else {
                        positional.push(self.evaluate(arg, depth + 1)?);
                    }
                }
                let mut results = vec![String::new()];
                let mut rest = format.as_str();
                let mut automatic = 0;
                while let Some(start) = rest.find('{') {
                    for result in &mut results {
                        result.push_str(&rest[..start].replace("}}", "}"));
                    }
                    rest = &rest[start + 1..];
                    if let Some(tail) = rest.strip_prefix('{') {
                        for result in &mut results {
                            result.push('{');
                        }
                        rest = tail;
                        continue;
                    }
                    let (key, tail) = rest.split_once('}').ok_or("unclosed format field")?;
                    let value = if key.is_empty() {
                        let n = automatic;
                        automatic += 1;
                        positional.get(n)
                    } else if let Ok(n) = key.parse::<usize>() {
                        positional.get(n)
                    } else {
                        named.get(key)
                    };
                    let value =
                        value.ok_or_else(|| format!("unresolved SQL format field {key}"))?;
                    results = results
                        .iter()
                        .flat_map(|prefix| {
                            value.split('\u{1f}').map(move |v| format!("{prefix}{v}"))
                        })
                        .collect();
                    if results.len() > 256 {
                        return Err("SQL alternative bound exceeded".into());
                    }
                    rest = tail;
                }
                for result in &mut results {
                    result.push_str(&rest.replace("}}", "}"));
                }
                Ok(results.join("\u{1f}"))
            }
            _ => Err("unsupported dynamic SQL expression".into()),
        }
    }
    fn check_macro_identity(&mut self, node: &syn::Macro) {
        if node.path.segments.len() > 1 {
            let provider = node
                .path
                .segments
                .first()
                .map(|s| s.ident.to_string())
                .unwrap_or_default();
            if ![
                "std",
                "core",
                "alloc",
                "anyhow",
                "pgrx",
                "serde_json",
                "tokio",
                "tracing",
                "log",
            ]
            .contains(&provider.as_str())
            {
                self.errors
                    .push("unreviewed qualified generating macro".into());
            }
        }
    }
    fn macro_tokens(&mut self, tokens: proc_macro2::TokenStream, depth: usize) {
        use proc_macro2::TokenTree;
        if depth > 32 {
            self.errors.push("macro nesting bound exceeded".into());
            return;
        }
        let tokens: Vec<_> = tokens.into_iter().collect();
        let mut i = 0;
        while i < tokens.len() {
            if let TokenTree::Ident(name) = &tokens[i] {
                if i + 2 < tokens.len()
                    && matches!(&tokens[i+1], TokenTree::Punct(p) if p.as_char() == '!')
                    && matches!(&tokens[i + 2], TokenTree::Group(_))
                {
                    let stream = tokens[i..=i + 2].iter().cloned().collect();
                    if let Ok(mac) = syn::parse2::<syn::Macro>(stream) {
                        self.visit_macro(&mac);
                        i += 3;
                        continue;
                    }
                }
                if let Some(index) = self.sinks.get(&name.to_string()).copied()
                    && let Some(TokenTree::Group(group)) = tokens.get(i + 1)
                {
                    match syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated
                        .parse2(group.stream())
                    {
                        Ok(args) => self.sink(&name.to_string(), args.get(index)),
                        Err(_) => self.errors.push("unparsed SQL sink in macro".into()),
                    }
                }
            }
            match &tokens[i] {
                TokenTree::Group(group) => {
                    let whole = std::iter::once(tokens[i].clone()).collect();
                    if let Ok(block) = syn::parse2::<syn::Block>(whole) {
                        self.visit_block(&block);
                    } else if let Ok(args) =
                        syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated
                            .parse2(group.stream())
                    {
                        for arg in args {
                            self.visit_expr(&arg);
                        }
                    } else {
                        self.macro_tokens(group.stream(), depth + 1);
                    }
                }
                TokenTree::Literal(literal) => {
                    if let Ok(value) = syn::parse_str::<syn::LitStr>(&literal.to_string()) {
                        self.add(value.value());
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }
    fn sink(&mut self, name: &str, input: Option<&syn::Expr>) {
        let Some(input) = input else {
            self.errors.push(format!("missing SQL argument {name}"));
            return;
        };
        let forwarded = expression_ident(input).is_some_and(|id| self.parameters.contains(&id));
        if forwarded {
            return;
        }
        match self.evaluate(input, 0) {
            Ok(value) => self.add(value),
            Err(e) => self.errors.push(format!("unresolved SQL sink {name}: {e}")),
        }
    }
    fn qualified_file(&self, path: &syn::Path) -> Result<Option<syn::File>, String> {
        let parts: Vec<_> = path.segments.iter().map(|s| s.ident.to_string()).collect();
        if parts.len() < 2 || parts[0] == "Self" || parts[0] == "String" {
            return Ok(None);
        }
        let mut file = if parts[0] == "crate" {
            self.directory
                .ancestors()
                .find(|p| p.file_name().is_some_and(|s| s == "src"))
                .ok_or("crate source root")?
                .to_owned()
        } else {
            self.directory.to_owned()
        };
        let start = usize::from(parts[0] == "crate");
        for component in &parts[start..parts.len() - 1] {
            file.push(component);
        }
        file.set_extension("rs");
        if !file.is_file() {
            return Ok(None);
        }
        let file = file.canonicalize().map_err(|e| e.to_string())?;
        if !file.starts_with(self.root) {
            return Err("qualified Rust source escapes root".into());
        }
        Ok(Some(
            syn::parse_file(&std::fs::read_to_string(file).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?,
        ))
    }
    fn evaluate_function(
        &self,
        name: &str,
        args: Vec<&syn::Expr>,
        depth: usize,
    ) -> Result<String, String> {
        let (signature, block) = self
            .functions
            .get(name)
            .ok_or_else(|| format!("unresolved SQL builder {name}"))?;
        if ["quote_identifier", "quote_literal", "pgvector_schema"].contains(&name)
            && let Some(value) = builders::quoted_body(signature, block)
        {
            return Ok(value);
        }
        if name == "build_batch_query"
            && let Some(value) = builders::tables(block, self.source_file, &self.functions)?
        {
            return Ok(value);
        }
        let mut scope = self.clone();
        scope.constants = self.module_constants.clone();
        scope.closures.clear();
        scope.arrays.clear();
        scope.variable_types = signature_types(signature);
        let parameters: Vec<_> = signature
            .inputs
            .iter()
            .filter_map(|a| {
                if let syn::FnArg::Typed(p) = a {
                    Some(&p.pat)
                } else {
                    None
                }
            })
            .collect();
        if parameters.len() != args.len() {
            return Err("SQL builder arity".into());
        }
        let typed: Vec<_> = signature
            .inputs
            .iter()
            .filter_map(|a| {
                if let syn::FnArg::Typed(p) = a {
                    Some(&p.ty)
                } else {
                    None
                }
            })
            .collect();
        for ((pattern, value), ty) in parameters.into_iter().zip(args).zip(typed) {
            if numeric_type(ty) {
                if let syn::Pat::Ident(p) = &**pattern {
                    scope.constants.insert(p.ident.to_string(), "0".into());
                }
            } else if let syn::Pat::Ident(p) = &**pattern {
                scope.constants.remove(&p.ident.to_string());
                if let Ok(value) = self.evaluate(value, depth + 1) {
                    scope.constants.insert(p.ident.to_string(), value);
                }
            } else {
                return Err("unsupported builder parameter pattern".into());
            }
        }

        scope.evaluate_block(block, depth + 1)
    }
    fn bind(&mut self, pattern: &syn::Pat, value: &syn::Expr, depth: usize) {
        match pattern {
            syn::Pat::Type(p) => self.bind(&p.pat, value, depth + 1),
            syn::Pat::TupleStruct(p) if p.path.is_ident("Some") && p.elems.len() == 1 => {
                self.bind(&p.elems[0], value, depth + 1);
            }
            syn::Pat::Ident(p) => {
                if let syn::Expr::Closure(closure) = value {
                    self.closures.insert(p.ident.to_string(), closure.clone());
                }
                let resolved = self.evaluate(value, depth + 1);
                self.constants.remove(&p.ident.to_string());
                match resolved {
                    Ok(value) => {
                        self.constants.insert(p.ident.to_string(), value);
                    }
                    Err(e) => {
                        self.unresolved.insert(p.ident.to_string(), e);
                    }
                }
            }
            syn::Pat::Tuple(p) => {
                if let syn::Expr::Tuple(tuple) = value {
                    for (pattern, value) in p.elems.iter().zip(&tuple.elems) {
                        self.bind(pattern, value, depth + 1);
                    }
                } else if let syn::Expr::Match(m) = value {
                    let mut choices = BTreeMap::<String, Vec<String>>::new();
                    for arm in &m.arms {
                        let mut branch = self.clone();
                        branch.bind(pattern, &arm.body, depth + 1);
                        for element in &p.elems {
                            if let syn::Pat::Ident(name) = element
                                && let Some(v) = branch.constants.get(&name.ident.to_string())
                            {
                                choices
                                    .entry(name.ident.to_string())
                                    .or_default()
                                    .push(v.clone());
                            }
                        }
                    }
                    for (key, values) in choices {
                        self.constants.insert(key, values.join("\u{1f}"));
                    }
                }
            }
            _ => {}
        }
    }
    fn evaluate_block(&self, block: &syn::Block, depth: usize) -> Result<String, String> {
        if depth > 32 {
            return Err("SQL builder recursion bound".into());
        }
        let mut scope = self.clone();
        for (i, statement) in block.stmts.iter().enumerate() {
            match statement {
                syn::Stmt::Local(local) => {
                    if let Some(init) = &local.init {
                        scope.bind(&local.pat, &init.expr, depth + 1);
                    }
                }
                syn::Stmt::Expr(value, None) if i + 1 == block.stmts.len() => {
                    return scope.evaluate(value, depth + 1);
                }
                _ => return Err("unsupported SQL builder statement".into()),
            }
        }
        Err("SQL builder has no value".into())
    }
    fn add(&mut self, value: String) {
        if value.contains('\u{1f}') {
            for choice in value.split('\u{1f}') {
                self.add(choice.to_owned());
            }
            return;
        }

        if normalize_sql(&value) == DYNAMIC {
            for table in TABLES {
                self.values.insert(value.replace("{table}", table));
            }
        } else {
            self.values.insert(value);
        }
    }
}
impl<'ast> Visit<'ast> for Strings<'_> {
    fn visit_item_use(&mut self, node: &'ast syn::ItemUse) {
        fn inspect(tree: &syn::UseTree, prefix: &mut Vec<String>, errors: &mut Vec<String>) {
            match tree {
                syn::UseTree::Path(p) => {
                    prefix.push(p.ident.to_string());
                    inspect(&p.tree, prefix, errors);
                    prefix.pop();
                }
                syn::UseTree::Group(g) => {
                    for item in &g.items {
                        inspect(item, prefix, errors);
                    }
                }
                syn::UseTree::Glob(_) => {
                    if prefix.first().is_none_or(|p| {
                        !["super", "self", "crate", "std", "pgrx"].contains(&p.as_str())
                    }) {
                        errors.push("unreviewed wildcard import".into());
                    }
                }
                syn::UseTree::Name(n) => check_import(&n.ident.to_string(), prefix, errors),
                syn::UseTree::Rename(n) => check_import(&n.rename.to_string(), prefix, errors),
            }
        }
        fn check_import(name: &str, prefix: &[String], errors: &mut Vec<String>) {
            if [
                "format",
                "concat",
                "include_str",
                "include",
                "env",
                "Debug",
                "Clone",
                "Copy",
                "Serialize",
                "Deserialize",
                "Parser",
                "Subcommand",
                "Template",
            ]
            .contains(&name)
                && prefix.first().is_none_or(|p| {
                    ![
                        "std",
                        "core",
                        "alloc",
                        "serde",
                        "clap",
                        "askama",
                        "pulldown_cmark",
                    ]
                    .contains(&p.as_str())
                })
            {
                errors.push("unreviewed generating import".into());
            }
        }
        inspect(&node.tree, &mut Vec::new(), &mut self.errors);
    }
    fn visit_attribute(&mut self, node: &'ast syn::Attribute) {
        let name = node
            .path()
            .segments
            .last()
            .map(|p| p.ident.to_string())
            .unwrap_or_default();
        let path = node
            .path()
            .segments
            .iter()
            .map(|p| p.ident.to_string())
            .collect::<Vec<_>>()
            .join("::");
        if node.path().segments.len() > 1
            && !["pgrx::pg_schema", "tokio::main", "tokio::test"].contains(&path.as_str())
        {
            self.errors
                .push("unreviewed qualified generating attribute".into());
        }
        if name == "derive" {
            let parser = syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated;
            let approved = node.parse_args_with(parser).is_ok_and(|paths| {
                paths.iter().all(|p| {
                    (p.segments.len() == 1
                        || ["serde::Serialize", "serde::Deserialize"].contains(
                            &p.segments
                                .iter()
                                .map(|s| s.ident.to_string())
                                .collect::<Vec<_>>()
                                .join("::")
                                .as_str(),
                        ))
                        && p.segments.last().is_some_and(|s| {
                            [
                                "Debug",
                                "Clone",
                                "Copy",
                                "PartialEq",
                                "Eq",
                                "Hash",
                                "PartialOrd",
                                "Ord",
                                "Default",
                                "Serialize",
                                "Deserialize",
                                "Error",
                                "Parser",
                                "Subcommand",
                                "ValueEnum",
                                "Template",
                                "Args",
                            ]
                            .contains(&s.ident.to_string().as_str())
                        })
                })
            });
            if !approved {
                self.errors.push("unreviewed generated derive".into());
            }
        } else if ![
            "doc",
            "allow",
            "warn",
            "deny",
            "forbid",
            "cfg",
            "arg",
            "command",
            "default",
            "error",
            "must_use",
            "pg_extern",
            "pg_guard",
            "pg_schema",
            "pg_test",
            "serde",
            "source",
            "template",
            "test",
            "main",
        ]
        .contains(&name.as_str())
        {
            self.errors
                .push(format!("unreviewed generated attribute {name}"));
        }
    }
    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        let previous = self.constants.clone();
        let parameters = self.parameters.clone();
        self.parameters = parameter_names(&node.sig);
        self.variable_types = signature_types(&node.sig);
        syn::visit::visit_item_fn(self, node);
        self.constants = previous;
        self.parameters = parameters;
    }
    fn visit_block(&mut self, node: &'ast syn::Block) {
        let previous = self.constants.clone();
        syn::visit::visit_block(self, node);
        self.constants = previous
            .into_iter()
            .filter(|(name, _)| self.constants.contains_key(name))
            .collect();
    }
    fn visit_local(&mut self, node: &'ast syn::Local) {
        if let Some(init) = &node.init {
            self.bind(&node.pat, &init.expr, 0);
        }
        syn::visit::visit_local(self, node);
    }
    fn visit_expr_for_loop(&mut self, node: &'ast syn::ExprForLoop) {
        let values = match &*node.expr {
            syn::Expr::Array(a) => Some(a.elems.iter().cloned().collect::<Vec<_>>()),
            syn::Expr::Path(p) => p
                .path
                .get_ident()
                .and_then(|id| self.arrays.get(&id.to_string()).cloned()),
            _ => None,
        };
        if let Some(values) = values {
            let previous = self.constants.clone();
            for value in values {
                self.bind(&node.pat, &value, 0);
                self.visit_block(&node.body);
            }
            self.constants = previous;
        } else {
            syn::visit::visit_expr_for_loop(self, node);
        }
    }
    fn visit_expr_reference(&mut self, node: &'ast syn::ExprReference) {
        if node.mutability.is_some()
            && let Some(name) = expression_ident(&node.expr)
        {
            self.constants.remove(&name);
            self.parameters.remove(&name);
        }
        syn::visit::visit_expr_reference(self, node);
    }
    fn visit_expr_binary(&mut self, node: &'ast syn::ExprBinary) {
        if matches!(
            node.op,
            syn::BinOp::AddAssign(_)
                | syn::BinOp::SubAssign(_)
                | syn::BinOp::MulAssign(_)
                | syn::BinOp::DivAssign(_)
                | syn::BinOp::RemAssign(_)
                | syn::BinOp::BitXorAssign(_)
                | syn::BinOp::BitAndAssign(_)
                | syn::BinOp::BitOrAssign(_)
                | syn::BinOp::ShlAssign(_)
                | syn::BinOp::ShrAssign(_)
        ) && let Some(name) = expression_ident(&node.left)
        {
            self.constants.remove(&name);
            self.parameters.remove(&name);
        }
        syn::visit::visit_expr_binary(self, node);
    }
    fn visit_expr_assign(&mut self, node: &'ast syn::ExprAssign) {
        if let Some(name) = expression_ident(&node.left) {
            self.constants.remove(&name);
            self.parameters.remove(&name);
        }
        syn::visit::visit_expr_assign(self, node);
    }
    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        let name = node.method.to_string();
        if method_sink(&name, node.args.len())
            && let Some(index) = self.sinks.get(&name).copied()
            && !node.args.is_empty()
            && !(name == "prepare" && node.args.len() != 1)
            && !(["select", "update"].contains(&name.as_str()) && node.args.len() != 3)
        {
            self.sink(&name, node.args.get(index));
        }
        if ![
            "clone",
            "to_owned",
            "to_string",
            "as_str",
            "into",
            "len",
            "is_empty",
            "get",
            "iter",
            "join",
            "replace",
        ]
        .contains(&name.as_str())
            && let Some(receiver) = expression_ident(&node.receiver)
        {
            self.constants.remove(&receiver);
            self.parameters.remove(&receiver);
        }
        syn::visit::visit_expr_method_call(self, node);
    }
    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        if let syn::Expr::Path(path) = &*node.func
            && let Some(name) = path.path.segments.last().map(|p| p.ident.to_string())
            && let Some(index) = self.sinks.get(&name).copied()
            && (!sink_names().contains_key(&name)
                || path.path.segments.iter().any(|s| s.ident == "Spi"))
        {
            self.sink(&name, node.args.get(index));
        }
        syn::visit::visit_expr_call(self, node);
    }
    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        let previous = self.constants.clone();
        let parameters = self.parameters.clone();
        self.parameters = parameter_names(&node.sig);
        self.variable_types = signature_types(&node.sig);
        syn::visit::visit_impl_item_fn(self, node);
        self.constants = previous;
        self.parameters = parameters;
    }
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        if node.attrs.iter().any(|a| {
            a.path().is_ident("cfg")
                && matches!(&a.meta, syn::Meta::List(l) if l.tokens.to_string() == "test")
        }) {
            return;
        }
        syn::visit::visit_item_mod(self, node);
    }

    fn visit_item_const(&mut self, node: &'ast syn::ItemConst) {
        let value = if let syn::Expr::Reference(r) = &*node.expr {
            &*r.expr
        } else {
            &*node.expr
        };
        if let syn::Expr::Array(a) = value {
            self.arrays
                .insert(node.ident.to_string(), a.elems.iter().cloned().collect());
        }

        if let Ok(value) = self.evaluate(&node.expr, 0) {
            self.constants.insert(node.ident.to_string(), value);
        }
        syn::visit::visit_item_const(self, node);
    }
    fn visit_lit_str(&mut self, node: &'ast syn::LitStr) {
        self.add(node.value());
    }
    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        let name = node
            .path
            .segments
            .last()
            .map(|p| p.ident.to_string())
            .unwrap_or_default();
        let expression = syn::Expr::Macro(syn::ExprMacro {
            attrs: vec![],
            mac: node.clone(),
        });
        self.check_macro_identity(node);
        let raw = node.tokens.to_string();
        let template_sql =
            syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated
                .parse2(node.tokens.clone())
                .ok()
                .and_then(|a| a.first().cloned())
                .and_then(|e| self.evaluate(&e, 0).ok())
                .is_some_and(|v| is_sql(&v));
        match name.as_str() {
            "concat" | "format" | "include_str" => match self.evaluate(&expression, 0) {
                Ok(value) => self.add(value),
                Err(error)
                    if template_sql
                        && (TABLES.iter().any(|t| raw.contains(t))
                            || self.protected_source && raw.contains("{table}"))
                        || name == "include_str" =>
                {
                    self.errors.push(error);
                }
                Err(_) => {}
            },
            "bail" | "anyhow" | "format_err" | "println" | "eprintln" | "assert" | "assert_eq"
            | "assert_ne" | "panic" | "vec" | "matches" | "json" | "write" | "writeln"
            | "format_args" | "debug_assert" | "debug_assert_eq" | "debug_assert_ne" | "ensure"
            | "unreachable" | "todo" | "select" | "join" | "try_join" | "pin" | "warning"
            | "notice" | "ereport" | "default" | "debug" | "trace" | "warn" | "error" | "info" => {
                self.macro_tokens(node.tokens.clone(), 0);
            }
            "env" | "option_env" | "include_bytes" | "file" | "line" | "column" | "module_path"
            | "stringify" | "cfg" | "pg_module_magic" | "pg_test" | "compile_error" => {}
            "include" => self
                .errors
                .push("generated Rust include is outside the closed source language".into()),
            "composite_type" => {
                if TABLES.iter().any(|t| raw.contains(t)) {
                    self.errors.push("protected composite Rust type".into());
                }
                self.macro_tokens(node.tokens.clone(), 0);
            }
            "extension_sql_file" => {
                let parser =
                    syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated;
                let value = parser
                    .parse2(node.tokens.clone())
                    .map_err(|e| e.to_string())
                    .and_then(|args| self.evaluate(args.first().ok_or("missing SQL file")?, 0));
                match value {
                    Ok(name) => {
                        let base = self.directory;
                        match base
                            .join(name)
                            .canonicalize()
                            .map_err(|e| e.to_string())
                            .and_then(|p| {
                                if p.starts_with(self.root) {
                                    std::fs::read_to_string(p).map_err(|e| e.to_string())
                                } else {
                                    Err("SQL file escapes source root".into())
                                }
                            }) {
                            Ok(value) => self.add(value),
                            Err(e) => self.errors.push(e),
                        }
                    }
                    Err(e) => self.errors.push(e),
                }
            }
            "extension_sql" => {
                let parser =
                    syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated;
                match parser
                    .parse2(node.tokens.clone())
                    .ok()
                    .and_then(|args| args.first().cloned())
                {
                    Some(first) => match self.evaluate(&first, 0) {
                        Ok(value) => self.add(value),
                        Err(e) => self.errors.push(e),
                    },
                    None => self.errors.push("unsupported extension_sql source".into()),
                }
            }
            _ => self.errors.push(format!("unreviewed source macro {name}")),
        }
    }
}

/// Extract SQL values after bounded Rust expression expansion.
/// # Errors
/// Rejects malformed Rust and unresolved protected SQL construction.
pub fn rust_strings(source: &str, directory: &Path, root: &Path) -> Result<Vec<String>, String> {
    let file = syn::parse_file(source).map_err(|e| format!("Rust syntax: {e}"))?;
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let mut visitor = Strings {
        constants: BTreeMap::new(),
        module_constants: BTreeMap::new(),
        values: BTreeSet::new(),
        errors: vec![],
        directory,
        root: &root,
        protected_source: TABLES.iter().any(|t| source.contains(t)),
        functions: std::rc::Rc::new(functions(&file)),
        parameters: BTreeSet::new(),
        sinks: infer_sinks(&functions(&file)),
        closures: BTreeMap::new(),
        numeric_fields: numeric_fields(&file),
        variable_types: BTreeMap::new(),
        arrays: BTreeMap::new(),
        unresolved: BTreeMap::new(),
        source_file: &file,
    };
    for imported in imports(&file, directory, &root)? {
        for (name, function) in functions(&imported) {
            insert_function(
                std::rc::Rc::make_mut(&mut visitor.functions),
                name,
                function,
            );
        }
        for item in &imported.items {
            if let syn::Item::Const(c) = item
                && let Ok(value) = visitor.evaluate(&c.expr, 0)
            {
                visitor
                    .constants
                    .entry(c.ident.to_string())
                    .or_insert(value);
            }
        }
    }
    visitor.sinks = infer_sinks(&visitor.functions);
    // Constants may be declared after their use. Fixed-point expansion is
    // bounded and never resolves cycles by inventing a value.
    for _ in 0..32 {
        let before = visitor.constants.clone();
        for item in &file.items {
            if let syn::Item::Const(c) = item
                && let Ok(value) = visitor.evaluate(&c.expr, 0)
            {
                visitor.constants.insert(c.ident.to_string(), value);
            }
        }
        if visitor.constants == before {
            break;
        }
    }
    visitor.module_constants = visitor.constants.clone();
    visitor.visit_file(&file);
    if !visitor.errors.is_empty() {
        return Err(visitor.errors.join("; "));
    }
    Ok(visitor.values.into_iter().collect())
}

fn files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut pending: Vec<_> = ROOTS
        .iter()
        .map(|name| root.join(name))
        .filter(|p| p.exists())
        .collect();
    let mut result = Vec::new();
    while let Some(path) = pending.pop() {
        let metadata = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if metadata.file_type().is_symlink() {
            return Err(format!("source symlink {}", path.display()));
        }
        if metadata.is_dir() {
            let relative = path.strip_prefix(root).map_err(|e| e.to_string())?;
            let parts: Vec<_> = relative.iter().filter_map(|s| s.to_str()).collect();
            if parts.get(2) == Some(&"tests") || parts.get(1) == Some(&"pgokf-verifier") {
                continue;
            }
            for child in std::fs::read_dir(path).map_err(|e| e.to_string())? {
                pending.push(child.map_err(|e| e.to_string())?.path());
            }
        } else if path
            .strip_prefix(root)
            .is_ok_and(|p| p == Path::new("crates/extension/src/pg_tests.rs"))
        {
        } else {
            if metadata.len() > 4_000_000 {
                return Err(format!("source size bound {}", path.display()));
            }
            result.push(path);
        }
    }
    result.sort();
    Ok(result)
}

fn source_values(root: &Path, path: &Path) -> Result<Vec<String>, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let Ok(source) = String::from_utf8(bytes) else {
        return Ok(vec![]);
    };
    if path.extension().is_some_and(|s| s == "rs") {
        rust_strings(&source, path.parent().ok_or("source parent")?, root)
    } else if path.extension().is_some_and(|s| s == "sql") {
        Ok(vec![source])
    } else {
        // Non-Rust executable text must not conceal protected SQL. Markdown
        // code spans/blocks are inspected too; configuration comments are inert.
        if path.extension().is_some_and(|s| s == "md") {
            let mut values = Vec::new();
            for fragment in source.split('`') {
                for unit in fragment.split(';') {
                    let unit = unit.trim().strip_prefix("sql\n").unwrap_or(unit.trim());
                    if is_sql(unit) {
                        values.push(unit.to_owned());
                    }
                }
            }
            Ok(values)
        } else {
            let executable = source
                .lines()
                .filter(|line| !line.trim_start().starts_with('#'))
                .collect::<Vec<_>>()
                .join("\n");
            if executable.to_ascii_lowercase().contains("pgokf_web") {
                Err("protected SQL in unanalyzed text source".into())
            } else {
                Ok(vec![])
            }
        }
    }
}

/// Verify every in-scope source and the exact exception/statement contracts.
/// # Errors
/// Rejects unproven source constructions and any positional use.
pub fn scan(root: &Path) -> Result<(), String> {
    let mut errors = Vec::new();
    for path in files(root)? {
        let values = match source_values(root, &path) {
            Ok(v) => v,
            Err(e) => {
                errors.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        for value in values {
            if is_sql(&value) {
                check_sql(&value).map_err(|e| {
                    format!(
                        "{}: {e}: {}",
                        path.display(),
                        normalize_sql(&value).chars().take(160).collect::<String>()
                    )
                })?;
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join(
            "
",
        ))
    }
}

/// Enumerate product statements after source expansion, deterministically.
/// # Errors
/// Rejects dynamic SQL that cannot be resolved within the source language.
pub fn product_statements(root: &Path) -> Result<Vec<String>, String> {
    let mut statements = BTreeSet::new();
    for path in files(root)? {
        let relative = path.strip_prefix(root).map_err(|e| e.to_string())?;
        let parts: Vec<_> = relative.iter().filter_map(|s| s.to_str()).collect();
        if parts.first() != Some(&"crates")
            || parts.get(1) == Some(&"extension")
            || path.extension().is_none_or(|s| s != "rs")
        {
            continue;
        }
        for value in source_values(root, &path).map_err(|e| format!("{}: {e}", path.display()))? {
            if is_sql(&value)
                && sql::lex(&value).is_ok_and(|tokens| {
                    let words: Vec<_> = tokens.iter().map(normalized).collect();
                    (0..words.len()).any(|i| relation_at(&words, i))
                })
            {
                check_sql(&value)?;
                statements.insert(normalize_sql(&value));
            }
        }
    }
    Ok(statements.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ambiguous_functions_do_not_overwrite_bindings() {
        let source = r#"fn builder() -> String { "SELECT * FROM pgokf_web.users".into() }
struct Harmless; impl Harmless { fn builder() -> String { "SELECT id FROM pgokf.concepts".into() } }
mod other { fn builder() -> String { "SELECT 1".into() } }"#;
        let functions = functions(&syn::parse_file(source).unwrap());
        assert_eq!(
            functions["builder"].1,
            syn::parse_quote!({ __ambiguous_sql_builder() })
        );
    }
    #[test]
    fn alternatives_are_cartesian_and_bounded() {
        assert_eq!(
            combine("a\u{1f}b", "c\u{1f}d", |a, b| format!("{a}{b}")).unwrap(),
            "ac\u{1f}ad\u{1f}bc\u{1f}bd"
        );
        let many = (0..257)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\u{1f}");
        assert!(combine(&many, "x", |a, b| format!("{a}{b}")).is_err());
        assert!(combine(&"x".repeat(4_000_001), "", |a, b| format!("{a}{b}")).is_err());
    }
    #[test]
    fn sql_positional_forms_refused() {
        for bad in [
            "SELECT * FROM pgokf_web.users",
            "SELECT DISTINCT * FROM pgokf_web.sessions",
            "SELECT u.* FROM pgokf_web.users u",
            "SELECT * FROM \"pgokf_web\".\"users\"",
            "UPDATE pgokf_web.users SET role=$2 RETURNING *",
            "INSERT INTO pgokf_web.users VALUES ($1)",
            "INSERT INTO pgokf_web.users (SELECT name FROM source)",
            "INSERT INTO pgokf_web.users DEFAULT VALUES",
            "COPY pgokf_web.users TO STDOUT",
            "TABLE pgokf_web.sessions",
            "SELECT $1::pgokf_web.users",
            "SELECT CAST($1 AS pgokf_web.sessions[])",
            "SELECT row_to_json(u) FROM pgokf_web.users u",
            "SELECT u FROM pgokf_web.users u",
            "SELECT (u).* FROM pgokf_web.users u",
            "SELECT ARRAY[u] FROM pgokf_web.users u",
            "SELECT ROW(u) FROM pgokf_web.users u",
            "SELECT name FROM pgokf_web.users ORDER BY 1",
            "SELECT name FROM pgokf_web.users GROUP BY 1",
            "SELECT name FROM pgokf_web.users ORDER BY coalesce(name, 'x'), 1",
            "CREATE FUNCTION f(u pgokf_web.users) RETURNS text AS $$ SELECT u.name $$ LANGUAGE sql",
            "DO $$ DECLARE r pgokf_web.sessions%ROWTYPE; BEGIN END $$",
            "MERGE INTO pgokf_web.users u USING src ON true WHEN NOT MATCHED THEN INSERT VALUES (src.name)",
        ] {
            assert!(check_sql(bad).is_err(), "{bad}");
        }
    }
    #[test]
    fn named_columns_and_inert_text_are_safe() {
        for good in [
            "SELECT name, role FROM pgokf_web.users WHERE name=$1",
            "INSERT INTO pgokf_web.users (name, role) VALUES ($1, $2)",
            "SELECT count(*) FROM pgokf_web.sessions WHERE subject=$1",
            "SELECT 'SELECT * FROM pgokf_web.users'",
            "-- SELECT * FROM pgokf_web.users\nSELECT 1",
            "SELECT u.name FROM pgokf_web.users u",
            "DELETE FROM pgokf_web.sessions WHERE nonce=$1",
        ] {
            assert!(check_sql(good).is_ok(), "{good}");
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Exceptions {
    #[serde(default)]
    #[serde(rename = "comment")]
    _comment: String,
    column_order: BTreeMap<String, Order>,
    runtime_indexes: BTreeMap<String, Index>,
    positional_access: Access,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Order {
    fresh: Vec<String>,
    declared: Vec<String>,
    #[serde(default)]
    #[serde(rename = "reason")]
    _reason: String,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    table: String,
    access_method: String,
    #[serde(default)]
    #[serde(rename = "created_by")]
    _created_by: String,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Access {
    scan_roots: Vec<String>,
    dynamic_statements: Vec<String>,
}

/// Pin the complete exceptions contract, ignoring only its declared prose fields.
/// # Errors
/// Rejects duplicate keys, widening, third orders and unknown fields.
pub fn check_exceptions(bytes: &[u8]) -> Result<(), String> {
    let exceptions: Exceptions = crate::json::from_slice(bytes)?;
    if exceptions.column_order.len() != 2
        || exceptions.runtime_indexes.len() != 2
        || exceptions.positional_access.scan_roots != ROOTS
        || exceptions.positional_access.dynamic_statements != [DYNAMIC]
    {
        return Err("exception contract changed".into());
    }
    for (table, fresh, declared) in [
        (
            TABLES[0],
            "name role password_hash display_name provider created_at updated_at",
            "name role password_hash created_at updated_at display_name provider",
        ),
        (
            TABLES[1],
            "nonce subject mode provider expires_at created_at",
            "nonce subject mode expires_at created_at provider",
        ),
    ] {
        let order = exceptions
            .column_order
            .get(table)
            .ok_or("missing exception relation")?;
        if order.fresh != fresh.split_whitespace().collect::<Vec<_>>()
            || order.declared != declared.split_whitespace().collect::<Vec<_>>()
        {
            return Err("physical order exception changed".into());
        }
    }
    for (index, table, method) in [
        ("pgokf.concepts_bm25_idx", "pgokf.concepts", "bm25"),
        (
            "pgokf.concept_embedding_hnsw_idx",
            "pgokf.concept_embedding",
            "hnsw",
        ),
    ] {
        let entry = exceptions
            .runtime_indexes
            .get(index)
            .ok_or("missing runtime index")?;
        if entry.table != table || entry.access_method != method {
            return Err("runtime index exception changed".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod source_tests {
    use super::*;
    fn check(source: &str, directory: &Path) -> Result<(), String> {
        for value in rust_strings(source, directory, directory)? {
            if is_sql(&value) {
                check_sql(&value)?;
            }
        }
        Ok(())
    }
    #[test]
    fn rust_dynamic_sources_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            "const Q: &str = concat!(\"SELECT * FROM \", \"pgokf_web.users\");",
            "const T: &str = \"pgokf_web.users\"; fn q() { format!(\"SELECT * FROM {T}\"); }",
            "fn q() { format!(\"SELECT {} FROM {}\", \"*\", \"pgokf_web.users\"); }",
            "fn q(x: &str) { format!(\"SELECT {x} FROM pgokf_web.users\"); }",
            "fn q() { arbitrary!(\"SELECT name FROM pgokf_web.users\"); }",
            "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));",
            "const Q: &str = concat!(\"SELECT CAST($1 AS \", \"pgokf_web\", \".users)\");",
        ];
        for source in cases {
            assert!(check(source, dir.path()).is_err(), "{source}");
        }
        assert!(check("const C: &str = concat!(\"name\", \", role\"); fn q() { format!(\"SELECT {C} FROM pgokf_web.users\"); }", dir.path()).is_ok());
        assert!(check("const T: &str = \"pgokf_web.users\"; fn q(table: &str) { format!(\"SELECT 1 FROM {table} LIMIT 0\"); }", dir.path()).is_ok());
    }
    #[test]
    fn included_sql_is_checked_and_missing_generated_input_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("query.sql"),
            "SELECT name FROM pgokf_web.users",
        )
        .unwrap();
        let source = "const Q: &str = include_str!(\"query.sql\");";
        assert!(check(source, dir.path()).is_ok());
        std::fs::write(
            dir.path().join("query.sql"),
            "SELECT * FROM pgokf_web.users",
        )
        .unwrap();
        assert!(check(source, dir.path()).is_err());
        assert!(
            check(
                "const Q: &str = include_str!(concat!(env!(\"OUT_DIR\"), \"/sql\"));",
                dir.path()
            )
            .is_err()
        );
    }
    #[test]
    fn exception_contract_cannot_widen() {
        let bytes = include_bytes!("../../../tests/upgrade/catalog-exceptions.json");
        assert!(check_exceptions(bytes).is_ok());
        let original: serde_json::Value = crate::json::from_slice(bytes).unwrap();
        for field in ["column_order", "runtime_indexes", "positional_access"] {
            let mut mutated = original.clone();
            mutated[field] = serde_json::json!({});
            assert!(check_exceptions(&serde_json::to_vec(&mutated).unwrap()).is_err());
        }
        assert!(check_exceptions(br#"{"column_order":{},"column_order":{}}"#).is_err());
        for (path, value) in [
            (
                "/column_order/pgokf_web.users/fresh/0",
                serde_json::json!("provider"),
            ),
            (
                "/column_order/pgokf_web.users/declared/0",
                serde_json::json!("provider"),
            ),
            (
                "/runtime_indexes/pgokf.concepts_bm25_idx/access_method",
                serde_json::json!("btree"),
            ),
            (
                "/positional_access/scan_roots/0",
                serde_json::json!("elsewhere"),
            ),
            (
                "/positional_access/dynamic_statements/0",
                serde_json::json!("SELECT * FROM {table}"),
            ),
        ] {
            let mut mutated = original.clone();
            *mutated.pointer_mut(path).unwrap() = value;
            assert!(
                check_exceptions(&serde_json::to_vec(&mutated).unwrap()).is_err(),
                "{path}"
            );
        }
    }
}

fn expression_ident(expression: &syn::Expr) -> Option<String> {
    match expression {
        syn::Expr::Path(p) => p.path.get_ident().map(ToString::to_string),
        syn::Expr::Reference(r) => expression_ident(&r.expr),
        syn::Expr::Paren(p) => expression_ident(&p.expr),
        _ => None,
    }
}
fn parameter_names(signature: &syn::Signature) -> BTreeSet<String> {
    signature
        .inputs
        .iter()
        .filter_map(|arg| {
            if let syn::FnArg::Typed(p) = arg {
                if let syn::Pat::Ident(p) = &*p.pat {
                    Some(p.ident.to_string())
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect()
}
fn sink_names() -> BTreeMap<String, usize> {
    [
        "select",
        "update",
        "open_cursor",
        "run",
        "run_with_args",
        "get_one",
        "get_one_with_args",
        "get_two",
        "get_two_with_args",
        "get_three",
        "get_three_with_args",
        "query",
        "query_one",
        "query_opt",
        "query_raw",
        "query_typed",
        "query_typed_raw",
        "execute",
        "batch_execute",
        "simple_query",
        "prepare",
        "prepare_typed",
    ]
    .into_iter()
    .map(|s| (s.to_owned(), 0))
    .collect()
}
fn insert_function(
    map: &mut BTreeMap<String, (syn::Signature, syn::Block)>,
    name: String,
    mut function: (syn::Signature, syn::Block),
) {
    if map.contains_key(&name) {
        function.0.output = syn::ReturnType::Default;
        function.1 = syn::parse_quote!({ __ambiguous_sql_builder() });
    }
    map.insert(name, function);
}
fn functions(file: &syn::File) -> BTreeMap<String, (syn::Signature, syn::Block)> {
    struct Functions(BTreeMap<String, (syn::Signature, syn::Block)>);
    impl<'ast> Visit<'ast> for Functions {
        fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
            insert_function(
                &mut self.0,
                node.sig.ident.to_string(),
                (node.sig.clone(), (*node.block).clone()),
            );
        }
        fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
            insert_function(
                &mut self.0,
                node.sig.ident.to_string(),
                (node.sig.clone(), node.block.clone()),
            );
        }
    }
    let mut visitor = Functions(BTreeMap::new());
    visitor.visit_file(file);
    visitor.0
}

fn infer_sinks(
    functions: &BTreeMap<String, (syn::Signature, syn::Block)>,
) -> BTreeMap<String, usize> {
    struct Forward<'a> {
        sinks: &'a BTreeMap<String, usize>,
        names: Vec<String>,
        found: BTreeSet<usize>,
    }
    impl<'ast> Visit<'ast> for Forward<'_> {
        fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
            if method_sink(&call.method.to_string(), call.args.len())
                && let Some(index) = self.sinks.get(&call.method.to_string())
                && let Some(name) = call.args.get(*index).and_then(expression_ident)
                && let Some(index) = self.names.iter().position(|p| *p == name)
            {
                self.found.insert(index);
            }
            syn::visit::visit_expr_method_call(self, call);
        }
    }
    let mut sinks = sink_names();
    for _ in 0..32 {
        let before = sinks.clone();
        for (name, (signature, body)) in functions {
            let names = signature
                .inputs
                .iter()
                .filter_map(|arg| {
                    if let syn::FnArg::Typed(p) = arg {
                        if let syn::Pat::Ident(p) = &*p.pat {
                            Some(p.ident.to_string())
                        } else {
                            Some(String::new())
                        }
                    } else {
                        None
                    }
                })
                .collect();
            let mut forward = Forward {
                sinks: &before,
                names,
                found: BTreeSet::new(),
            };
            forward.visit_block(body);
            if forward.found.len() == 1 {
                sinks.insert(name.clone(), *forward.found.first().unwrap_or(&0));
            }
        }
        if before == sinks {
            break;
        }
    }
    sinks
}

fn numeric_type(ty: &syn::Type) -> bool {
    matches!(ty, syn::Type::Path(p) if p.path.get_ident().is_some_and(|i| ["u8","u16","u32","u64","u128","usize","i8","i16","i32","i64","i128","isize"].contains(&i.to_string().as_str())))
}
fn imports(file: &syn::File, directory: &Path, root: &Path) -> Result<Vec<syn::File>, String> {
    fn paths(tree: &syn::UseTree, prefix: Vec<String>, out: &mut Vec<Vec<String>>) {
        match tree {
            syn::UseTree::Path(p) => {
                let mut prefix = prefix;
                prefix.push(p.ident.to_string());
                paths(&p.tree, prefix, out);
            }
            syn::UseTree::Group(g) => {
                for item in &g.items {
                    paths(item, prefix.clone(), out);
                }
            }
            syn::UseTree::Name(n) => {
                let mut prefix = prefix;
                prefix.push(n.ident.to_string());
                out.push(prefix);
            }
            _ => {}
        }
    }
    let mut names = Vec::new();
    for item in &file.items {
        if let syn::Item::Use(item) = item {
            paths(&item.tree, vec![], &mut names);
        }
    }
    let Some(src) = directory
        .ancestors()
        .find(|p| p.file_name().is_some_and(|s| s == "src"))
    else {
        return Ok(vec![]);
    };
    let mut files = BTreeSet::new();
    for name in names {
        if name.first().is_none_or(|s| s != "crate") || name.len() < 3 {
            continue;
        }
        let mut path = src.to_owned();
        for component in &name[1..name.len() - 1] {
            path.push(component);
        }
        path.set_extension("rs");
        if path.is_file() {
            let path = path.canonicalize().map_err(|e| e.to_string())?;
            if !path.starts_with(root) {
                return Err("Rust import outside source root".into());
            }
            files.insert(path);
        }
    }
    files
        .into_iter()
        .map(|p| {
            std::fs::read_to_string(p)
                .map_err(|e| e.to_string())
                .and_then(|s| syn::parse_file(&s).map_err(|e| e.to_string()))
        })
        .collect()
}
fn signature_types(signature: &syn::Signature) -> BTreeMap<String, String> {
    signature
        .inputs
        .iter()
        .filter_map(|arg| {
            let syn::FnArg::Typed(p) = arg else {
                return None;
            };
            let syn::Pat::Ident(name) = &*p.pat else {
                return None;
            };
            let ty = if let syn::Type::Reference(t) = &*p.ty {
                &*t.elem
            } else {
                &*p.ty
            };
            let syn::Type::Path(ty) = ty else {
                return None;
            };
            Some((
                name.ident.to_string(),
                ty.path.segments.last()?.ident.to_string(),
            ))
        })
        .collect()
}
fn numeric_fields(file: &syn::File) -> BTreeSet<(String, String)> {
    file.items
        .iter()
        .filter_map(|item| {
            if let syn::Item::Struct(s) = item {
                Some(s)
            } else {
                None
            }
        })
        .flat_map(|s| {
            s.fields
                .iter()
                .filter(|f| numeric_type(&f.ty))
                .filter_map(|f| {
                    f.ident
                        .as_ref()
                        .map(|name| (s.ident.to_string(), name.to_string()))
                })
        })
        .collect()
}

fn token_words(source: &str) -> Result<Vec<String>, String> {
    Ok(sql::lex(source)?
        .iter()
        .filter(|t| t.text != ";")
        .map(normalized)
        .collect())
}
/// Extract explicit table columns from SQL, respecting nested expressions.
/// # Errors
/// Rejects absent/duplicate definitions and malformed column lists.
pub fn table_columns(source: &str, table: &str) -> Result<Vec<String>, String> {
    let relation = token_words(table)?;
    let mut found = Vec::new();
    for statement in sql::statements(source)? {
        let words: Vec<_> = statement.iter().map(normalized).collect();
        if !words.starts_with(&["create".into(), "table".into()]) {
            continue;
        }
        if words.get(2..2 + relation.len()) != Some(relation.as_slice()) {
            continue;
        }
        let start = 2 + relation.len();
        if words.get(start).is_none_or(|s| s != "(") {
            return Err("missing table column list".into());
        }
        let mut depth = 1;
        let mut column_start = true;
        let mut columns = Vec::new();
        for word in words.iter().skip(start + 1) {
            if word == ")" {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            if column_start && depth == 1 {
                if ![
                    "constraint",
                    "primary",
                    "unique",
                    "check",
                    "foreign",
                    "exclude",
                ]
                .contains(&word.as_str())
                {
                    columns.push(word.clone());
                }
                column_start = false;
            }
            if word == "(" {
                depth += 1;
            }
            if word == "," && depth == 1 {
                column_start = true;
            }
        }
        if depth != 0 {
            return Err("unclosed table columns".into());
        }
        found.push(columns);
    }
    if found.len() != 1 {
        return Err("exactly one protected table definition required".into());
    }
    Ok(found.remove(0))
}

/// Prove the live-shaped fixture includes all recorded changes and seven stale comments.
/// # Errors
/// Rejects missing mutations, comments hidden in literals and unchanged comments.
pub fn check_live_drift_fixture(fixture: &str, fresh: &str) -> Result<(), String> {
    let statements = |source: &str| {
        sql::statements(source).map(|s| {
            s.iter()
                .map(|tokens| tokens.iter().map(normalized).collect::<Vec<_>>())
                .collect::<Vec<_>>()
        })
    };
    let drifted = statements(fixture)?;
    let fresh = statements(fresh)?;
    for prefix in [
        "ALTER EXTENSION pgokf ADD FUNCTION pgokf.concept_search_semantic(real[], bigint, integer)",
        "ALTER EXTENSION pgokf ADD FUNCTION pgokf.concept_search_hybrid(text, real[], bigint, integer)",
        "ALTER EXTENSION pgokf ADD FUNCTION pgokf.bm25_hits(text, bigint, bigint, text, text, text[], text, text, real, bigint, text)",
        "ALTER EXTENSION pgokf DROP FUNCTION pgokf.bm25_hits(text, bigint, bigint, text, text, text[], text, text, real, bigint, text, text[])",
        "COMMENT ON FUNCTION pgokf.get_skill(bigint, text) IS",
        "COMMENT ON FUNCTION pgokf.get_script(bigint, text) IS",
        "COMMENT ON FUNCTION pgokf.get_reference(bigint, text, boolean) IS",
        "ALTER TABLE pgokf_web.users DROP COLUMN display_name, DROP COLUMN provider",
        "UPDATE pgokf_web.users AS u SET display_name = d.display_name, provider = d.provider",
        "ALTER TABLE pgokf_web.sessions DROP COLUMN provider",
        "UPDATE pgokf_web.sessions AS s SET provider = d.provider",
    ] {
        let prefix = token_words(prefix)?;
        if !drifted.iter().any(|s| s.starts_with(&prefix)) {
            return Err(format!("live drift lacks {prefix:?}"));
        }
    }
    for suffix in [
        "SCHEMA pgokf_web",
        "TABLE pgokf_web.users",
        "COLUMN pgokf_web.users.name",
        "COLUMN pgokf_web.users.role",
        "COLUMN pgokf_web.users.password_hash",
        "COLUMN pgokf_web.users.created_at",
        "COLUMN pgokf_web.users.updated_at",
    ] {
        let prefix = token_words(&format!("COMMENT ON {suffix} IS"))?;
        let old: Vec<_> = drifted.iter().filter(|s| s.starts_with(&prefix)).collect();
        let current: Vec<_> = fresh.iter().filter(|s| s.starts_with(&prefix)).collect();
        if old.len() != 1 || current.len() != 1 || old == current {
            return Err(format!("live drift lacks superseded {suffix} comment"));
        }
    }
    Ok(())
}
fn method_sink(name: &str, arity: usize) -> bool {
    ![
        "run",
        "run_with_args",
        "get_one",
        "get_one_with_args",
        "get_two",
        "get_two_with_args",
        "get_three",
        "get_three_with_args",
    ]
    .contains(&name)
        && (!(name == "select" || name == "update") || arity == 3)
}
fn diverges(expression: &syn::Expr) -> bool {
    match expression {
        syn::Expr::Return(_) => true,
        syn::Expr::Block(b) => b
            .block
            .stmts
            .last()
            .is_some_and(|s| matches!(s, syn::Stmt::Expr(e, _) if diverges(e))),
        _ => false,
    }
}
fn numeric_result(ty: &syn::Type) -> bool {
    numeric_type(ty)
        || matches!(ty, syn::Type::Path(p) if p.path.segments.last().is_some_and(|s| s.ident == "Result" || s.ident == "Option") && p.path.segments.last().is_some_and(|s| matches!(&s.arguments, syn::PathArguments::AngleBracketed(a) if a.args.first().is_some_and(|arg| matches!(arg, syn::GenericArgument::Type(t) if numeric_result(t))))))
}
