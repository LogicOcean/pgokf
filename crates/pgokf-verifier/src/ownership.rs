//! Pure owned-resource authorization. Docker execution and durable receipt
//! publication must surround this policy; authorization alone deletes nothing.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerIdentity {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Created")]
    pub created: String,
    #[serde(rename = "Mounts")]
    pub mounts: Vec<BTreeMap<String, Value>>,
    #[serde(rename = "Config")]
    pub config: BTreeMap<String, Value>,
}

impl ContainerIdentity {
    fn canonical(&self) -> Self {
        let mut copy = self.clone();
        // BTreeMap gives stable key order; all mount contents stay significant.
        copy.mounts.sort_by_cached_key(|mount| {
            serde_json::to_string(mount).expect("JSON values serialize")
        });
        copy
    }
    fn label(&self, key: &str) -> Option<&str> {
        self.config.get("Labels")?.as_object()?.get(key)?.as_str()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeIdentity {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "CreatedAt")]
    pub created_at: String,
    #[serde(rename = "Labels")]
    pub labels: Option<BTreeMap<String, String>>,
    #[serde(flatten)]
    pub metadata: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreationReceipt {
    pub container: ContainerIdentity,
    pub volumes: Vec<VolumeIdentity>,
    pub ownership_label: BTreeMap<String, String>,
    pub nonce: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleanupAuthorization {
    pub container: ContainerIdentity,
    pub volumes: Vec<VolumeIdentity>,
    pub owned: Vec<VolumeIdentity>,
    pub ownership_label: BTreeMap<String, String>,
}

fn lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Authorize only the immutable requested container and receipt-proven volumes.
/// A caller must durably publish the resulting receipt and then call `recheck`
/// before removing that exact container ID without Docker's volume flag.
///
/// # Errors
/// Any missing identity, duplicated claim, changed metadata or label fails.
pub fn authorize(
    requested_id: &str,
    key: &str,
    value: &str,
    current: &ContainerIdentity,
    volumes: &[VolumeIdentity],
    creation: Option<&CreationReceipt>,
) -> Result<CleanupAuthorization, String> {
    if !lower_hex(requested_id, 64)
        || key.is_empty()
        || value.is_empty()
        || key == "pgokf.creation-nonce"
    {
        return Err("full immutable ID and independent ownership label required".into());
    }
    if current.id != requested_id || current.created.is_empty() || current.label(key) != Some(value)
    {
        return Err("container ownership mismatch".into());
    }
    if current.mounts.iter().any(|mount| {
        !matches!(
            mount.get("Type").and_then(Value::as_str),
            Some("volume" | "bind" | "tmpfs")
        )
    }) {
        return Err("missing or unsupported mount type".into());
    }
    let mounted: BTreeSet<&str> = current
        .mounts
        .iter()
        .filter(|m| m.get("Type").and_then(Value::as_str) == Some("volume"))
        .map(|mount| {
            mount
                .get("Name")
                .and_then(Value::as_str)
                .ok_or_else(|| "volume mount lacks name".to_owned())
        })
        .collect::<Result<_, _>>()?;
    let observed: BTreeSet<&str> = volumes.iter().map(|v| v.name.as_str()).collect();
    if observed.len() != volumes.len() || observed != mounted {
        return Err("mounted-volume inventory incomplete or ambiguous".into());
    }
    let ownership_label = BTreeMap::from([(key.to_owned(), value.to_owned())]);
    let mut owned = Vec::new();
    if let Some(proof) = creation {
        if !lower_hex(&proof.nonce, 32) {
            return Err("invalid volume creation nonce".into());
        }
        let names: BTreeSet<&str> = proof.volumes.iter().map(|v| v.name.as_str()).collect();
        if names.len() != proof.volumes.len() {
            return Err("duplicate volume creation records".into());
        }
        if proof.container.canonical() != current.canonical()
            || proof.ownership_label != ownership_label
        {
            return Err("creation receipt does not bind this container".into());
        }
        for volume in &proof.volumes {
            if !volumes.contains(volume)
                || volume.created_at.is_empty()
                || volume
                    .labels
                    .as_ref()
                    .and_then(|labels| labels.get(key))
                    .map(String::as_str)
                    != Some(value)
                || volume
                    .labels
                    .as_ref()
                    .and_then(|labels| labels.get("pgokf.creation-nonce"))
                    != Some(&proof.nonce)
            {
                return Err("volume creation identity or label changed".into());
            }
            owned.push(volume.clone());
        }
    }
    Ok(CleanupAuthorization {
        container: current.canonical(),
        volumes: volumes.to_vec(),
        owned,
        ownership_label,
    })
}

/// Revalidate after exclusive durable publication and before destructive calls.
/// Repeat volume equality immediately before each non-force volume removal.
/// # Errors
/// Changed container or owned-volume metadata preserves the resource.
pub fn recheck(
    authorization: &CleanupAuthorization,
    current: &ContainerIdentity,
    volumes: &[VolumeIdentity],
) -> Result<(), String> {
    if authorization.container != current.canonical() {
        return Err("container changed during receipt publication".into());
    }
    for volume in &authorization.owned {
        let matches: Vec<_> = volumes.iter().filter(|v| v.name == volume.name).collect();
        if matches.as_slice() != [volume] {
            return Err("volume changed during receipt publication".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn fixture() -> (ContainerIdentity, Vec<VolumeIdentity>, CreationReceipt) {
        let container: ContainerIdentity = serde_json::from_value(json!({"Id":"a".repeat(64), "Created":"now",
            "Config":{"Labels":{"run":"owned"},"Image":"sha256:exact"},
            "Mounts":[{"Type":"volume","Name":"mine","Destination":"/data"},{"Type":"volume","Name":"borrowed","Destination":"/other"}]})).unwrap();
        let volume = VolumeIdentity {
            name: "mine".into(),
            created_at: "then".into(),
            labels: Some(BTreeMap::from([
                ("run".into(), "owned".into()),
                ("pgokf.creation-nonce".into(), "b".repeat(32)),
            ])),
            metadata: BTreeMap::from([("Driver".into(), json!("local"))]),
        };
        let mut borrowed = volume.clone();
        borrowed.name = "borrowed".into();
        let proof = CreationReceipt {
            container: container.clone(),
            volumes: vec![volume.clone()],
            ownership_label: BTreeMap::from([("run".into(), "owned".into())]),
            nonce: "b".repeat(32),
        };
        (container, vec![volume, borrowed], proof)
    }
    fn check(
        c: &ContainerIdentity,
        v: &[VolumeIdentity],
        p: Option<&CreationReceipt>,
    ) -> Result<CleanupAuthorization, String> {
        authorize(&"a".repeat(64), "run", "owned", c, v, p)
    }
    #[test]
    fn capture_before_delete_and_refusals() {
        let (c, v, p) = fixture();
        let receipt = check(&c, &v, Some(&p)).unwrap();
        assert_eq!(receipt.owned.len(), 1);
        assert_eq!(receipt.owned[0].name, "mine");
        assert!(check(&c, &v, None).unwrap().owned.is_empty());
        assert!(authorize("short", "run", "owned", &c, &v, Some(&p)).is_err());
        assert!(authorize(&c.id, "run", "other", &c, &v, Some(&p)).is_err());
        assert!(check(&c, &v[..1], Some(&p)).is_err());
        let mut incomplete = c;
        incomplete.mounts[0].remove("Type");
        assert!(check(&incomplete, &v[1..], None).is_err());
    }
    #[test]
    fn inspect_and_publication_races_preserve_authorization_boundary() {
        let (c, v, p) = fixture();
        let receipt = check(&c, &v, Some(&p)).unwrap();
        assert!(recheck(&receipt, &c, &v).is_ok());
        let mut changed = c.clone();
        changed.created.push('x');
        assert!(recheck(&receipt, &changed, &v).is_err());
        let mut changed = v.clone();
        changed[0]
            .metadata
            .insert("Driver".into(), json!("foreign"));
        assert!(recheck(&receipt, &c, &changed).is_err());
        assert!(recheck(&receipt, &c, &[]).is_err());
    }
    #[test]
    fn invalid_nonce_and_duplicate_claims_refuse() {
        let (c, v, p) = fixture();
        for nonce in [
            String::new(),
            "b".repeat(31),
            "B".repeat(32),
            "z".repeat(32),
        ] {
            let mut bad = p.clone();
            bad.nonce = nonce;
            assert!(check(&c, &v, Some(&bad)).is_err());
        }
        let mut bad = p;
        bad.volumes.push(bad.volumes[0].clone());
        assert!(check(&c, &v, Some(&bad)).is_err());
    }
    #[test]
    fn creation_receipt_binds_container_label_and_complete_volume_metadata() {
        let (c, v, p) = fixture();
        let mut bad = p.clone();
        bad.container.id = "c".repeat(64);
        assert!(check(&c, &v, Some(&bad)).is_err());
        let mut bad = p.clone();
        bad.ownership_label.insert("run".into(), "other".into());
        assert!(check(&c, &v, Some(&bad)).is_err());
        let mut bad = p.clone();
        bad.volumes[0].created_at.clear();
        assert!(check(&c, &v, Some(&bad)).is_err());
        let mut bad = p;
        bad.volumes[0]
            .metadata
            .insert("Options".into(), json!({"device":"foreign"}));
        assert!(check(&c, &v, Some(&bad)).is_err());
    }
    #[test]
    fn mount_order_is_not_identity_change_but_contents_are() {
        let (mut c, v, p) = fixture();
        c.mounts.reverse();
        let receipt = check(&c, &v, Some(&p)).unwrap();
        assert!(recheck(&receipt, &p.container, &v).is_ok());
        c.mounts[0].insert("Destination".into(), json!("/forged"));
        assert!(check(&c, &v, Some(&p)).is_err());
    }
}
