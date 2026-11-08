//! Docker resource creation and deletion bound to durable, exclusive receipts.
use crate::{
    ownership::{self, ContainerIdentity, CreationReceipt, VolumeIdentity},
    process::CommandSpec,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::File,
    io::Write,
    path::{Path, PathBuf},
};

/// An argv-only Docker transport, injectable for deterministic race tests.
pub trait DockerApi {
    /// # Errors
    /// Every nonzero exit, timeout, output overflow, or transport failure refuses.
    fn call(&mut self, args: &[String]) -> Result<String, String>;
}

#[derive(Clone, Debug)]
pub struct Docker {
    pub command: CommandSpec,
}
impl Default for Docker {
    fn default() -> Self {
        Self {
            command: CommandSpec::new("docker"),
        }
    }
}
impl DockerApi for Docker {
    fn call(&mut self, args: &[String]) -> Result<String, String> {
        let mut command = self.command.clone();
        command.args.extend(args.iter().map(OsString::from));
        String::from_utf8(command.checked()?.stdout).map_err(|e| e.to_string())
    }
}

/// Publication is a separate dependency so failed durability can be tested at
/// the actual orchestration boundary, before any destructive Docker command.
pub trait ReceiptStore {
    /// # Errors
    /// Existing destination and any write or synchronization failure refuse.
    fn publish(&mut self, path: &Path, value: &Value) -> Result<(), String>;
}

pub struct DurableStore;
impl ReceiptStore for DurableStore {
    fn publish(&mut self, path: &Path, value: &Value) -> Result<(), String> {
        publish(path, value)
    }
}

/// Atomically publish a complete fsynced receipt without replacing evidence.
/// # Errors
/// Existing destination, invalid parent, serialization, write or fsync failure.
pub fn publish(path: &Path, value: &impl Serialize) -> Result<(), String> {
    publish_with_sync(path, value, |file, _| {
        file.sync_all().map_err(|e| e.to_string())
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SyncStage {
    File,
    Directory,
}

fn publish_with_sync(
    path: &Path,
    value: &impl Serialize,
    mut sync: impl FnMut(&File, SyncStage) -> Result<(), String>,
) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    serde_json::to_writer_pretty(&mut temporary, value).map_err(|e| e.to_string())?;
    temporary.write_all(b"\n").map_err(|e| e.to_string())?;
    sync(temporary.as_file(), SyncStage::File)?;
    temporary
        .persist_noclobber(path)
        .map_err(|e| e.to_string())?;
    sync(
        &File::open(parent).map_err(|e| e.to_string())?,
        SyncStage::Directory,
    )
}

fn one<T: DeserializeOwned>(text: &str) -> Result<T, String> {
    let mut rows: Vec<T> = crate::json::from_slice(text.as_bytes())?;
    if rows.len() != 1 {
        return Err("inspect must return exactly one object".into());
    }
    Ok(rows.remove(0))
}
fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| (*p).to_owned()).collect()
}
fn inspect<T: DeserializeOwned>(
    docker: &mut impl DockerApi,
    kind: &str,
    name: &str,
) -> Result<T, String> {
    let argv = if kind.is_empty() {
        args(&["inspect", name])
    } else {
        args(&[kind, "inspect", name])
    };
    one(&docker.call(&argv)?)
}
fn suffix(path: &Path, ending: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(ending);
    name.into()
}
fn mounted_volumes(
    docker: &mut impl DockerApi,
    container: &ContainerIdentity,
) -> Result<Vec<VolumeIdentity>, String> {
    let mut names = std::collections::BTreeSet::new();
    for mount in &container.mounts {
        if mount.get("Type").and_then(Value::as_str) == Some("volume") {
            names.insert(
                mount
                    .get("Name")
                    .and_then(Value::as_str)
                    .ok_or("volume has no name")?
                    .to_owned(),
            );
        }
    }
    names
        .iter()
        .map(|name| inspect(docker, "volume", name))
        .collect()
}

/// Delete only the immutable labelled container and separately proven volumes.
/// # Errors
/// Any ambiguous identity, receipt, inspect, publication or deletion fails closed.
pub fn cleanup(
    docker: &mut impl DockerApi,
    store: &mut impl ReceiptStore,
    id: &str,
    key: &str,
    value: &str,
    receipt: &Path,
    creation: Option<&CreationReceipt>,
) -> Result<(), String> {
    if id.len() != 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || key.is_empty()
        || value.is_empty()
        || key == "pgokf.creation-nonce"
    {
        return Err("full immutable ID and independent ownership label required".into());
    }
    let container: ContainerIdentity = inspect(docker, "", id)?;
    let volumes = mounted_volumes(docker, &container)?;
    let authorization = ownership::authorize(id, key, value, &container, &volumes, creation)?;
    store.publish(
        receipt,
        &serde_json::to_value(&authorization).map_err(|e| e.to_string())?,
    )?;
    let current: ContainerIdentity = inspect(docker, "", id)?;
    let current_volumes: Vec<VolumeIdentity> = authorization
        .owned
        .iter()
        .map(|v| inspect(docker, "volume", &v.name))
        .collect::<Result<_, _>>()?;
    ownership::recheck(&authorization, &current, &current_volumes)?;
    let result = docker.call(&args(&["rm", "-f", id]))?;
    let mut deleted = Vec::new();
    for volume in &authorization.owned {
        let current: VolumeIdentity = inspect(docker, "volume", &volume.name)?;
        if current != *volume {
            return Err("volume changed after container removal".into());
        }
        deleted.push(docker.call(&args(&["volume", "rm", &volume.name]))?);
    }
    store.publish(
        &suffix(receipt, ".deleted.json"),
        &serde_json::json!({"container_id":id,"result":result,"deleted_volumes":deleted}),
    )
}

/// Create labelled image volumes and pin container creation to the inspected image.
/// Failure retains the intent and every already-published resource receipt.
/// # Errors
/// Any collision, ambiguous creation or missing binding preserves resources.
#[allow(clippy::too_many_lines)] // Keep the durable creation ordering in one audit boundary.
pub fn create(
    docker: &mut impl DockerApi,
    store: &mut impl ReceiptStore,
    key: &str,
    value: &str,
    receipt: &Path,
    image: &str,
    options_and_command: &[String],
) -> Result<CreationReceipt, String> {
    #[derive(serde::Deserialize)]
    struct Image {
        #[serde(rename = "Id")]
        id: String,
        #[serde(rename = "Config")]
        config: ImageConfig,
    }
    #[derive(serde::Deserialize)]
    struct ImageConfig {
        #[serde(rename = "Volumes")]
        volumes: Option<BTreeMap<String, Value>>,
    }
    if key.is_empty() || value.is_empty() || key == "pgokf.creation-nonce" {
        return Err("independent ownership label required".into());
    }
    match std::fs::symlink_metadata(receipt) {
        Ok(_) => return Err("creation receipt already exists".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.to_string()),
    }
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    store.publish(
        &suffix(receipt, ".intent.json"),
        &serde_json::json!({"nonce":nonce,"image":image}),
    )?;
    let image: Image = inspect(docker, "image", image)?;
    if !image.id.strip_prefix("sha256:").is_some_and(|id| {
        id.len() == 64
            && id
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    }) {
        return Err("image lacks immutable digest".into());
    }
    let split = options_and_command.iter().position(|a| a == "--");
    let (options, command) = split.map_or((options_and_command, &[][..]), |at| {
        (&options_and_command[..at], &options_and_command[at + 1..])
    });
    let labels = BTreeMap::from([
        (key.to_owned(), value.to_owned()),
        ("pgokf.creation-nonce".into(), nonce.clone()),
    ]);
    let mut mounts = Vec::new();
    let mut volumes = Vec::new();
    let mut destinations = BTreeMap::new();
    for destination in image.config.volumes.unwrap_or_default().keys() {
        if !destination.starts_with('/') || destination.contains(',') {
            return Err("unsupported image volume destination".into());
        }
        let name = format!("pgokf-{}", uuid::Uuid::new_v4().simple());
        if docker
            .call(&args(&["volume", "ls", "-q"]))?
            .lines()
            .any(|line| line == name)
        {
            return Err("volume name collision".into());
        }
        let created = docker.call(&args(&[
            "volume",
            "create",
            "--label",
            &format!("{key}={value}"),
            "--label",
            &format!("pgokf.creation-nonce={nonce}"),
            &name,
        ]))?;
        let volume: VolumeIdentity = inspect(docker, "volume", created.trim())?;
        if created.trim() != name
            || volume.name != name
            || volume.labels.as_ref() != Some(&labels)
            || volume.created_at.is_empty()
        {
            return Err("ambiguous volume creation".into());
        }
        store.publish(
            &suffix(receipt, &format!(".{name}.json")),
            &serde_json::to_value(&volume).map_err(|e| e.to_string())?,
        )?;
        destinations.insert(name.clone(), destination.clone());
        volumes.push(volume);
        mounts.extend(args(&[
            "--mount",
            &format!("type=volume,source={name},target={destination}"),
        ]));
    }
    let mut argv = args(&["create", "--label", &format!("{key}={value}")]);
    argv.extend(mounts);
    argv.extend_from_slice(options);
    argv.push(image.id.clone());
    argv.extend_from_slice(command);
    let id = docker.call(&argv)?.trim().to_owned();
    let container: ContainerIdentity = inspect(docker, "", &id)?;
    if container.config.get("Image").and_then(Value::as_str) != Some(image.id.as_str()) {
        return Err("container image changed".into());
    }
    for (name, destination) in destinations {
        if container
            .mounts
            .iter()
            .filter(|m| {
                m.get("Type").and_then(Value::as_str) == Some("volume")
                    && m.get("Name").and_then(Value::as_str) == Some(&name)
                    && m.get("Destination").and_then(Value::as_str) == Some(&destination)
            })
            .count()
            != 1
        {
            return Err("created volume not attached at exact destination".into());
        }
    }
    let proof = CreationReceipt {
        container,
        volumes,
        ownership_label: BTreeMap::from([(key.to_owned(), value.to_owned())]),
        nonce,
    };
    let observed = mounted_volumes(docker, &proof.container)?;
    ownership::authorize(&id, key, value, &proof.container, &observed, Some(&proof))?;
    store.publish(
        receipt,
        &serde_json::to_value(&proof).map_err(|e| e.to_string())?,
    )?;
    Ok(proof)
}

/// Run the real borrowed-volume and creation/refusal campaign with durable evidence.
/// # Errors
/// Docker prerequisites, any control failure, or incomplete owned cleanup fails.
pub fn live_controls(out: &Path) -> Result<(), String> {
    std::fs::create_dir(out).map_err(|e| e.to_string())?;
    let mut campaign = Campaign {
        docker: Docker::default(),
        out: out.to_owned(),
        run: uuid::Uuid::new_v4().simple().to_string(),
        containers: Vec::new(),
        volumes: Vec::new(),
    };
    let result = campaign.controls();
    let cleanup = campaign.finish();
    result.and(cleanup)?;
    publish(
        &out.join("result.json"),
        &serde_json::json!({"result":"PASS","donor_data":"donor-sentinel","cases":["borrowed anonymous","mixed owned and borrowed","wrong container label","wrong volume label","receipt collision","creation collision","named preservation","normal owned cleanup"]}),
    )
}

struct Campaign {
    docker: Docker,
    out: PathBuf,
    run: String,
    containers: Vec<String>,
    volumes: Vec<VolumeIdentity>,
}
impl Campaign {
    fn call(&mut self, argv: &[&str]) -> Result<String, String> {
        self.docker.call(&args(argv))
    }
    fn raw_create(&mut self, options: &[&str]) -> Result<String, String> {
        let mut argv = args(&["create", "--label", &format!("test={}", self.run)]);
        argv.extend(args(options));
        let id = self.docker.call(&argv)?.trim().to_owned();
        self.containers.push(id.clone());
        Ok(id)
    }
    fn create(&mut self, stem: &str, options: &[&str]) -> Result<CreationReceipt, String> {
        let proof = create(
            &mut self.docker,
            &mut DurableStore,
            "test",
            &self.run,
            &self.out.join(format!("{stem}-create.json")),
            "postgres:18",
            &args(options),
        )?;
        self.containers.push(proof.container.id.clone());
        self.volumes.extend(proof.volumes.clone());
        Ok(proof)
    }
    fn cleanup(
        &mut self,
        id: &str,
        stem: &str,
        proof: Option<&CreationReceipt>,
    ) -> Result<(), String> {
        cleanup(
            &mut self.docker,
            &mut DurableStore,
            id,
            "test",
            &self.run,
            &self.out.join(format!("{stem}.json")),
            proof,
        )
    }
    fn volume_present(&mut self, name: &str) -> Result<bool, String> {
        Ok(self
            .call(&["volume", "ls", "-q"])?
            .lines()
            .any(|v| v == name))
    }
    fn container_present(&mut self, id: &str) -> Result<bool, String> {
        Ok(self
            .call(&["ps", "-aq", "--no-trunc"])?
            .lines()
            .any(|v| v == id))
    }
    fn require(check: bool, reason: &str) -> Result<(), String> {
        if check {
            Ok(())
        } else {
            Err(reason.to_owned())
        }
    }
    #[allow(clippy::too_many_lines)] // Sequential controls deliberately share the same donor bytes.
    fn controls(&mut self) -> Result<(), String> {
        let donor = self.raw_create(&[
            "-v",
            "/data",
            "alpine:3.21",
            "sh",
            "-c",
            "echo donor-sentinel >/data/proof",
        ])?;
        self.call(&["start", "-a", &donor])?;
        let info: ContainerIdentity = inspect(&mut self.docker, "", &donor)?;
        let volumes = mounted_volumes(&mut self.docker, &info)?;
        Self::require(volumes.len() == 1, "donor volume inventory differs")?;
        let borrowed = volumes[0].name.clone();
        self.volumes.extend(volumes);
        publish(&self.out.join("donor.json"), &info)?;
        let consumer = self.raw_create(&["--volumes-from", &donor, "alpine:3.21", "true"])?;
        self.cleanup(&donor, "donor-delete", None)?;
        self.cleanup(&consumer, "borrowed", None)?;
        Self::require(self.volume_present(&borrowed)?, "borrowed volume deleted")?;
        let mixed = self.create(
            "mixed",
            &[
                "--mount",
                &format!("type=volume,source={borrowed},target=/borrowed"),
                "--",
                "true",
            ],
        )?;
        Self::require(
            !mixed.volumes.is_empty(),
            "mixed control needs owned image volume",
        )?;
        self.cleanup(&mixed.container.id, "mixed-delete", Some(&mixed))?;
        for v in &mixed.volumes {
            Self::require(
                !self.volume_present(&v.name)?,
                "owned mixed volume retained",
            )?;
        }
        Self::require(
            self.volume_present(&borrowed)?,
            "mixed deleted borrowed data",
        )?;
        let reader = self.raw_create(&[
            "--mount",
            &format!("type=volume,source={borrowed},target=/data"),
            "alpine:3.21",
            "cat",
            "/data/proof",
        ])?;
        Self::require(
            self.call(&["start", "-a", &reader])?.trim() == "donor-sentinel",
            "donor bytes differ",
        )?;
        self.cleanup(&reader, "reader", None)?;
        let name = format!("pgokf-test-{}", uuid::Uuid::new_v4().simple());
        let named = self
            .call(&[
                "volume",
                "create",
                "--label",
                &format!("test={}", self.run),
                &name,
            ])?
            .trim()
            .to_owned();
        Self::require(named == name, "volume creation identity differs")?;
        self.volumes
            .push(inspect(&mut self.docker, "volume", &named)?);
        let id = self.raw_create(&["-v", &format!("{named}:/data"), "alpine:3.21", "true"])?;
        Self::require(
            cleanup(
                &mut self.docker,
                &mut DurableStore,
                &id,
                "test",
                "wrong",
                &self.out.join("wrong-container.json"),
                None,
            )
            .is_err(),
            "wrong label accepted",
        )?;
        Self::require(
            self.container_present(&id)?,
            "wrong-label refusal deleted container",
        )?;
        publish(
            &self.out.join("collision.json"),
            &serde_json::json!({"prior":"evidence"}),
        )?;
        Self::require(
            self.cleanup(&id, "collision", None).is_err(),
            "occupied receipt accepted",
        )?;
        Self::require(
            self.container_present(&id)?,
            "receipt collision deleted container",
        )?;
        self.cleanup(&id, "named", None)?;
        Self::require(
            self.volume_present(&named)?,
            "unproven named volume deleted",
        )?;
        let normal = self.create("normal", &["--", "true"])?;
        Self::require(
            !normal.volumes.is_empty(),
            "normal control needs owned image volume",
        )?;
        let mut wrong = normal.clone();
        wrong.volumes[0]
            .labels
            .as_mut()
            .ok_or("missing owned labels")?
            .insert("test".into(), "other".into());
        publish(&self.out.join("wrong-proof.json"), &wrong)?;
        Self::require(
            self.cleanup(&normal.container.id, "wrong-volume", Some(&wrong))
                .is_err(),
            "forged volume proof accepted",
        )?;
        Self::require(
            self.container_present(&normal.container.id)?,
            "forged proof deleted container",
        )?;
        let before = self.call(&["ps", "-aq", "--no-trunc"])?;
        Self::require(
            self.create("normal", &["--", "true"]).is_err(),
            "creation receipt reused",
        )?;
        Self::require(
            before == self.call(&["ps", "-aq", "--no-trunc"])?,
            "creation collision created a container",
        )?;
        self.cleanup(&normal.container.id, "normal-delete", Some(&normal))?;
        for v in normal.volumes {
            Self::require(
                !self.volume_present(&v.name)?,
                "normal owned volume retained",
            )?;
        }
        Ok(())
    }
    fn finish(&mut self) -> Result<(), String> {
        let mut errors = Vec::new();
        for id in self.containers.clone() {
            let result = (|| {
                if self.container_present(&id)? {
                    self.cleanup(&id, &format!("{id}.final"), None)?;
                }
                Ok::<_, String>(())
            })();
            if let Err(e) = result {
                errors.push(e);
            }
        }
        for volume in self.volumes.clone() {
            let result = (|| {
                if self.volume_present(&volume.name)? {
                    let now: VolumeIdentity = inspect(&mut self.docker, "volume", &volume.name)?;
                    Self::require(now == volume, "final owned volume identity changed")?;
                    publish(&self.out.join(format!("{}.final.json", volume.name)), &now)?;
                    let rechecked: VolumeIdentity =
                        inspect(&mut self.docker, "volume", &volume.name)?;
                    Self::require(
                        rechecked == volume,
                        "final owned volume changed after receipt",
                    )?;
                    self.call(&["volume", "rm", &volume.name])?;
                }
                Ok::<_, String>(())
            })();
            if let Err(e) = result {
                errors.push(e);
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("\n"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{cell::RefCell, rc::Rc};

    struct Fake {
        container: Value,
        volume: Value,
        calls: Rc<RefCell<Vec<Vec<String>>>>,
        receipt: PathBuf,
        inspect_count: usize,
        change: bool,
    }
    impl DockerApi for Fake {
        fn call(&mut self, args: &[String]) -> Result<String, String> {
            self.calls.borrow_mut().push(args.to_vec());
            match args[0].as_str() {
                "inspect" => {
                    self.inspect_count += 1;
                    let mut value = self.container.clone();
                    if self.change && self.inspect_count > 1 {
                        value["Created"] = json!("changed");
                    }
                    Ok(json!([value]).to_string())
                }
                "volume" if args[1] == "inspect" => Ok(json!([self.volume]).to_string()),
                "rm" | "volume" => {
                    let published: Value =
                        crate::json::from_slice(&std::fs::read(&self.receipt).unwrap()).unwrap();
                    assert_eq!(published["container"]["Id"], self.container["Id"]);
                    assert!(!args.iter().any(|a| a == "-v" || a == "--volumes"));
                    if args[0] == "volume" {
                        assert_eq!(args[1], "rm");
                        assert_eq!(args.len(), 3);
                    }
                    Ok(args.last().unwrap().clone())
                }
                _ => panic!("unexpected argv {args:?}"),
            }
        }
    }
    fn fixture(receipt: PathBuf) -> (Fake, CreationReceipt) {
        let container = json!({"Id":"a".repeat(64), "Created":"now", "Config":{"Labels":{"run":"mine"}}, "Mounts":[{"Type":"volume","Name":"owned"}]});
        let volume = json!({"Name":"owned", "CreatedAt":"now", "Labels":{"run":"mine","pgokf.creation-nonce":"b".repeat(32)}, "Driver":"local"});
        let proof = crate::json::from_slice(json!({"container":container,"volumes":[volume],"nonce":"b".repeat(32),"ownership_label":{"run":"mine"}}).to_string().as_bytes()).unwrap();
        (
            Fake {
                container,
                volume,
                calls: Rc::new(RefCell::new(Vec::new())),
                receipt,
                inspect_count: 0,
                change: false,
            },
            proof,
        )
    }
    fn deleted(fake: &Fake) -> Vec<Vec<String>> {
        fake.calls
            .borrow()
            .iter()
            .filter(|a| a[0] == "rm" || a.get(1).is_some_and(|s| s == "rm"))
            .cloned()
            .collect()
    }
    #[test]
    fn durable_capture_precedes_delete_and_borrowed_volume_is_retained() {
        for owned in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("receipt.json");
            let (mut fake, proof) = fixture(path.clone());
            cleanup(
                &mut fake,
                &mut DurableStore,
                &"a".repeat(64),
                "run",
                "mine",
                &path,
                owned.then_some(&proof),
            )
            .unwrap();
            assert_eq!(deleted(&fake).len(), if owned { 2 } else { 1 });
            assert!(suffix(&path, ".deleted.json").exists());
        }
    }
    #[test]
    fn occupied_receipt_and_changed_identity_never_delete() {
        for mode in ["occupied", "changed", "wrong-label", "missing-parent"] {
            let dir = tempfile::tempdir().unwrap();
            let mut path = dir.path().join("receipt.json");
            if mode == "occupied" {
                std::fs::write(&path, "prior evidence").unwrap();
            }
            if mode == "missing-parent" {
                path = path.join("missing.json");
            }
            let (mut fake, proof) = fixture(path.clone());
            fake.change = mode == "changed";
            if mode == "wrong-label" {
                fake.container["Config"]["Labels"]["run"] = json!("foreign");
            }
            assert!(
                cleanup(
                    &mut fake,
                    &mut DurableStore,
                    &"a".repeat(64),
                    "run",
                    "mine",
                    &path,
                    Some(&proof)
                )
                .is_err(),
                "{mode}"
            );
            assert!(deleted(&fake).is_empty(), "{mode}");
            if mode == "occupied" {
                assert_eq!(std::fs::read_to_string(path).unwrap(), "prior evidence");
            }
        }
    }
    #[test]
    fn publication_failure_never_reaches_destruction() {
        struct FailedStore;
        impl ReceiptStore for FailedStore {
            fn publish(&mut self, _: &Path, _: &Value) -> Result<(), String> {
                Err("fsync failed".into())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipt.json");
        let (mut fake, proof) = fixture(path.clone());
        assert!(
            cleanup(
                &mut fake,
                &mut FailedStore,
                &"a".repeat(64),
                "run",
                "mine",
                &path,
                Some(&proof)
            )
            .unwrap_err()
            .contains("fsync failed")
        );
        assert!(deleted(&fake).is_empty());
    }
    #[test]
    fn publication_is_exclusive_and_complete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipt.json");
        publish(&path, &json!({"proof":"original"})).unwrap();
        assert!(publish(&path, &json!({"proof":"replacement"})).is_err());
        assert_eq!(
            std::fs::read_to_string(path).unwrap().trim(),
            "{\n  \"proof\": \"original\"\n}"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
    #[test]
    fn inspect_refuses_duplicate_keys_and_multiple_objects() {
        assert!(one::<Value>("[{\"a\":1,\"a\":2}]").is_err());
        assert!(one::<Value>("[{},{}]").is_err());
    }
    #[test]
    fn invalid_container_identity_refuses_before_docker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipt.json");
        let (mut fake, _) = fixture(path.clone());
        assert!(
            cleanup(
                &mut fake,
                &mut DurableStore,
                "--format",
                "run",
                "mine",
                &path,
                None
            )
            .is_err()
        );
        assert!(fake.calls.borrow().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn dangling_creation_receipt_is_a_collision_before_any_resource() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipt.json");
        std::os::unix::fs::symlink(dir.path().join("absent"), &path).unwrap();
        let (mut fake, _) = fixture(path.clone());
        assert!(
            create(
                &mut fake,
                &mut DurableStore,
                "run",
                "mine",
                &path,
                "postgres:18",
                &[]
            )
            .is_err()
        );
        assert!(fake.calls.borrow().is_empty());
        assert!(!suffix(&path, ".intent.json").exists());
    }
    #[test]
    fn actual_file_and_directory_sync_failures_preserve_resources() {
        struct SyncFailure(SyncStage);
        impl ReceiptStore for SyncFailure {
            fn publish(&mut self, path: &Path, value: &Value) -> Result<(), String> {
                publish_with_sync(path, value, |file, stage| {
                    if stage == self.0 {
                        Err("injected synchronization failure".into())
                    } else {
                        file.sync_all().map_err(|e| e.to_string())
                    }
                })
            }
        }
        for stage in [SyncStage::File, SyncStage::Directory] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("receipt.json");
            let (mut fake, proof) = fixture(path.clone());
            assert!(
                cleanup(
                    &mut fake,
                    &mut SyncFailure(stage),
                    &"a".repeat(64),
                    "run",
                    "mine",
                    &path,
                    Some(&proof)
                )
                .unwrap_err()
                .contains("synchronization failure")
            );
            assert!(deleted(&fake).is_empty());
            assert_eq!(path.exists(), stage == SyncStage::Directory);
        }
    }

    #[test]
    fn changed_volume_before_and_after_container_removal_is_never_deleted() {
        struct Racing {
            fake: Fake,
            reads: usize,
            at: usize,
        }
        impl DockerApi for Racing {
            fn call(&mut self, args: &[String]) -> Result<String, String> {
                if args.first().is_some_and(|a| a == "volume")
                    && args.get(1).is_some_and(|a| a == "inspect")
                {
                    self.reads += 1;
                    if self.reads == self.at {
                        self.fake.volume["Driver"] = json!("foreign");
                    }
                }
                self.fake.call(args)
            }
        }
        for at in [2, 3] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("receipt.json");
            let (fake, proof) = fixture(path.clone());
            let mut race = Racing { fake, reads: 0, at };
            assert!(
                cleanup(
                    &mut race,
                    &mut DurableStore,
                    &"a".repeat(64),
                    "run",
                    "mine",
                    &path,
                    Some(&proof)
                )
                .is_err()
            );
            let deleted = deleted(&race.fake);
            assert_eq!(deleted.len(), usize::from(at == 3));
            assert!(deleted.iter().all(|a| a[0] == "rm"));
        }
    }

    #[test]
    fn creation_intent_is_durable_before_any_docker_and_collisions_are_atomic() {
        struct Unreachable;
        impl DockerApi for Unreachable {
            fn call(&mut self, _: &[String]) -> Result<String, String> {
                panic!("Docker reached before durable intent")
            }
        }
        struct Failing;
        impl ReceiptStore for Failing {
            fn publish(&mut self, _: &Path, _: &Value) -> Result<(), String> {
                Err("intent sync failed".into())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("create.json");
        assert!(
            create(
                &mut Unreachable,
                &mut Failing,
                "run",
                "mine",
                &path,
                "postgres:18",
                &[]
            )
            .unwrap_err()
            .contains("intent sync failed")
        );
        publish(&suffix(&path, ".intent.json"), &json!({"prior":"intent"})).unwrap();
        assert!(
            create(
                &mut Unreachable,
                &mut DurableStore,
                "run",
                "mine",
                &path,
                "postgres:18",
                &[]
            )
            .is_err()
        );
    }
    #[test]
    fn creation_pins_image_and_rejects_each_ambiguous_resource_binding() {
        struct Creator {
            mode: &'static str,
            volume: Option<Value>,
            destructive: bool,
            container: Option<Value>,
        }
        impl DockerApi for Creator {
            fn call(&mut self, argv: &[String]) -> Result<String, String> {
                match argv[0].as_str() {
                    "image" => Ok(json!([{"Id":format!("sha256:{}","c".repeat(64)),"Config":{"Volumes":{"/data":{}}}}]).to_string()),
                    "volume" if argv[1] == "ls" => Ok(String::new()),
                    "volume" if argv[1] == "create" => {
                        assert_eq!(&argv[2], "--label"); assert_eq!(&argv[3], "run=mine");
                        assert_eq!(&argv[4], "--label");
                        let nonce = argv[5].strip_prefix("pgokf.creation-nonce=").unwrap();
                        let name = argv[6].clone();
                        self.volume = Some(json!({"Name":name,"CreatedAt":"now","Labels":{"run":"mine","pgokf.creation-nonce":nonce},"Driver":"local"}));
                        if self.mode == "volume-label" { self.volume.as_mut().unwrap()["Labels"]["run"] = json!("foreign"); }
                        if self.mode == "volume-created" { self.volume.as_mut().unwrap()["CreatedAt"] = json!(""); }
                        if self.mode == "volume-name" { return Ok("other".into()); }
                        Ok(name)
                    }
                    "volume" if argv[1] == "inspect" => Ok(json!([self.volume.as_ref().unwrap()]).to_string()),
                    "create" => {
                        let digest = format!("sha256:{}","c".repeat(64));
                        assert!(argv.contains(&digest)); assert!(!argv.contains(&"postgres:18".to_owned()));
                        assert_eq!(argv.last().unwrap(), "true");
                        let mut container = json!({"Id":"a".repeat(64),"Created":"now","Config":{"Labels":{"run":"mine"},"Image":digest},"Mounts":[{"Type":"volume","Name":self.volume.as_ref().unwrap()["Name"],"Destination":"/data"}]});
                        if self.mode == "image" { container["Config"]["Image"] = json!("foreign"); }
                        if self.mode == "destination" { container["Mounts"][0]["Destination"] = json!("/other"); }
                        if self.mode == "container-label" { container["Config"]["Labels"]["run"] = json!("foreign"); }
                        if self.mode == "container-id" { container["Id"] = json!("short"); }
                        self.container = Some(container);
                        Ok("a".repeat(64))
                    }
                    "inspect" => Ok(json!([self.container.as_ref().unwrap()]).to_string()),
                    _ => { self.destructive = true; Err("unexpected destructive call".into()) }
                }
            }
        }
        for mode in [
            "valid",
            "volume-label",
            "volume-created",
            "volume-name",
            "image",
            "destination",
            "container-label",
            "container-id",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("receipt.json");
            let mut docker = Creator {
                mode,
                volume: None,
                destructive: false,
                container: None,
            };
            let result = create(
                &mut docker,
                &mut DurableStore,
                "run",
                "mine",
                &path,
                "postgres:18",
                &args(&["--", "true"]),
            );
            assert_eq!(result.is_ok(), mode == "valid", "{mode}: {result:?}");
            assert!(!docker.destructive);
            assert!(suffix(&path, ".intent.json").exists());
            assert_eq!(path.exists(), mode == "valid");
            if let Ok(proof) = result {
                assert_eq!(proof.volumes.len(), 1);
                assert_eq!(proof.container.id, "a".repeat(64));
            }
        }
    }
}
