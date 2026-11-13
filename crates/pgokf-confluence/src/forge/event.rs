// SPDX-License-Identifier: AGPL-3.0-only
//! Decode the Confluence event bodies that Forge delivers to a remote.
//!
//! Forge wraps each event in a JSON envelope whose `payload` holds the
//! documented event fields (`eventType`, `atlassianId`, `eventCreatedDate`,
//! `selfGenerated`, `content`, and for tasks `task`/`oldTask`) plus a
//! `retryContext` on redelivery. Only the fields this companion acts on are
//! read; unknown fields are ignored so additive changes on Atlassian's side
//! never break delivery.

use serde::Deserialize;

use super::printable;
use crate::client::is_confluence_id;

/// Why a delivery body could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError(String);

/// Shown with control characters escaped: a reason can quote the body, and
/// each one is logged as one line.
impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "undecodable Forge event: {}", printable(&self.0))
    }
}

impl std::error::Error for DecodeError {}

/// What a delivery turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decoded {
    /// An event this companion acts on.
    Event(Box<Delivery>),
    /// An event the app caused itself (publishing, adding an approval task);
    /// acknowledged and dropped so the app's own writes never echo back.
    SelfGenerated,
    /// A well-formed event this companion does not act on, named by its
    /// event type (and, for content on something other than a page, why).
    Unsupported(String),
}

/// One decoded event and how many times Forge has already tried to deliver it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub event: Event,
    /// Zero on the first attempt.
    pub retry_count: u32,
}

/// A Confluence change that concerns one page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub kind: EventKind,
    /// The account ID of the person who caused the event; absent for
    /// anonymous actions.
    pub actor: Option<String>,
    /// When Confluence recorded the event, exactly as sent (ISO 8601).
    pub occurred_at: String,
    /// The page the event concerns: the page itself, or the page holding the
    /// comment or task.
    pub page: PageRef,
}

/// The kind of change, with the details each kind carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    Page(PageChange),
    Comment {
        change: CommentChange,
        comment_id: String,
        /// `footer` or `inline`, when Confluence says.
        location: Option<String>,
    },
    Task {
        change: TaskChange,
        task: Task,
        /// The task before the change; present only for updates.
        previous: Option<Task>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageChange {
    Created,
    Updated,
    Moved,
    Copied,
    Archived,
    Unarchived,
    Trashed,
    Restored,
    Deleted,
    PermissionsUpdated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentChange {
    Created,
    Updated,
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskChange {
    Created,
    Updated,
    Removed,
}

/// The page an event concerns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRef {
    pub id: String,
    pub space_id: String,
    /// The page version the event saw, when the payload carries it.
    pub version: Option<u64>,
    /// `current`, `trashed`, `archived`, ...
    pub status: String,
    /// A live doc rather than a classic page.
    pub live_doc: bool,
}

/// An inline task (action item) on a page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: u64,
    pub uuid: Option<String>,
    pub complete: bool,
    /// The assignee's account ID.
    pub assignee: Option<String>,
}

impl EventKind {
    /// A stable name for the kind of change, such as `page:updated`.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Page(change) => match change {
                PageChange::Created => "page:created",
                PageChange::Updated => "page:updated",
                PageChange::Moved => "page:moved",
                PageChange::Copied => "page:copied",
                PageChange::Archived => "page:archived",
                PageChange::Unarchived => "page:unarchived",
                PageChange::Trashed => "page:trashed",
                PageChange::Restored => "page:restored",
                PageChange::Deleted => "page:deleted",
                PageChange::PermissionsUpdated => "page:permissions_updated",
            },
            Self::Comment { change, .. } => match change {
                CommentChange::Created => "comment:created",
                CommentChange::Updated => "comment:updated",
                CommentChange::Deleted => "comment:deleted",
            },
            Self::Task { change, .. } => match change {
                TaskChange::Created => "task:created",
                TaskChange::Updated => "task:updated",
                TaskChange::Removed => "task:removed",
            },
        }
    }
}

impl Event {
    /// A key that is identical for every delivery attempt of one event and
    /// differs between events, so a consumer can drop redeliveries. Forge
    /// assigns events no ID of their own; the kind of change, the page and
    /// its version, the comment or task and its state, and Confluence's own
    /// event time, and actor together identify one. The key is a BLAKE3 digest
    /// of those fields, length-prefixed so no two field lists collide.
    #[must_use]
    pub fn dedupe_key(&self) -> String {
        let version = self.page.version.map(|v| v.to_string()).unwrap_or_default();
        let detail = match &self.kind {
            EventKind::Page(_) => String::new(),
            EventKind::Comment { comment_id, .. } => comment_id.clone(),
            EventKind::Task { task, .. } => format!(
                "{}/{}/{}",
                task.id,
                task.uuid.as_deref().unwrap_or_default(),
                if task.complete {
                    "complete"
                } else {
                    "incomplete"
                }
            ),
        };
        let actor = self.actor.clone().unwrap_or_default();
        let mut hasher = blake3::Hasher::new();
        for field in [
            self.kind.name(),
            &self.page.id,
            &version,
            &self.occurred_at,
            &detail,
            &actor,
        ] {
            hasher.update(&(field.len() as u64).to_le_bytes());
            hasher.update(field.as_bytes());
        }
        hasher.finalize().to_hex().to_string()
    }
}

/// Decode one delivery body.
///
/// # Errors
/// Returns an error when the body is not the documented envelope - a JSON
/// object whose `payload` is an object - or an event this companion
/// subscribes to lacks a field it needs or names a page, space, or comment
/// by anything but a canonical Confluence ID.
pub fn decode_delivery(body: &[u8]) -> Result<Decoded, DecodeError> {
    let envelope: serde_json::Value = serde_json::from_slice(body)
        .map_err(|error| DecodeError(format!("the body is not JSON: {error}")))?;
    // Matched by hand: a derived struct would also take serde's sequence
    // form, so `[payload]` would pass for the envelope.
    let serde_json::Value::Object(mut envelope) = envelope else {
        return Err(DecodeError("the body is not a JSON object".into()));
    };
    let payload = match envelope.remove("payload") {
        Some(payload @ serde_json::Value::Object(_)) => payload,
        Some(_) => return Err(DecodeError("the `payload` is not an object".into())),
        None => return Err(DecodeError("the body has no `payload` object".into())),
    };
    let event_type = payload
        .get("eventType")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| DecodeError("the payload has no `eventType`".into()))?
        .to_owned();
    // `selfGenerated` must be a boolean when present. A value of any other
    // JSON type is treated as self-generated so a malformed flag can never
    // fail open into the echo path the filter exists to close.
    if payload
        .get("selfGenerated")
        .is_some_and(|flag| !flag.is_boolean() || flag.as_bool().unwrap_or(true))
    {
        return Ok(Decoded::SelfGenerated);
    }
    let Some(subscribed) = Subscribed::parse(&event_type) else {
        return Ok(Decoded::Unsupported(event_type));
    };
    let payload: Payload = serde_json::from_value(payload)
        .map_err(|error| DecodeError(format!("{event_type}: {error}")))?;
    let retry_count = payload.retry_context.map_or(0, |retry| retry.retry_count);
    // Part of the dedupe key, so a blank one would merge distinct events.
    let occurred_at = payload
        .event_created_date
        .filter(|date| !date.trim().is_empty())
        .ok_or_else(|| DecodeError(format!("{event_type}: no `eventCreatedDate`")))?;
    let content = payload
        .content
        .ok_or_else(|| DecodeError(format!("{event_type}: no `content`")))?;
    let (kind, page) = match subscribed {
        Subscribed::Page(change) => (EventKind::Page(change), content),
        Subscribed::Comment(change) => {
            let container = content.container.ok_or_else(|| {
                DecodeError(format!("{event_type}: the comment names no `container`"))
            })?;
            let kind = EventKind::Comment {
                change,
                comment_id: content.id,
                location: content.extensions.and_then(|e| e.location),
            };
            (kind, *container)
        }
        Subscribed::Task(change) => {
            let task = payload
                .task
                .ok_or_else(|| DecodeError(format!("{event_type}: no `task`")))?;
            let kind = EventKind::Task {
                change,
                task: task.into(),
                previous: payload.old_task.map(Task::from),
            };
            (kind, content)
        }
    };
    if page.kind != "page" {
        return Ok(Decoded::Unsupported(format!(
            "{event_type} on a {}, not a page",
            page.kind
        )));
    }
    if let EventKind::Comment { comment_id, .. } = &kind
        && !is_confluence_id(comment_id)
    {
        return Err(DecodeError(format!(
            "{event_type}: comment ID {comment_id:?} is not a Confluence ID"
        )));
    }
    let page = page.into_page_ref(&event_type)?;
    Ok(Decoded::Event(Box::new(Delivery {
        event: Event {
            kind,
            actor: payload.atlassian_id,
            occurred_at,
            page,
        },
        retry_count,
    })))
}

/// The event types the Forge app subscribes to, by what they concern.
#[derive(Clone, Copy)]
enum Subscribed {
    Page(PageChange),
    Comment(CommentChange),
    Task(TaskChange),
}

impl Subscribed {
    fn parse(event_type: &str) -> Option<Self> {
        let rest = event_type.strip_prefix("avi:confluence:")?;
        let (verb, noun) = rest.split_once(':')?;
        Some(match (noun, verb) {
            ("page", "created") => Self::Page(PageChange::Created),
            ("page", "updated") => Self::Page(PageChange::Updated),
            ("page", "moved") => Self::Page(PageChange::Moved),
            ("page", "copied") => Self::Page(PageChange::Copied),
            ("page", "archived") => Self::Page(PageChange::Archived),
            ("page", "unarchived") => Self::Page(PageChange::Unarchived),
            ("page", "trashed") => Self::Page(PageChange::Trashed),
            ("page", "restored") => Self::Page(PageChange::Restored),
            ("page", "deleted") => Self::Page(PageChange::Deleted),
            ("page", "permissions_updated") => Self::Page(PageChange::PermissionsUpdated),
            ("comment", "created") => Self::Comment(CommentChange::Created),
            ("comment", "updated") => Self::Comment(CommentChange::Updated),
            ("comment", "deleted") => Self::Comment(CommentChange::Deleted),
            ("task", "created") => Self::Task(TaskChange::Created),
            ("task", "updated") => Self::Task(TaskChange::Updated),
            ("task", "removed") => Self::Task(TaskChange::Removed),
            _ => return None,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Payload {
    atlassian_id: Option<String>,
    event_created_date: Option<String>,
    content: Option<Content>,
    task: Option<RawTask>,
    old_task: Option<RawTask>,
    retry_context: Option<RetryContext>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Content {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    sub_type: Option<String>,
    status: Option<String>,
    space: Option<Space>,
    version: Option<Version>,
    container: Option<Box<Content>>,
    extensions: Option<Extensions>,
}

impl Content {
    /// The page this content is, with its page and space IDs in the
    /// canonical form the rest of this crate requires.
    fn into_page_ref(self, event_type: &str) -> Result<PageRef, DecodeError> {
        if !is_confluence_id(&self.id) {
            return Err(DecodeError(format!(
                "{event_type}: page ID {:?} is not a Confluence ID",
                self.id
            )));
        }
        let space = self
            .space
            .ok_or_else(|| DecodeError(format!("{event_type}: page {} has no `space`", self.id)))?;
        let space_id = match space.id {
            serde_json::Value::Number(number) => number.to_string(),
            serde_json::Value::String(text) => text,
            _ => String::new(),
        };
        if !is_confluence_id(&space_id) {
            return Err(DecodeError(format!(
                "{event_type}: page {} has no usable space ID",
                self.id
            )));
        }
        let status = self.status.ok_or_else(|| {
            DecodeError(format!("{event_type}: page {} has no `status`", self.id))
        })?;
        Ok(PageRef {
            id: self.id,
            space_id,
            version: self.version.map(|v| v.number),
            status,
            live_doc: self.sub_type.as_deref() == Some("live"),
        })
    }
}

#[derive(Deserialize)]
struct Space {
    id: serde_json::Value,
}

#[derive(Deserialize)]
struct Version {
    number: u64,
}

#[derive(Deserialize)]
struct Extensions {
    location: Option<String>,
}

#[derive(Deserialize)]
struct RawTask {
    id: u64,
    uuid: Option<String>,
    status: String,
    assignee: Option<String>,
}

impl From<RawTask> for Task {
    fn from(raw: RawTask) -> Self {
        Self {
            id: raw.id,
            uuid: raw.uuid,
            complete: raw.status == "complete",
            assignee: raw.assignee,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RetryContext {
    #[serde(default)]
    retry_count: u32,
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn space() -> Value {
        json!({"id": 827_392_002, "key": "SP", "name": "Sample", "type": "global", "status": "current"})
    }

    fn page_event(event_type: &str, version: u64) -> Value {
        json!({"payload": {
            "eventType": event_type,
            "atlassianId": "4ad9aa0c52dc1b420a791d12",
            "eventCreatedDate": "2026-11-12T06:29:21.907Z",
            "suppressNotifications": false,
            "selfGenerated": false,
            "updateTrigger": "edit_page",
            "content": {
                "id": "838205441", "type": "page", "status": "current",
                "title": "A page", "space": space(),
                "version": {"number": version, "by": {"accountId": "4ad9aa0c52dc1b420a791d12"}}
            }
        }})
    }

    fn body(value: &Value) -> Vec<u8> {
        serde_json::to_vec(value).unwrap()
    }

    #[test]
    fn decode_delivery_reads_a_page_update_with_actor_and_version() {
        // Arrange
        let value = page_event("avi:confluence:updated:page", 7);

        // Act
        let decoded = decode_delivery(&body(&value)).unwrap();

        // Assert
        let Decoded::Event(delivery) = decoded else {
            panic!("expected an event, got {decoded:?}")
        };
        assert_eq!(delivery.retry_count, 0);
        assert_eq!(delivery.event.kind, EventKind::Page(PageChange::Updated));
        assert_eq!(
            delivery.event.actor.as_deref(),
            Some("4ad9aa0c52dc1b420a791d12")
        );
        assert_eq!(delivery.event.occurred_at, "2026-11-12T06:29:21.907Z");
        assert_eq!(
            delivery.event.page,
            PageRef {
                id: "838205441".into(),
                space_id: "827392002".into(),
                version: Some(7),
                status: "current".into(),
                live_doc: false,
            }
        );
    }

    #[test]
    fn decode_delivery_reads_every_subscribed_page_change() {
        // Arrange
        let cases = [
            ("created", PageChange::Created),
            ("updated", PageChange::Updated),
            ("moved", PageChange::Moved),
            ("copied", PageChange::Copied),
            ("archived", PageChange::Archived),
            ("unarchived", PageChange::Unarchived),
            ("trashed", PageChange::Trashed),
            ("restored", PageChange::Restored),
            ("deleted", PageChange::Deleted),
            ("permissions_updated", PageChange::PermissionsUpdated),
        ];

        for (verb, expected) in cases {
            // Act
            let value = page_event(&format!("avi:confluence:{verb}:page"), 1);
            let decoded = decode_delivery(&body(&value)).unwrap();

            // Assert
            let Decoded::Event(delivery) = decoded else {
                panic!("{verb}: {decoded:?}")
            };
            assert_eq!(delivery.event.kind, EventKind::Page(expected), "{verb}");
        }
    }

    #[test]
    fn decode_delivery_drops_events_the_app_caused_itself() {
        // Arrange
        let mut value = page_event("avi:confluence:updated:page", 3);
        value["payload"]["selfGenerated"] = json!(true);

        // Act
        let decoded = decode_delivery(&body(&value)).unwrap();

        // Assert
        assert_eq!(decoded, Decoded::SelfGenerated);
    }

    #[test]
    fn decode_delivery_reads_a_completed_task_with_its_previous_state() {
        // Arrange
        let value = json!({"payload": {
            "eventType": "avi:confluence:updated:task",
            "atlassianId": "reviewer-account",
            "eventCreatedDate": "2026-11-12T07:00:00Z",
            "task": {"id": 1, "uuid": "e0a0e71d-5575-4185-bf33-61364fb0960e",
                     "status": "complete", "statusAsString": "CHECKED",
                     "assignee": "reviewer-account", "dueDate": null},
            "oldTask": {"id": 1, "uuid": "e0a0e71d-5575-4185-bf33-61364fb0960e",
                        "status": "incomplete", "statusAsString": "UNCHECKED",
                        "assignee": "reviewer-account", "dueDate": null},
            "content": {"id": "838205441", "type": "page", "status": "current",
                        "title": "A page", "space": space()}
        }});

        // Act
        let decoded = decode_delivery(&body(&value)).unwrap();

        // Assert
        let Decoded::Event(delivery) = decoded else {
            panic!("{decoded:?}")
        };
        let EventKind::Task {
            change,
            task,
            previous,
        } = delivery.event.kind
        else {
            panic!("expected a task event")
        };
        assert_eq!(change, TaskChange::Updated);
        assert!(task.complete);
        assert_eq!(
            task.uuid.as_deref(),
            Some("e0a0e71d-5575-4185-bf33-61364fb0960e")
        );
        assert_eq!(task.assignee.as_deref(), Some("reviewer-account"));
        assert!(
            !previous
                .expect("an update carries the previous task")
                .complete
        );
        assert_eq!(delivery.event.actor.as_deref(), Some("reviewer-account"));
        assert_eq!(delivery.event.page.id, "838205441");
        assert_eq!(delivery.event.page.version, None);
    }

    #[test]
    fn decode_delivery_reads_a_footer_comment_against_its_page() {
        // Arrange
        let value = json!({"payload": {
            "eventType": "avi:confluence:created:comment",
            "atlassianId": "commenter",
            "eventCreatedDate": "2026-11-12T08:00:00Z",
            "content": {
                "id": "838205455", "type": "comment", "status": "current",
                "title": "Re: A page", "space": space(),
                "version": {"number": 1},
                "container": {"id": "838205441", "type": "page", "status": "current",
                              "title": "A page", "space": space(),
                              "version": {"number": 4}},
                "extensions": {"location": "footer"}
            }
        }});

        // Act
        let decoded = decode_delivery(&body(&value)).unwrap();

        // Assert
        let Decoded::Event(delivery) = decoded else {
            panic!("{decoded:?}")
        };
        assert_eq!(
            delivery.event.kind,
            EventKind::Comment {
                change: CommentChange::Created,
                comment_id: "838205455".into(),
                location: Some("footer".into()),
            }
        );
        assert_eq!(delivery.event.page.id, "838205441");
        assert_eq!(delivery.event.page.version, Some(4));
    }

    #[test]
    fn decode_delivery_marks_a_live_doc() {
        // Arrange
        let mut value = page_event("avi:confluence:updated:page", 2);
        value["payload"]["content"]["subType"] = json!("live");

        // Act
        let decoded = decode_delivery(&body(&value)).unwrap();

        // Assert
        let Decoded::Event(delivery) = decoded else {
            panic!("{decoded:?}")
        };
        assert!(delivery.event.page.live_doc);
    }

    #[test]
    fn decode_delivery_reads_the_retry_count_of_a_redelivery() {
        // Arrange
        let mut value = page_event("avi:confluence:updated:page", 5);
        value["payload"]["retryContext"] =
            json!({"retryData": null, "retryCount": 2, "retryReason": "TIMEOUT_ERROR"});

        // Act
        let decoded = decode_delivery(&body(&value)).unwrap();

        // Assert
        let Decoded::Event(delivery) = decoded else {
            panic!("{decoded:?}")
        };
        assert_eq!(delivery.retry_count, 2);
    }

    #[test]
    fn decode_delivery_treats_an_unsubscribed_event_type_as_unsupported() {
        // Arrange
        let value = page_event("avi:confluence:viewed:page", 1);

        // Act
        let decoded = decode_delivery(&body(&value)).unwrap();

        // Assert
        assert_eq!(
            decoded,
            Decoded::Unsupported("avi:confluence:viewed:page".into())
        );
    }

    #[test]
    fn decode_delivery_treats_a_comment_on_a_blog_post_as_unsupported() {
        // Arrange
        let value = json!({"payload": {
            "eventType": "avi:confluence:created:comment",
            "eventCreatedDate": "2026-11-12T08:00:00Z",
            "content": {"id": "9", "type": "comment", "status": "current", "title": "c",
                        "space": space(),
                        "container": {"id": "8", "type": "blogpost", "status": "current",
                                      "title": "b", "space": space()}}
        }});

        // Act
        let decoded = decode_delivery(&body(&value)).unwrap();

        // Assert
        assert!(
            matches!(&decoded, Decoded::Unsupported(why) if why.contains("blogpost")),
            "{decoded:?}"
        );
    }

    #[test]
    fn decode_delivery_rejects_a_body_without_the_payload_envelope() {
        // Arrange
        let value = json!({"eventType": "avi:confluence:updated:page"});

        // Act
        let error = decode_delivery(&body(&value)).unwrap_err();

        // Assert
        assert!(error.to_string().contains("payload"), "{error}");
    }

    #[test]
    fn decode_delivery_rejects_a_subscribed_event_missing_its_page() {
        // Arrange
        let mut value = page_event("avi:confluence:updated:page", 1);
        value["payload"].as_object_mut().unwrap().remove("content");

        // Act
        let error = decode_delivery(&body(&value)).unwrap_err();

        // Assert
        assert!(error.to_string().contains("content"), "{error}");
    }

    #[test]
    fn decode_delivery_rejects_bytes_that_are_not_json() {
        // Arrange
        let bytes = b"not json";

        // Act
        let error = decode_delivery(bytes).unwrap_err();

        // Assert
        assert!(
            error.to_string().starts_with("undecodable Forge event"),
            "{error}"
        );
    }

    #[test]
    fn dedupe_key_is_identical_for_a_redelivery_and_distinct_for_another_event() {
        // Arrange
        let first = page_event("avi:confluence:updated:page", 7);
        let mut redelivered = first.clone();
        redelivered["payload"]["retryContext"] = json!({"retryCount": 1});
        let next_version = page_event("avi:confluence:updated:page", 8);
        let key = |value: &Value| match decode_delivery(&body(value)).unwrap() {
            Decoded::Event(delivery) => delivery.event.dedupe_key(),
            other => panic!("{other:?}"),
        };

        // Act
        let (a, b, c) = (key(&first), key(&redelivered), key(&next_version));

        // Assert
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn a_non_boolean_self_generated_flag_is_treated_as_self_generated() {
        // Arrange: a truthy value of the wrong JSON type must not fail open
        // into the echo path (the review's `evt_self_generated_string` probe).
        for value in [json!("true"), json!(1), json!(null), json!(["true"])] {
            let mut event = page_event("avi:confluence:updated:page", 7);
            event["payload"]["selfGenerated"] = value.clone();

            // Act
            let decoded = decode_delivery(&body(&event)).unwrap();

            // Assert
            assert!(
                matches!(decoded, Decoded::SelfGenerated),
                "{value:?} decoded as {decoded:?}"
            );
        }
    }

    #[test]
    fn dedupe_key_separates_events_that_differ_only_by_actor() {
        // Arrange: two versionless events on the same page in the same
        // millisecond, caused by different people (the review's
        // `evt_dedupe_distinct_events` probe).
        let event = |actor: &str| {
            let mut event = page_event("avi:confluence:updated:page", 7);
            event["payload"]["content"]
                .as_object_mut()
                .unwrap()
                .remove("version");
            event["payload"]["atlassianId"] = json!(actor);
            event
        };
        let key = |value: &Value| match decode_delivery(&body(value)).unwrap() {
            Decoded::Event(delivery) => delivery.event.dedupe_key(),
            other => panic!("{other:?}"),
        };
        let alice = key(&event("5f1a2b3c:alice"));
        let bob = key(&event("5f1a2b3c:bob"));
        let alice_again = key(&event("5f1a2b3c:alice"));

        // Act & Assert
        assert_ne!(alice, bob);
        assert_eq!(alice, alice_again);
    }

    #[test]
    fn dedupe_key_distinguishes_ticking_and_unticking_one_task() {
        // Arrange
        let task_event = |status: &str| {
            json!({"payload": {
                "eventType": "avi:confluence:updated:task",
                "atlassianId": "reviewer",
                "eventCreatedDate": "2026-11-12T07:00:00Z",
                "task": {"id": 1, "uuid": "u-1", "status": status, "assignee": null},
                "content": {"id": "838205441", "type": "page", "status": "current",
                            "title": "A page", "space": space()}
            }})
        };
        let key = |value: &Value| match decode_delivery(&body(value)).unwrap() {
            Decoded::Event(delivery) => delivery.event.dedupe_key(),
            other => panic!("{other:?}"),
        };

        // Act
        let (ticked, unticked) = (key(&task_event("complete")), key(&task_event("incomplete")));

        // Assert
        assert_ne!(ticked, unticked);
    }
}
