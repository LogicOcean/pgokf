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
/// its embedding input text.
#[derive(Debug, Clone)]
pub struct PendingConcept {
    pub bundle_id: i64,
    pub concept_id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub body_text: String,
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

/// Apply an optional multi-tenant scope to the session, so the reader/writer
/// calls see and touch only that tenant's rows (mirrors `pgokf.tenant`
/// row-level security).
///
/// # Errors
///
/// Returns an error if the `set_config` call fails.
pub async fn set_tenant(client: &Client, tenant: &str) -> Result<()> {
    client
        .execute("SELECT set_config('pgokf.tenant', $1, false)", &[&tenant])
        .await
        .context("failed to set pgokf.tenant")?;
    Ok(())
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
/// # Errors
///
/// Returns an error if the query fails.
pub async fn pending_concepts(
    client: &Client,
    bundle_id: Option<i64>,
) -> Result<Vec<PendingConcept>> {
    let rows = client
        .query(
            "SELECT c.bundle_id, c.id, c.title, c.description, c.body_text
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
        })
        .collect())
}

/// Store one concept's embedding through `pgokf.set_concept_embedding`, which
/// enforces the `pgokf_writer` role, the concept's existence, and the length ==
/// `embedding_dim` invariant server-side.
///
/// # Errors
///
/// Returns an error if the setter call fails (a wrong length, an unknown
/// concept, or an insufficient role all surface here).
pub async fn store_embedding(
    client: &Client,
    bundle_id: i64,
    concept_id: &str,
    embedding: &[f32],
) -> Result<()> {
    client
        .execute(
            "SELECT pgokf.set_concept_embedding($1, $2, $3)",
            &[&bundle_id, &concept_id, &embedding],
        )
        .await
        .with_context(|| format!("failed to store embedding for concept '{concept_id}'"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn concept(title: Option<&str>, description: Option<&str>, body: &str) -> PendingConcept {
        PendingConcept {
            bundle_id: 1,
            concept_id: "a/b".to_owned(),
            title: title.map(str::to_owned),
            description: description.map(str::to_owned),
            body_text: body.to_owned(),
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
