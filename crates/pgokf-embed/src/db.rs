// SPDX-License-Identifier: AGPL-3.0-only
//! PostgreSQL access for the embedder.
//!
//! Every statement goes through the shipped `pgokf` public surface (the
//! `pgokf.concepts` / `pgokf.concept_embedding` projections, `pgokf.get_config`,
//! and `pgokf.set_concept_embedding`) as a `pgokf_writer`-capable role. The
//! extension never computes an embedding or performs network I/O; this
//! companion does, and hands the finished vectors back through the setter.

use anyhow::{Context, Result};
use tokio_postgres::Client;

/// One concept that has no stored embedding yet, with the fields used to build
/// its embedding input text and the `file_hash` the concept carried when it
/// was read (the compare-and-set token [`store_embedding`] guards the write
/// with, so a vector computed from text a concurrent sync has since replaced is
/// rejected server-side rather than stored).
#[derive(Debug, Clone)]
pub struct PendingConcept {
    pub bundle_id: i64,
    pub concept_id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub body_text: String,
    pub file_hash: String,
}

impl PendingConcept {
    /// Build the bounded text to embed: the title, description, and body joined
    /// with blank lines, truncated to at most `max_chars` characters on a UTF-8
    /// character boundary. Empty sections are skipped so the input carries no
    /// stray blank lines.
    #[must_use]
    pub fn embedding_input(&self, max_chars: usize) -> String {
        let mut sections: Vec<&str> = Vec::with_capacity(3);
        if let Some(title) = self.title.as_deref()
            && !title.is_empty()
        {
            sections.push(title);
        }
        if let Some(description) = self.description.as_deref()
            && !description.is_empty()
        {
            sections.push(description);
        }
        if !self.body_text.is_empty() {
            sections.push(&self.body_text);
        }

        let joined = sections.join("\n\n");
        if joined.chars().count() <= max_chars {
            joined
        } else {
            joined.chars().take(max_chars).collect()
        }
    }
}

/// Read the durable `embedding_dim` configuration key through `pgokf.get_config`.
///
/// # Errors
///
/// Returns an error if the call fails or the value is missing / non-integer.
pub async fn embedding_dim(client: &Client) -> Result<i32> {
    let row = client
        .query_one(
            "SELECT (pgokf.get_config() ->> 'embedding_dim')::int AS dim",
            &[],
        )
        .await
        .context("failed to read embedding_dim from pgokf.get_config()")?;
    Ok(row.get("dim"))
}

/// Fetch every concept that has no matching `pgokf.concept_embedding` row,
/// optionally scoped to a single bundle. Ordered deterministically so runs and
/// logs are reproducible.
///
/// A sync that re-writes a concept deletes its embedding row in the same
/// transaction, so this missing-row poll covers both never-embedded concepts
/// and concepts whose text changed since their vector was stored.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn pending_concepts(
    client: &Client,
    bundle_id: Option<i64>,
) -> Result<Vec<PendingConcept>> {
    let rows = client
        .query(
            "SELECT c.bundle_id, c.id, c.title, c.description, c.body_text, c.file_hash
             FROM pgokf.concepts c
             LEFT JOIN pgokf.concept_embedding e
                 ON e.bundle_id = c.bundle_id AND e.concept_id = c.id
             WHERE e.concept_id IS NULL
               AND ($1::bigint IS NULL OR c.bundle_id = $1)
             ORDER BY c.bundle_id, c.id",
            &[&bundle_id],
        )
        .await
        .context("failed to list concepts needing an embedding")?;

    Ok(rows
        .into_iter()
        .map(|row| PendingConcept {
            bundle_id: row.get("bundle_id"),
            concept_id: row.get("id"),
            title: row.get("title"),
            description: row.get("description"),
            body_text: row.get("body_text"),
            file_hash: row.get("file_hash"),
        })
        .collect())
}

/// Store one concept's embedding through the guarded four-argument
/// `pgokf.set_concept_embedding` overload, which enforces the `pgokf_writer`
/// role, the concept's existence, and the length == `embedding_dim` invariant
/// server-side - and rejects the write with SQLSTATE `40001` when the
/// concept's `file_hash` no longer equals `expected_file_hash` (a sync changed
/// the concept while its vector was being computed).
///
/// # Errors
///
/// Returns an error if the setter call fails (a wrong length, an unknown
/// concept, an insufficient role, or the retryable `40001` hash-mismatch guard
/// all surface here; callers decide whether `40001` is retried).
pub async fn store_embedding(
    client: &Client,
    bundle_id: i64,
    concept_id: &str,
    embedding: &[f32],
    expected_file_hash: &str,
) -> Result<()> {
    client
        .execute(
            "SELECT pgokf.set_concept_embedding($1, $2, $3, $4)",
            &[&bundle_id, &concept_id, &embedding, &expected_file_hash],
        )
        .await
        .with_context(|| format!("failed to store embedding for concept '{concept_id}'"))?;
    Ok(())
}

/// Whether an error from [`store_embedding`] is the compare-and-set guard's
/// retryable rejection (SQLSTATE `40001`): the concept changed while its
/// vector was computed, so the write was refused and the row stays absent for
/// the next pass.
#[must_use]
pub fn is_stale_input_rejection(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut source = Some(error);
    while let Some(error) = source {
        if let Some(db_error) = error
            .downcast_ref::<tokio_postgres::Error>()
            .and_then(tokio_postgres::Error::as_db_error)
            && db_error.code().code() == "40001"
        {
            return true;
        }
        source = error.source();
    }
    false
}

/// Decode a real PostgreSQL ErrorResponse without requiring a running server.
/// `tokio_postgres::Error` deliberately has no public constructor.
#[cfg(test)]
pub(crate) async fn test_database_error(code: &str) -> tokio_postgres::Error {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let fields = format!("SERROR\0C{code}\0Mtest rejection\0\0").into_bytes();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut length = [0; 4];
        stream.read_exact(&mut length).unwrap();
        let mut startup = vec![0; usize::try_from(u32::from_be_bytes(length)).unwrap() - 4];
        stream.read_exact(&mut startup).unwrap();
        stream.write_all(b"E").unwrap();
        stream
            .write_all(&u32::try_from(fields.len() + 4).unwrap().to_be_bytes())
            .unwrap();
        stream.write_all(&fields).unwrap();
    });
    let error = tokio_postgres::Config::new()
        .host("127.0.0.1")
        .port(address.port())
        .user("test")
        .ssl_mode(tokio_postgres::config::SslMode::Disable)
        .connect_timeout(std::time::Duration::from_secs(5))
        .connect(tokio_postgres::NoTls)
        .await
        .err()
        .expect("server sent ErrorResponse");
    server.join().unwrap();
    assert_eq!(error.as_db_error().unwrap().code().code(), code);
    error
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stale_input_classifier_walks_context_chain_and_checks_sqlstate() {
        let error = test_database_error("40001").await;
        assert!(is_stale_input_rejection(&error));
        let wrapped = anyhow::Error::new(error)
            .context("storing concept")
            .context("embedding batch");
        assert!(is_stale_input_rejection(wrapped.as_ref()));

        for code in ["23505", "42501"] {
            let error = test_database_error(code).await;
            assert!(!is_stale_input_rejection(&error));
            let wrapped = anyhow::Error::new(error)
                .context("SQLSTATE 40001 in context is not a database code")
                .context("embedding batch");
            assert!(!is_stale_input_rejection(wrapped.as_ref()));
        }
        let plain = anyhow::anyhow!("40001").context("not a database error");
        assert!(!is_stale_input_rejection(plain.as_ref()));
    }

    fn concept(title: Option<&str>, description: Option<&str>, body: &str) -> PendingConcept {
        PendingConcept {
            bundle_id: 1,
            concept_id: "a/b".to_owned(),
            title: title.map(str::to_owned),
            description: description.map(str::to_owned),
            body_text: body.to_owned(),
            file_hash: "hash-1".to_owned(),
        }
    }

    #[test]
    fn embedding_input_joins_present_sections_with_blank_lines() {
        // Arrange
        let concept = concept(Some("Title"), Some("Desc"), "Body");

        // Act
        let input = concept.embedding_input(1000);

        // Assert
        assert_eq!(input, "Title\n\nDesc\n\nBody");
    }

    #[test]
    fn embedding_input_skips_absent_and_empty_sections() {
        // Arrange: no description, empty title.
        let concept = concept(Some(""), None, "Body");

        // Act
        let input = concept.embedding_input(1000);

        // Assert: only the body remains, no leading blank lines.
        assert_eq!(input, "Body");
    }

    #[test]
    fn embedding_input_truncates_on_a_char_boundary() {
        // Arrange: a multi-byte body longer than the bound.
        let concept = concept(None, None, "áéíóú");

        // Act: keep three characters.
        let input = concept.embedding_input(3);

        // Assert: exactly three chars, valid UTF-8, no panic on the boundary.
        assert_eq!(input, "áéí");
    }
}
