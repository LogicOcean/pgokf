//! Correlated forgeries must fail even when both inspection and receipt agree.
use pgokf_verifier::ownership::{self, ContainerIdentity, CreationReceipt, VolumeIdentity};
use serde_json::json;
use std::collections::BTreeMap;
fn fixture() -> (ContainerIdentity, VolumeIdentity, CreationReceipt) {
    let container: ContainerIdentity = serde_json::from_value(json!({"Id":"a".repeat(64),"Created":"now","Config":{"Labels":{"run":"mine"}},"Mounts":[{"Type":"volume","Name":"mine","Destination":"/data"}]})).unwrap();
    let volume = VolumeIdentity {
        name: "mine".into(),
        created_at: "now".into(),
        labels: Some(BTreeMap::from([
            ("run".into(), "mine".into()),
            ("pgokf.creation-nonce".into(), "b".repeat(32)),
        ])),
        metadata: BTreeMap::new(),
    };
    let proof = CreationReceipt {
        container: container.clone(),
        volumes: vec![volume.clone()],
        ownership_label: BTreeMap::from([("run".into(), "mine".into())]),
        nonce: "b".repeat(32),
    };
    (container, volume, proof)
}
#[test]
fn correlated_invalid_id_never_becomes_an_ownership_identity() {
    let (mut container, volume, mut proof) = fixture();
    for id in [
        "short".into(),
        "A".repeat(64),
        "z".repeat(64),
        String::new(),
    ] {
        container.id = id.clone();
        proof.container = container.clone();
        assert!(
            ownership::authorize(
                &id,
                "run",
                "mine",
                &container,
                std::slice::from_ref(&volume),
                Some(&proof)
            )
            .is_err(),
            "invalid immutable ID accepted"
        );
    }
}
#[test]
fn correlated_invalid_nonce_never_authorizes_volume_deletion() {
    let (container, mut volume, mut proof) = fixture();
    for nonce in [
        "short".into(),
        "B".repeat(32),
        "z".repeat(32),
        String::new(),
    ] {
        volume
            .labels
            .as_mut()
            .unwrap()
            .insert("pgokf.creation-nonce".into(), nonce.clone());
        proof.volumes = vec![volume.clone()];
        proof.nonce = nonce;
        assert!(
            ownership::authorize(
                &container.id,
                "run",
                "mine",
                &container,
                std::slice::from_ref(&volume),
                Some(&proof)
            )
            .is_err(),
            "invalid creation nonce accepted"
        );
    }
}
#[test]
fn correlated_empty_volume_creation_time_never_authorizes_deletion() {
    let (container, mut volume, mut proof) = fixture();
    volume.created_at.clear();
    proof.volumes = vec![volume.clone()];
    assert!(
        ownership::authorize(
            &container.id,
            "run",
            "mine",
            &container,
            &[volume],
            Some(&proof)
        )
        .is_err()
    );
}
#[test]
fn borrowed_container_label_is_still_required() {
    let (mut container, volume, _proof) = fixture();
    container
        .config
        .insert("Labels".into(), json!({"run":"foreign"}));
    assert!(
        ownership::authorize(&container.id, "run", "mine", &container, &[volume], None).is_err()
    );
}
#[test]
fn correlated_volume_labels_and_container_creation_time_are_required() {
    for field in ["run", "pgokf.creation-nonce"] {
        let (container, mut volume, mut proof) = fixture();
        volume
            .labels
            .as_mut()
            .unwrap()
            .insert(field.into(), "foreign".into());
        proof.volumes = vec![volume.clone()];
        assert!(
            ownership::authorize(
                &container.id,
                "run",
                "mine",
                &container,
                &[volume],
                Some(&proof)
            )
            .is_err(),
            "{field}"
        );
    }
    let (mut container, volume, mut proof) = fixture();
    container.created.clear();
    proof.container = container.clone();
    assert!(
        ownership::authorize(
            &container.id,
            "run",
            "mine",
            &container,
            &[volume],
            Some(&proof)
        )
        .is_err()
    );
}
