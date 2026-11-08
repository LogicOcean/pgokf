//! `PostgreSQL` lexical boundaries. This module does not claim SQL semantic proof.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Token {
    pub kind: Kind,
    pub text: String,
    pub offset: usize,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Word,
    String,
    Identifier,
    Dollar,
    Symbol,
}
/// Tokenize SQL, preserving quoted bytes and rejecting incomplete lexical forms.
///
/// # Errors
/// Returns an error for unterminated strings, identifiers, comments or dollar bodies.
pub fn lex(sql: &str) -> Result<Vec<Token>, String> {
    let bytes = sql.as_bytes();
    let mut result = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if bytes[i..].starts_with(b"--") {
            while i < bytes.len() && !matches!(bytes[i], b'\n' | b'\r') {
                i += 1;
            }
            continue;
        }
        if bytes[i..].starts_with(b"/*") {
            i = comment_end(sql, i)?;
            continue;
        }
        let escape = matches!(bytes[i], b'e' | b'E') && bytes.get(i + 1) == Some(&b'\'');
        let unicode = matches!(bytes[i], b'u' | b'U')
            && bytes.get(i + 1) == Some(&b'&')
            && matches!(bytes.get(i + 2), Some(b'\'' | b'"'));
        let quote_at = i + usize::from(escape) + if unicode { 2 } else { 0 };
        let kind = if matches!(bytes.get(quote_at), Some(b'\'' | b'"')) {
            let quote = bytes[quote_at];
            i = quote_at + 1;
            loop {
                if i >= bytes.len() {
                    return Err(format!("unterminated quote at {start}"));
                }
                if escape && bytes[i] == b'\\' {
                    i += 2;
                } else if bytes[i] == quote {
                    i += 1;
                    if bytes.get(i) == Some(&quote) {
                        i += 1;
                    } else {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            if quote == b'"' {
                Kind::Identifier
            } else {
                Kind::String
            }
        } else if let Some(delimiter) = dollar_delimiter(&sql[i..]) {
            let body = i + delimiter.len();
            let end = sql[body..]
                .find(delimiter)
                .ok_or_else(|| format!("unterminated dollar quote at {start}"))?;
            i = body + end + delimiter.len();
            Kind::Dollar
        } else if bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] >= 128 {
            i += 1;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric()
                    || matches!(bytes[i], b'_' | b'$')
                    || bytes[i] >= 128)
            {
                i += 1;
            }
            Kind::Word
        } else {
            i += 1;
            Kind::Symbol
        };
        result.push(Token {
            kind,
            text: sql[start..i].to_owned(),
            offset: start,
        });
    }
    Ok(result)
}

fn dollar_delimiter(sql: &str) -> Option<&str> {
    let bytes = sql.as_bytes();
    if bytes.first() != Some(&b'$') {
        return None;
    }
    if bytes.get(1) == Some(&b'$') {
        return Some(&sql[..2]);
    }
    if !bytes
        .get(1)
        .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_' || *b >= 128)
    {
        return None;
    }
    let mut i = 2;
    while bytes
        .get(i)
        .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b >= 128)
    {
        i += 1;
    }
    (bytes.get(i) == Some(&b'$')).then(|| &sql[..=i])
}

pub(crate) fn comment_end(sql: &str, start: usize) -> Result<usize, String> {
    let bytes = sql.as_bytes();
    let mut depth = 1;
    let mut i = start + 2;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"/*") {
            depth += 1;
            i += 2;
        } else if bytes[i..].starts_with(b"*/") {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return Ok(i);
            }
        } else {
            i += 1;
        }
    }
    Err(format!("unterminated comment at {start}"))
}
/// Split at semicolon tokens outside comments and quotes; dollar bodies stay opaque.
/// This is lexical splitting, not a parser for SQL-standard routine bodies.
///
/// # Errors
/// Returns lexical errors from [`lex`].
pub fn statements(sql: &str) -> Result<Vec<Vec<Token>>, String> {
    let mut result = vec![];
    let mut current = vec![];
    for token in lex(sql)? {
        if token.kind == Kind::Symbol && token.text == ";" {
            if !current.is_empty() {
                result.push(std::mem::take(&mut current));
            }
        } else {
            current.push(token);
        }
    }
    if !current.is_empty() {
        result.push(current);
    }
    Ok(result)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_comments_and_dollar_do_do_not_split() {
        let sql =
            "/* outer /* ; */ done */ DO $x$ BEGIN RAISE NOTICE ';'; END $x$; SELECT '--;'; -- end";
        let split = statements(sql).unwrap();
        assert_eq!(split.len(), 2);
        assert_eq!(split[0][1].kind, Kind::Dollar);
        assert_eq!(split[1][1].text, "'--;'");
    }
    #[test]
    fn standard_strings_do_not_use_backslash_escaping() {
        assert_eq!(statements("SELECT '\\'; SELECT 2;").unwrap().len(), 2);
        assert_eq!(
            statements("SELECT E'\\\';still string'; SELECT 2;")
                .unwrap()
                .len(),
            2
        );
    }
    #[test]
    fn quoted_identifiers_and_unicode_are_preserved() {
        let tokens = lex("SELECT \"a\"\"b\", café, 'a''b';").unwrap();
        assert_eq!(tokens[1].kind, Kind::Identifier);
        assert_eq!(tokens[1].text, "\"a\"\"b\"");
        assert_eq!(tokens[3].text, "café");
        assert_eq!(tokens[5].text, "'a''b'");
    }
    #[test]
    fn incomplete_lexical_forms_fail_closed() {
        for sql in ["/*", "/* /* */", "'x", "\"x", "$tag$ x", "E'x\\", "U&'x"] {
            assert!(lex(sql).is_err(), "{sql}");
        }
    }
    #[test]
    fn dollar_parameters_are_not_dollar_bodies() {
        let tokens = lex("SELECT $1, $$a;$$, $tag$x$tag$, foo$bar;").unwrap();
        assert_eq!(tokens.iter().filter(|t| t.kind == Kind::Dollar).count(), 2);
        assert!(tokens.iter().any(|t| t.text == "foo$bar"));
    }
}

#[cfg(test)]
mod line_ending_tests {
    #[test]
    fn each_postgres_line_ending_ends_comment() {
        for ending in ["\r", "\n", "\r\n"] {
            assert_eq!(
                super::statements(&format!("-- comment{ending}SELECT 2;"))
                    .unwrap()
                    .len(),
                1
            );
        }
    }
}

/// Decode a standard SQL string or dollar body for recursive analysis.
/// # Errors
/// Rejects escape/unicode strings whose interpretation is session-dependent.
pub fn quoted_body(token: &Token) -> Result<String, String> {
    if token.kind == Kind::Dollar {
        let delimiter = dollar_delimiter(&token.text).ok_or("invalid dollar body")?;
        return Ok(token.text[delimiter.len()..token.text.len() - delimiter.len()].to_owned());
    }
    if token.kind == Kind::String && token.text.starts_with('\'') {
        return Ok(token.text[1..token.text.len() - 1].replace("''", "'"));
    }
    Err("unsupported executable string encoding".into())
}

/// Recognize `PostgreSQL`'s two positions for a DO language clause.
/// The caller supplies the index of a quoted body token.
pub(crate) fn is_do_body(tokens: &[Token], index: usize) -> Result<bool, String> {
    if index > 0 && tokens[index - 1].text.eq_ignore_ascii_case("do") {
        return Ok(true);
    }
    if index >= 3
        && tokens[index - 3].text.eq_ignore_ascii_case("do")
        && tokens[index - 2].text.eq_ignore_ascii_case("language")
    {
        if !tokens[index - 1].text.eq_ignore_ascii_case("plpgsql") {
            return Err("unsupported DO language".into());
        }
        return Ok(true);
    }
    Ok(false)
}
