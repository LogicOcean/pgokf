use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::links::Link;

/// Fully parsed, database-neutral representation of an OKF concept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParsedConcept {
    pub id: String,
    pub path: String,
    #[serde(rename = "type")]
    pub r#type: String,
    pub title: String,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub resource: Option<Value>,
    pub body_text: String,
    pub links: Vec<Link>,
    pub metadata: Map<String, Value>,
}
