// SPDX-License-Identifier: AGPL-3.0-only
//! OKF documents as the human workflow handles them: the frontmatter as an
//! ordered mapping plus the Markdown body, and the provenance operations
//! the workflow performs on them. Every operation writes ordinary OKF
//! fields (`generated`, `author`, `verified`, `status`) that the catalog
//! already projects, so the trust tier and the lifecycle status a person
//! sets here are the ones every reader, agent, and plugin sees.

use std::time::{SystemTime, UNIX_EPOCH};

use okf_parser::{ParserLimits, frontmatter, parse_concept};
use serde_yaml::{Mapping, Value};

/// A document split into its two halves.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Document {
    pub frontmatter: Mapping,
    pub body: String,
}

/// The status a document takes when a reviewer sends it back.
pub(crate) const STATUS_DRAFT: &str = "draft";
/// The status an approved draft takes.
pub(crate) const STATUS_ACTIVE: &str = "active";

impl Document {
    /// Split a document: a `---` frontmatter block (a YAML mapping, possibly
    /// empty) and the body after it.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let limits = ParserLimits::default();
        let (yaml, body) = frontmatter::split(text, "document.md", limits.max_frontmatter_bytes)
            .map_err(|e| e.to_string())?;
        let frontmatter = if yaml.trim().is_empty() {
            Mapping::new()
        } else {
            match serde_yaml::from_str::<Value>(yaml).map_err(|e| format!("frontmatter: {e}"))? {
                Value::Mapping(map) => map,
                Value::Null => Mapping::new(),
                _ => return Err("frontmatter must be a YAML mapping".to_owned()),
            }
        };
        Ok(Self {
            frontmatter,
            body: body.to_owned(),
        })
    }

    /// The document as text, frontmatter first.
    pub(crate) fn render(&self) -> String {
        let yaml = if self.frontmatter.is_empty() {
            String::new()
        } else {
            serde_yaml::to_string(&Value::Mapping(self.frontmatter.clone())).unwrap_or_default()
        };
        let mut out = String::with_capacity(yaml.len() + self.body.len() + 8);
        out.push_str("---\n");
        out.push_str(&yaml);
        if !yaml.ends_with('\n') && !yaml.is_empty() {
            out.push('\n');
        }
        out.push_str("---\n");
        out.push_str(&self.body);
        out
    }

    /// Whether the catalog would accept the document at `path`.
    pub(crate) fn validate(&self, path: &str) -> Result<(), String> {
        parse_concept(self.render().as_bytes(), path, ParserLimits::default())
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    pub(crate) fn text(&self, key: &str) -> Option<&str> {
        self.frontmatter.get(key).and_then(Value::as_str)
    }

    fn set(&mut self, key: &str, value: Value) {
        self.frontmatter
            .insert(Value::String(key.to_owned()), value);
    }

    fn list_mut(&mut self, key: &str) -> &mut Vec<Value> {
        let entry = self
            .frontmatter
            .entry(Value::String(key.to_owned()))
            .or_insert_with(|| Value::Sequence(Vec::new()));
        if !entry.is_sequence() {
            *entry = Value::Sequence(Vec::new());
        }
        entry.as_sequence_mut().expect("a sequence")
    }

    /// Record who supplied the document, when the author did not: `generated`
    /// (who produced the current content) and `author` (who publishes it),
    /// both as the person's OKF actor. Returns whether anything was added.
    pub(crate) fn stamp_origin(&mut self, actor: &str, at: &str) -> bool {
        let mut changed = false;
        if !self.frontmatter.contains_key("generated") {
            self.set("generated", event(actor, at, None));
            changed = true;
        }
        if !self.frontmatter.contains_key("author") {
            self.set("author", Value::String(actor.to_owned()));
            changed = true;
        }
        changed
    }

    /// Record a human verification: the event the trust tier derives from.
    /// A draft becomes active at the same time.
    pub(crate) fn record_verification(&mut self, actor: &str, at: &str, note: Option<&str>) {
        self.list_mut("verified").push(event(actor, at, note));
        if self.text("status").is_none_or(|s| s == STATUS_DRAFT) {
            self.set("status", Value::String(STATUS_ACTIVE.to_owned()));
        }
    }

    /// Send a document back: it becomes a draft, and the review is kept
    /// under `reviews` so the author sees why.
    pub(crate) fn send_back(&mut self, actor: &str, at: &str, note: Option<&str>) {
        self.set("status", Value::String(STATUS_DRAFT.to_owned()));
        let mut review = mapping(&[("by", actor), ("at", at), ("outcome", "sent-back")]);
        if let Some(note) = note.map(str::trim).filter(|n| !n.is_empty()) {
            review.insert(
                Value::String("note".to_owned()),
                Value::String(note.to_owned()),
            );
        }
        self.list_mut("reviews").push(Value::Mapping(review));
    }

    /// Set aside every `verified` event: they move under
    /// `superseded_verifications`, each marked with who set it aside, when,
    /// and why, so the document returns to unverified. Uploads and edits
    /// both go through here, which is what keeps a verification something
    /// only an approver can grant: a `verified` list typed into a document
    /// never counts. Returns how many events were set aside.
    pub(crate) fn quarantine_verifications(
        &mut self,
        actor: &str,
        at: &str,
        reason: &str,
    ) -> usize {
        let Some(Value::Sequence(previous)) = self.frontmatter.remove("verified") else {
            return 0;
        };
        let count = previous.len();
        let superseded = self.list_mut("superseded_verifications");
        for item in previous {
            let mut entry = match item {
                Value::Mapping(m) => m,
                other => mapping(&[("event", other.as_str().unwrap_or_default())]),
            };
            for (key, value) in [
                ("superseded_by", actor),
                ("superseded_at", at),
                ("reason", reason),
            ] {
                entry.insert(
                    Value::String(key.to_owned()),
                    Value::String(value.to_owned()),
                );
            }
            superseded.push(Value::Mapping(entry));
        }
        count
    }

    /// Carry the verification record of the stored version into this
    /// (edited) version: the live `verified` events, so an editor who
    /// removed the block cannot make them vanish (they are set aside next,
    /// visibly), and the already set-aside ones, so the trail survives a
    /// full-document edit.
    pub(crate) fn inherit_verifications(&mut self, stored: &Document) {
        for key in ["verified", "superseded_verifications"] {
            let Some(Value::Sequence(previous)) = stored.frontmatter.get(key) else {
                continue;
            };
            let mine = self.list_mut(key);
            for event in previous {
                if !mine.contains(event) {
                    mine.push(event.clone());
                }
            }
        }
    }

    /// After an edit the previous verifications no longer describe the
    /// content: they are set aside (see [`Self::quarantine_verifications`])
    /// and `generated` names the editor as the producer of the current
    /// content.
    pub(crate) fn supersede_verifications(&mut self, actor: &str, at: &str) {
        self.quarantine_verifications(actor, at, "edited");
        self.set("generated", event(actor, at, None));
    }

    /// Whether a `human:` actor has verified the document.
    pub(crate) fn has_human_verification(&self) -> bool {
        self.frontmatter
            .get("verified")
            .and_then(Value::as_sequence)
            .is_some_and(|events| {
                events.iter().any(|e| {
                    e.get("by")
                        .and_then(Value::as_str)
                        .is_some_and(|by| by.starts_with("human:"))
                })
            })
    }
}

fn mapping(pairs: &[(&str, &str)]) -> Mapping {
    let mut map = Mapping::new();
    for (key, value) in pairs {
        map.insert(
            Value::String((*key).to_owned()),
            Value::String((*value).to_owned()),
        );
    }
    map
}

/// An OKF provenance event: `{by, at}` plus an optional note.
fn event(actor: &str, at: &str, note: Option<&str>) -> Value {
    let mut map = mapping(&[("by", actor), ("at", at)]);
    if let Some(note) = note.map(str::trim).filter(|n| !n.is_empty()) {
        map.insert(
            Value::String("note".to_owned()),
            Value::String(note.to_owned()),
        );
    }
    Value::Mapping(map)
}

/// The current instant as an RFC 3339 UTC timestamp with second precision.
pub(crate) fn now_iso() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    iso_from_unix(secs)
}

/// `YYYY-MM-DDTHH:MM:SSZ` for a Unix timestamp (proleptic Gregorian, the
/// civil-from-days algorithm).
pub(crate) fn iso_from_unix(secs: u64) -> String {
    let days = i64::try_from(secs / 86_400).unwrap_or(i64::MAX);
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str =
        "---\ntype: Runbook\ntitle: Failover\ntags: [ops]\n---\n\n# Failover\n\nDo it.\n";

    #[test]
    fn a_document_splits_and_renders_back_with_its_keys_in_order() {
        // Arrange / Act
        let doc = Document::parse(DOC).expect("parses");
        let text = doc.render();

        // Assert
        assert_eq!(doc.text("type"), Some("Runbook"));
        assert_eq!(doc.body, "\n# Failover\n\nDo it.\n");
        assert!(
            text.starts_with("---\ntype: Runbook\ntitle: Failover\ntags:\n- ops\n---\n"),
            "{text}"
        );
        assert!(text.ends_with("\n# Failover\n\nDo it.\n"));
        assert!(doc.validate("runbooks/failover.md").is_ok());
        assert!(Document::parse("no frontmatter\n").is_err());
        assert!(Document::parse("---\n- a list\n---\nbody\n").is_err());
    }

    #[test]
    fn an_origin_is_stamped_only_when_the_author_left_it_out() {
        // Arrange
        let mut fresh = Document::parse(DOC).expect("parses");
        let mut stamped = Document::parse(
            "---\ntype: Runbook\ntitle: F\ngenerated:\n  by: human:bob\n  at: 2026-01-01T00:00:00Z\nauthor: human:bob\n---\nx\n",
        )
        .expect("parses");

        // Act
        let changed = fresh.stamp_origin("human:alice", "2026-09-07T10:00:00Z");
        let unchanged = stamped.stamp_origin("human:alice", "2026-09-07T10:00:00Z");

        // Assert
        assert!(changed && !unchanged);
        assert_eq!(fresh.frontmatter["generated"]["by"], "human:alice");
        assert_eq!(fresh.text("author"), Some("human:alice"));
        assert_eq!(stamped.frontmatter["generated"]["by"], "human:bob");
    }

    #[test]
    fn a_verification_makes_the_document_human_reviewed_and_activates_a_draft() {
        // Arrange
        let mut doc = Document::parse("---\ntype: Runbook\ntitle: F\nstatus: draft\n---\nx\n")
            .expect("parses");
        assert!(!doc.has_human_verification());

        // Act
        doc.record_verification(
            "human:alice",
            "2026-09-07T10:00:00Z",
            Some("Checked the steps."),
        );

        // Assert
        assert!(doc.has_human_verification());
        assert_eq!(doc.text("status"), Some("active"));
        let text = doc.render();
        assert!(text.contains("verified:\n- by: human:alice\n"), "{text}");
        assert!(
            text.contains("2026-09-07T10:00:00Z") && text.contains("note: Checked the steps."),
            "{text}"
        );
        assert!(doc.validate("f.md").is_ok());
    }

    #[test]
    fn an_edit_supersedes_previous_verifications_and_names_the_editor() {
        // Arrange
        let mut doc = Document::parse(
            "---\ntype: Runbook\ntitle: F\ngenerated:\n  by: process:gen\n  at: 2026-01-01T00:00:00Z\nverified:\n  - by: human:alice\n    at: 2026-02-01T00:00:00Z\n---\nx\n",
        )
        .expect("parses");

        // Act
        doc.supersede_verifications("human:bob", "2026-09-07T10:00:00Z");

        // Assert
        assert!(!doc.has_human_verification());
        assert!(doc.frontmatter.get("verified").is_none());
        let superseded = doc.frontmatter["superseded_verifications"]
            .as_sequence()
            .expect("kept");
        assert_eq!(superseded[0]["by"], "human:alice");
        assert_eq!(superseded[0]["superseded_by"], "human:bob");
        assert_eq!(doc.frontmatter["generated"]["by"], "human:bob");
    }

    #[test]
    fn a_typed_verification_is_set_aside_not_believed() {
        // Arrange: an upload that claims a human verification, and an edit
        // that removed the stored one while adding its own.
        let mut uploaded = Document::parse(
            "---\ntype: Runbook\ntitle: F\nverified:\n  - by: human:alice\n    at: 2026-02-01T00:00:00Z\n---\nx\n",
        )
        .expect("parses");
        let stored = Document::parse(
            "---\ntype: Runbook\ntitle: F\nverified:\n  - by: human:alice\n    at: 2026-02-01T00:00:00Z\nsuperseded_verifications:\n  - by: human:old\n    at: 2025-01-01T00:00:00Z\n    reason: uploaded\n---\nx\n",
        )
        .expect("parses");
        let mut edited = Document::parse(
            "---\ntype: Runbook\ntitle: F\nverified:\n  - by: human:mallory\n    at: 2026-03-01T00:00:00Z\n---\ny\n",
        )
        .expect("parses");

        // Act
        let set_aside =
            uploaded.quarantine_verifications("human:bob", "2026-09-07T10:00:00Z", "uploaded");
        edited.inherit_verifications(&stored);
        edited.supersede_verifications("human:carol", "2026-09-07T10:00:00Z");

        // Assert
        assert_eq!(set_aside, 1);
        assert!(!uploaded.has_human_verification());
        assert_eq!(
            uploaded.frontmatter["superseded_verifications"][0]["reason"],
            "uploaded"
        );
        assert!(!edited.has_human_verification());
        let kept = edited.frontmatter["superseded_verifications"]
            .as_sequence()
            .expect("kept");
        assert_eq!(
            kept.len(),
            3,
            "the earlier trail, the stored event, and the typed one"
        );
        assert!(
            kept.iter().any(|e| e["by"] == "human:old"),
            "the trail survives the edit"
        );
        assert!(
            kept.iter().any(|e| e["by"] == "human:alice")
                && kept.iter().any(|e| e["by"] == "human:mallory")
        );
    }

    #[test]
    fn sending_back_drafts_the_document_and_keeps_the_review() {
        // Arrange
        let mut doc = Document::parse(DOC).expect("parses");

        // Act
        doc.send_back(
            "human:alice",
            "2026-09-07T10:00:00Z",
            Some("  Missing the rollback step. "),
        );

        // Assert
        assert_eq!(doc.text("status"), Some("draft"));
        let review = &doc.frontmatter["reviews"][0];
        assert_eq!(review["outcome"], "sent-back");
        assert_eq!(review["note"], "Missing the rollback step.");
    }

    #[test]
    fn timestamps_render_as_utc_rfc3339() {
        // Arrange / Act / Assert
        assert_eq!(iso_from_unix(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_from_unix(1_788_796_800), "2026-09-07T16:00:00Z");
        assert_eq!(iso_from_unix(951_782_400), "2000-02-29T00:00:00Z");
        assert!(now_iso().ends_with('Z'));
    }
}
