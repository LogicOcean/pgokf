// SPDX-License-Identifier: AGPL-3.0-only
//! Shared, credential-free Confluence import records.
use serde::{Deserialize, Serialize};

/// A current page fetched at a verified version.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Page {
    pub id: String,
    pub space_id: String,
    pub title: String,
    pub parent_id: Option<String>,
    pub version: u64,
    pub modified_at: Option<String>,
    pub storage: String,
    pub web_url: String,
}

/// Stable identity of the Confluence space, without credentials.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SourceIdentity {
    pub site_url: String,
    pub cloud_id: Option<String>,
    pub space_id: String,
}

impl SourceIdentity {
    /// The space this identity names, as the opaque `origin` label of a
    /// catalog-change event: `confluence:<site origin>:space:<space ID>`.
    #[must_use]
    pub fn origin_label(&self) -> String {
        format!("confluence:{}:space:{}", self.site_url, self.space_id)
    }
}

/// One complete OKF document and any lossy conversion warnings.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Document {
    pub path: String,
    pub bytes: Vec<u8>,
    pub page_id: String,
    pub version: u64,
    pub warnings: Vec<String>,
}
