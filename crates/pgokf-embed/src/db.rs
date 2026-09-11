// SPDX-License-Identifier: AGPL-3.0-only
//! PostgreSQL access for the embedder.
//!
//! Every statement goes through the shipped `pgokf` public surface (the
//! `pgokf.concepts` / `pgokf.concept_embedding` projections, `pgokf.get_config`,
//! and `pgokf.set_concept_embedding_cas`) as a `pgokf_writer`-capable role. The
//! extension never computes an embedding or performs network I/O; this
//! companion does, and hands the finished vectors back through the
//! compare-and-set setter, which refuses to store a vector whose concept has
//! changed since this worker read it.

use anyhow::{Context, Result};
use tokio_postgres::Client;

/// One concept that needs an embedding (no stored row, or a stale one), with
/// the fields used to build its embedding input text and the `file_hash` the
/// compare-and-set store is bound to.
#[derive(Debug, Clone)]
pub struct PendingConcept {
    pub bundle_id: i64,
    pub concept_id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub body_text: String,
    /// The concept's `file_hash` at poll time. Passed to
    /// `pgokf.set_concept_embedding_cas` as the expected hash: if the concept
    /// changed while inference ran, the store is rejected (retryable) instead
    /// of writing a stale vector.
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

/// The render-contract identity this worker stamps on every vector it stores:
/// version `v1` of the `pgokf-embed` input construction (title, description,
/// body joined with blank lines, empty sections omitted), bounded at
/// `max_chars` characters. An administrator can pin the durable
/// `embedding_contract` configuration key to this value so semantic ranking
/// accepts only vectors rendered under exactly this contract.
#[must_use]
pub fn render_contract(max_chars: usize) -> String {
    format!("pgokf-embed/v1/max-chars:{max_chars}")
}

/// Hash of the exact bounded input text sent to the endpoint - the bytes of
/// the string [`PendingConcept::embedding_input`] produced, nothing else.
/// Stored as the row's `input_hash` provenance.
#[must_use]
pub fn input_hash(input: &str) -> String {
    blake3::hash(input.as_bytes()).to_hex().to_string()
}

/// The catalog's embedding policy: the expected dimension plus the optional
/// model / render-contract pins, all read through `pgokf.get_config`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingPolicy {
    pub dim: i32,
    /// The pinned `embedding_model`, or empty when unpinned (any non-NULL
    /// stored model is eligible).
    pub model: String,
    /// The pinned `embedding_contract`, or empty when unpinned.
    pub contract: String,
}

/// Read the durable embedding policy keys through `pgokf.get_config`.
///
/// # Errors
///
/// Returns an error if the call fails or a value is missing / ill-typed.
pub async fn embedding_policy(client: &Client) -> Result<EmbeddingPolicy> {
    let row = client
        .query_one(
            "SELECT (cfg ->> 'embedding_dim')::int AS dim,
                    cfg ->> 'embedding_model' AS model,
                    cfg ->> 'embedding_contract' AS contract
             FROM (SELECT pgokf.get_config() AS cfg) AS c",
            &[],
        )
        .await
        .context("failed to read the embedding policy from pgokf.get_config()")?;
    Ok(EmbeddingPolicy {
        dim: row.get("dim"),
        model: row.get("model"),
        contract: row.get("contract"),
    })
}

/// Fetch every concept whose embedding is missing or stale, optionally scoped
/// to a single bundle.
///
/// A stored row is stale when it cannot satisfy the semantic eligibility
/// predicate: no provenance at all (a legacy row: NULL `source_file_hash`,
/// `model`, or `contract`), a source hash behind the concept's current
/// `file_hash`, a dimension behind `policy.dim`, or a model/contract that a
/// pinned policy no longer matches. Ordered deterministically so runs and
/// logs are reproducible.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn pending_concepts(
    client: &Client,
    bundle_id: Option<i64>,
    policy: &EmbeddingPolicy,
) -> Result<Vec<PendingConcept>> {
    let rows = client
        .query(
            "SELECT c.bundle_id, c.id, c.title, c.description, c.body_text, c.file_hash
             FROM pgokf.concepts c
             LEFT JOIN pgokf.concept_embedding e
                 ON e.bundle_id = c.bundle_id AND e.concept_id = c.id
             WHERE (e.concept_id IS NULL
                    OR e.source_file_hash IS NULL
                    OR e.model IS NULL
                    OR e.contract IS NULL
                    OR e.source_file_hash <> c.file_hash
                    OR e.dim <> $2
                    OR ($3 <> '' AND e.model IS DISTINCT FROM $3)
                    OR ($4 <> '' AND e.contract IS DISTINCT FROM $4))
               AND ($1::bigint IS NULL OR c.bundle_id = $1)
             ORDER BY c.bundle_id, c.id",
            &[&bundle_id, &policy.dim, &policy.model, &policy.contract],
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

/// Store one concept's embedding through `pgokf.set_concept_embedding_cas`,
/// which enforces the `pgokf_writer` role, the concept's existence, and the
/// length == `embedding_dim` invariant server-side, and compare-and-sets
/// against the concept's current `file_hash`.
///
/// `expected_file_hash` is the hash the poll read; `input_hash` is
/// [`input_hash`] over the exact bounded input text that was embedded;
/// `model` and `contract` are this worker's model name and render-contract
/// identity.
///
/// # Errors
///
/// Returns an error if the setter call itself fails (a wrong length, an
/// unknown concept, or an insufficient role all surface here). A `false`
/// return is NOT an error: the concept changed while inference ran, so the
/// write was refused and the concept must be re-polled.
pub async fn store_embedding(
    client: &Client,
    concept: &PendingConcept,
    embedding: &[f32],
    input_hash: &str,
    model: &str,
    contract: &str,
) -> Result<bool> {
    let row = client
        .query_one(
            "SELECT pgokf.set_concept_embedding_cas($1, $2, $3, $4, $5, $6, $7) AS stored",
            &[
                &concept.bundle_id,
                &concept.concept_id,
                &embedding,
                &concept.file_hash,
                &input_hash,
                &model,
                &contract,
            ],
        )
        .await
        .with_context(|| {
            format!(
                "failed to store embedding for concept '{}'",
                concept.concept_id
            )
        })?;
    Ok(row.get("stored"))
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
            file_hash: "hash".to_owned(),
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

    #[test]
    fn render_contract_names_the_version_and_input_bound() {
        // Arrange / Act / Assert: the identity is deterministic in the bound.
        assert_eq!(render_contract(8000), "pgokf-embed/v1/max-chars:8000");
        assert_eq!(render_contract(4000), "pgokf-embed/v1/max-chars:4000");
    }

    #[test]
    fn input_hash_is_over_the_exact_bytes() {
        // Arrange / Act
        let hash = input_hash("Title\n\nBody");

        // Assert: the BLAKE3 hex of the exact input bytes, stable across runs.
        assert_eq!(hash, blake3::hash(b"Title\n\nBody").to_hex().to_string());
        assert_eq!(hash.len(), 64);
        // A one-byte difference is a different input.
        assert_ne!(hash, input_hash("Title\n\nBody!"));
    }
}
