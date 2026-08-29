// SPDX-License-Identifier: AGPL-3.0-only
//! OpenAI-compatible embeddings HTTP client.
//!
//! Speaks the `POST {endpoint}/v1/embeddings` protocol shared by OpenAI, a
//! local `text-embeddings-inference` / `llama.cpp` server, and any other
//! OpenAI-compatible service: a request body of `{"model": ..., "input":
//! [...]}` and a response body of `{"data": [{"embedding": [...], "index":
//! n}, ...]}`. The endpoint, model, and bearer API key are all injected by the
//! caller (from CLI/env) and are never stored server-side.

use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::{Deserialize, Serialize};

/// A configured client for one OpenAI-compatible embeddings endpoint.
///
/// Holds the resolved request URL, the model name, and an optional bearer
/// token. The token lives only in this process's memory — it is never written
/// to PostgreSQL or logged.
pub struct EmbeddingsClient {
    http: Client,
    url: String,
    model: String,
    api_key: Option<String>,
}

/// The request body for `POST /v1/embeddings`.
#[derive(Debug, Serialize)]
struct EmbeddingsRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

/// The response body from `POST /v1/embeddings`.
#[derive(Debug, Deserialize)]
struct EmbeddingsResponse {
    data: Vec<EmbeddingDatum>,
}

/// One embedding entry in the response, tagged with the index of the input it
/// corresponds to.
#[derive(Debug, Deserialize)]
struct EmbeddingDatum {
    index: usize,
    embedding: Vec<f32>,
}

impl EmbeddingsClient {
    /// Build a client. `endpoint` is the server base URL (for example
    /// `https://api.openai.com` or `http://127.0.0.1:8080`); the `/v1/embeddings`
    /// path is appended here so callers configure only the base.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying HTTP client cannot be constructed.
    pub fn new(endpoint: &str, model: String, api_key: Option<String>) -> Result<Self> {
        let http = Client::builder()
            .build()
            .context("failed to build the HTTP client")?;
        let url = format!("{}/v1/embeddings", endpoint.trim_end_matches('/'));
        Ok(Self {
            http,
            url,
            model,
            api_key,
        })
    }

    /// Embed a batch of input strings, returning one vector per input in the
    /// same order as `inputs`.
    ///
    /// # Errors
    ///
    /// Returns an error on transport failure, a non-success HTTP status, a
    /// malformed response body, or a response whose entry count or indices do
    /// not line up with the request.
    pub async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        let request = EmbeddingsRequest {
            model: &self.model,
            input: inputs,
        };

        let mut builder = self.http.post(&self.url).json(&request);
        if let Some(api_key) = &self.api_key {
            builder = builder.bearer_auth(api_key);
        }

        let response = builder
            .send()
            .await
            .with_context(|| format!("POST {} failed", self.url))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("embeddings endpoint returned HTTP {status}: {body}");
        }

        let parsed: EmbeddingsResponse = response
            .json()
            .await
            .context("failed to decode the embeddings response body")?;

        Self::order_by_index(parsed.data, inputs.len())
    }

    /// Reassemble the response entries into input order, validating that every
    /// index appears exactly once.
    fn order_by_index(data: Vec<EmbeddingDatum>, expected: usize) -> Result<Vec<Vec<f32>>> {
        if data.len() != expected {
            bail!(
                "embeddings endpoint returned {} vector(s) for {} input(s)",
                data.len(),
                expected,
            );
        }

        let mut ordered: Vec<Option<Vec<f32>>> = (0..expected).map(|_| None).collect();
        for datum in data {
            let slot = ordered
                .get_mut(datum.index)
                .with_context(|| format!("response index {} is out of range", datum.index))?;
            if slot.is_some() {
                bail!(
                    "embeddings endpoint returned a duplicate index {}",
                    datum.index
                );
            }
            *slot = Some(datum.embedding);
        }

        ordered
            .into_iter()
            .enumerate()
            .map(|(index, slot)| {
                slot.with_context(|| format!("embeddings response is missing index {index}"))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_by_index_reorders_shuffled_entries() {
        // Arrange: two entries returned out of order.
        let data = vec![
            EmbeddingDatum {
                index: 1,
                embedding: vec![2.0, 2.0],
            },
            EmbeddingDatum {
                index: 0,
                embedding: vec![1.0, 1.0],
            },
        ];

        // Act
        let ordered = EmbeddingsClient::order_by_index(data, 2).expect("reordering succeeds");

        // Assert: input order is restored.
        assert_eq!(ordered, vec![vec![1.0, 1.0], vec![2.0, 2.0]]);
    }

    #[test]
    fn order_by_index_rejects_a_count_mismatch() {
        // Arrange: one entry for two inputs.
        let data = vec![EmbeddingDatum {
            index: 0,
            embedding: vec![1.0],
        }];

        // Act
        let result = EmbeddingsClient::order_by_index(data, 2);

        // Assert
        assert!(result.is_err());
    }

    #[test]
    fn order_by_index_rejects_a_duplicate_index() {
        // Arrange: index 0 twice, index 1 never.
        let data = vec![
            EmbeddingDatum {
                index: 0,
                embedding: vec![1.0],
            },
            EmbeddingDatum {
                index: 0,
                embedding: vec![9.0],
            },
        ];

        // Act
        let result = EmbeddingsClient::order_by_index(data, 2);

        // Assert
        assert!(result.is_err());
    }
}
