//! Display groupings over concept type strings.
//!
//! The catalog stores concept types as free-form strings set by each
//! producer, so the schema has no type-group concept: grouping is a
//! presentation concern of the web UI. This module is a label layer over the
//! types a query actually observes (the `type` facet buckets), never a
//! closed list: a known type is shown under its group's label, and every
//! type no known group claims lands under "Other", so a new producer's types
//! are always visible and filterable instead of silently disappearing.
//!
//! Group selection travels in its own `type_group` parameter, never encoded
//! inside the legacy `type` parameter, so every exact type string keeps
//! working verbatim - no prefix is reserved and no producer type can be
//! reinterpreted as a group.

use crate::db::Facet;

/// The group every unclaimed observed type falls into.
pub(crate) const OTHER_SLUG: &str = "other";
pub(crate) const OTHER_LABEL: &str = "Other";

/// The known groups as `(slug, label, member type strings)`, in sidebar
/// display order. Membership is a default labelling, not an exhaustive
/// registry: [`groups_from_facets`] buckets only observed types, and
/// [`expand_group`] filters membership down to what the catalog actually
/// holds.
const KNOWN_GROUPS: &[(&str, &str, &[&str])] = &[
    ("code", "Code", &["Code Entity"]),
    (
        "documents",
        "Documents",
        &[
            "Bundle Index",
            "Dashboard",
            "Guide",
            "Reference",
            "Runbook",
            "Script",
            "Skill",
        ],
    ),
];

/// The slug of the group a type string displays under.
pub(crate) fn slug_of(concept_type: &str) -> &'static str {
    KNOWN_GROUPS
        .iter()
        .find(|(_, _, members)| members.contains(&concept_type))
        .map_or(OTHER_SLUG, |(slug, _, _)| *slug)
}

/// The display label of a group slug, for result summaries.
pub(crate) fn label_of(slug: &str) -> String {
    if slug == OTHER_SLUG {
        return OTHER_LABEL.to_owned();
    }
    KNOWN_GROUPS
        .iter()
        .find(|(s, _, _)| *s == slug)
        .map_or_else(|| slug.to_owned(), |(_, label, _)| (*label).to_owned())
}

/// Whether the slug names a group the UI offers ("Other" included).
pub(crate) fn is_known_slug(slug: &str) -> bool {
    slug == OTHER_SLUG || KNOWN_GROUPS.iter().any(|(s, _, _)| *s == slug)
}

/// Expand a group slug into the exact types to filter by: the group's
/// member types that were actually observed, in the static member order for
/// known groups and in observation order for "Other" (the observed types no
/// known group claims). `None` for a slug that names no group. An empty
/// expansion is a filter that matches nothing, never "no filter".
pub(crate) fn expand_group(slug: &str, observed: &[String]) -> Option<Vec<String>> {
    if slug == OTHER_SLUG {
        let mut types: Vec<String> = observed
            .iter()
            .filter(|t| slug_of(t) == OTHER_SLUG)
            .cloned()
            .collect();
        types.dedup();
        return Some(types);
    }
    KNOWN_GROUPS
        .iter()
        .find(|(s, _, _)| *s == slug)
        .map(|(_, _, members)| {
            members
                .iter()
                .filter(|m| observed.iter().any(|t| t == *m))
                .map(|m| (*m).to_owned())
                .collect()
        })
}

/// One group's bucket of observed types, for the sidebar controls.
#[derive(Debug, Clone)]
pub(crate) struct GroupView {
    pub slug: &'static str,
    pub label: &'static str,
    /// The summed count of the observed member buckets.
    pub count: i64,
    /// The observed member types, in facet (count) order.
    pub types: Vec<Facet>,
}

/// Bucket observed type facets into their display groups, in the static
/// group order. Groups with no observed members are left out (the callers
/// re-add a selected-but-empty group so a round-tripped choice stays
/// visible); "Other" appears exactly when unknown types were observed.
pub(crate) fn groups_from_facets(observed: &[Facet]) -> Vec<GroupView> {
    let mut groups: Vec<GroupView> = KNOWN_GROUPS
        .iter()
        .map(|(slug, label, _)| GroupView {
            slug,
            label,
            count: 0,
            types: Vec::new(),
        })
        .chain(std::iter::once(GroupView {
            slug: OTHER_SLUG,
            label: OTHER_LABEL,
            count: 0,
            types: Vec::new(),
        }))
        .collect();
    for facet in observed {
        let slug = slug_of(&facet.value);
        let group = groups
            .iter_mut()
            .find(|g| g.slug == slug)
            .expect("slug_of only returns declared slugs");
        group.count += facet.count;
        group.types.push(facet.clone());
    }
    groups.retain(|g| !g.types.is_empty());
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facet(value: &str, count: i64) -> Facet {
        Facet {
            value: value.to_owned(),
            count,
        }
    }

    #[test]
    fn slug_of_buckets_known_types_and_sends_unknowns_to_other() {
        // Arrange & Act & Assert
        assert_eq!(slug_of("Code Entity"), "code");
        assert_eq!(slug_of("Guide"), "documents");
        assert_eq!(slug_of("Skill"), "documents");
        assert_eq!(slug_of("Qualia"), OTHER_SLUG);
        assert_eq!(slug_of("Widget"), OTHER_SLUG);
    }

    #[test]
    fn groups_from_facets_buckets_observed_types_and_drops_empty_groups() {
        // Arrange
        let observed = vec![
            facet("Code Entity", 12_006),
            facet("Guide", 10),
            facet("Reference", 10),
        ];

        // Act
        let groups = groups_from_facets(&observed);

        // Assert
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].slug, "code");
        assert_eq!(groups[0].label, "Code");
        assert_eq!(groups[0].count, 12_006);
        assert_eq!(groups[1].slug, "documents");
        assert_eq!(groups[1].count, 20);
        assert_eq!(
            groups[1]
                .types
                .iter()
                .map(|f| f.value.as_str())
                .collect::<Vec<_>>(),
            vec!["Guide", "Reference"]
        );
    }

    #[test]
    fn groups_from_facets_never_drops_an_unknown_type() {
        // Arrange: a type from a producer this UI build predates.
        let observed = vec![facet("Code Entity", 12_006), facet("Qualia", 42)];

        // Act
        let groups = groups_from_facets(&observed);

        // Assert: it is visible under "Other", with its count.
        assert_eq!(groups.len(), 2);
        let other = &groups[1];
        assert_eq!(other.slug, OTHER_SLUG);
        assert_eq!(other.label, OTHER_LABEL);
        assert_eq!(other.count, 42);
        assert_eq!(other.types, vec![facet("Qualia", 42)]);
    }

    #[test]
    fn expand_group_keeps_only_observed_members_in_static_order() {
        // Arrange
        let observed: Vec<String> = vec!["Runbook".to_owned(), "Guide".to_owned()];

        // Act
        let expanded = expand_group("documents", &observed).expect("a known group");

        // Assert
        assert_eq!(expanded, vec!["Guide".to_owned(), "Runbook".to_owned()]);
    }

    #[test]
    fn expand_group_expands_other_to_the_unclaimed_observed_types() {
        // Arrange
        let observed: Vec<String> = vec![
            "Code Entity".to_owned(),
            "Qualia".to_owned(),
            "Guide".to_owned(),
            "Widget".to_owned(),
        ];

        // Act
        let expanded = expand_group(OTHER_SLUG, &observed).expect("the Other group");

        // Assert: known members are excluded, observation order kept.
        assert_eq!(expanded, vec!["Qualia".to_owned(), "Widget".to_owned()]);
    }

    #[test]
    fn expand_group_expands_to_nothing_when_no_member_is_observed() {
        // Arrange & Act
        let expanded = expand_group("code", &["Guide".to_owned()]).expect("a known group");

        // Assert: an empty expansion is "matches nothing", not "no filter".
        assert!(expanded.is_empty());
    }

    #[test]
    fn expand_group_rejects_an_unknown_slug() {
        // Arrange & Act & Assert
        assert!(expand_group("bogus", &[]).is_none());
    }

    #[test]
    fn expand_group_covers_every_observed_type_beyond_the_display_cap() {
        // Arrange: more distinct types than the display facets' top-100 cap
        // (membership reads the complete inventory, never the capped facets).
        let observed: Vec<String> = (1..=101).map(|n| format!("Widget{n:03}")).collect();

        // Act
        let expanded = expand_group(OTHER_SLUG, &observed).expect("the Other group");

        // Assert: all 101 types expand, including the one a top-100 display
        // facet list would have dropped.
        assert_eq!(expanded.len(), 101);
        assert!(expanded.iter().any(|t| t == "Widget101"));
    }

    #[test]
    fn label_of_names_known_groups_and_echoes_unknown_slugs() {
        // Arrange & Act & Assert
        assert_eq!(label_of("code"), "Code");
        assert_eq!(label_of("documents"), "Documents");
        assert_eq!(label_of(OTHER_SLUG), OTHER_LABEL);
        assert_eq!(label_of("bogus"), "bogus");
        assert!(is_known_slug("code"));
        assert!(is_known_slug(OTHER_SLUG));
        assert!(!is_known_slug("bogus"));
    }
}
