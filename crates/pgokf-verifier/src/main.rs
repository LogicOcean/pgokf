use std::path::Path;

const HELP: &str = r"pgokf-verifier commands:
  source-guard [ROOT]
  sql-statements FILE
  compare-install-sql EXPECTED ACTUAL
  catalog-validate CONTRACT SNAPSHOT
  catalog-accept CONTRACT CAPTURE
  catalog-compare [--allow-declared-order] EXPECTED ACTUAL
  parity [--source-root ROOT] [--pg-config PG_CONFIG] [--docker-image IMAGE]
         [--preload PROVIDER] [--provider-package PATH] [--historical-install SQL] [--extension-prefix DIR]
         [--reconciliation-only] [--operator-sql SQL]
         [--controls-output OUTPUT] [--contract-output OUTPUT]
         [--embedding-policy not_requested|rebuilt] [--embedding-dim N]
  reconciliation-mutations --results NEW_FILE [--pg-config PATH] [--preload PROVIDER]
                           [--extension-prefix DIR] [--only NAME]
  compatibility-parity ROOT PG_CONFIG
  historical-check FILE
  compatibility-source ROOT
  workflow-check ROOT
  workflow-contexts ROOT
  release-check ROOT
  release-resolve ROOT
  history-check ROOT
  source-manifest ROOT OUTPUT
  homebrew-policy ROOT [FORMULA]
  homebrew-observation LOG VERSION
  homebrew-packaging ROOT NEW_EVIDENCE
  postgres-beta-image
  postgres-check MAJOR IMAGE_TAG
  postgres-target MAJOR IMAGE_TAG
  postgres-candidate MAJOR IMAGE_TAG VERSION
  compose-check ROOT
  companions-smoke IMAGE NEW_EVIDENCE
  image-build SOURCE TOOLING VERSION PG_MAJOR LOCAL_TAG OUTPUT
  image-build-with-builder SOURCE TOOLING VERSION PG_MAJOR LOCAL_TAG OUTPUT
  compatibility-image SOURCE TOOLING IMAGE_TAG OUTPUT
  image-smoke ROOT IMAGE VERSION optional|plain NEW_EVIDENCE
  checkout-shapes ROOT NEW_EVIDENCE
  package-checksums VERSION OUTPUT
  rollback-manifest FROM_IMAGE TO_IMAGE FORMULA OUTPUT
  rollback-check MANIFEST
  owned-create KEY VALUE NEW_RECEIPT IMAGE [OPTIONS -- COMMAND]
  owned-cleanup FULL_ID KEY VALUE NEW_RECEIPT [CREATION_RECEIPT]
  ownership-controls NEW_OUTPUT_DIRECTORY
  policy-campaign ROOT NEW_OUTPUT_DIRECTORY
  mutation-campaign ROOT MANIFEST NEW_OUTPUT_DIRECTORY
  corpus --count N --out DIR --seed SEED [--manifest FILE]
  corpus-field MANIFEST FIELD
  corpus-check DIRECTORY
  notices-check EXPECTED ACTUAL
  rpm-check SPEC
  pgxn-check META_JSON
  digest-check DIGEST
  digests-check DIRECTORY
  deb-smoke PG_BINDIR EXPECTED_VERSION NEW_EVIDENCE
  mcp-golden ROOT PG_CONFIG MCP_BINARY NEW_EVIDENCE_DIR
  mcp-native ROOT PG_CONFIG MCP_BINARY NEW_EVIDENCE_DIR [--mutation NAME]
  mcp-external ROOT PG_CONFIG MCP_BINARY NEW_EVIDENCE_DIR
  positional-check ROOT
  positional-statements ROOT
  migration-check ROOT VERSION

Catalog validation requires an independent fresh-build reference contract.";

#[allow(clippy::too_many_lines)] // One discoverable command router; policies live in library modules.
fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args
        .first()
        .is_some_and(|a| a == "reconciliation-mutations")
    {
        return pgokf_verifier::reconciliation_mutations::cli(&args[1..]);
    }
    if args.first().is_some_and(|a| a == "reconciliation-probe") {
        return pgokf_verifier::upgrade::probe_cli(&args[1..]);
    }
    if args.first().is_some_and(|a| a == "parity") {
        return pgokf_verifier::upgrade::cli(&args[1..]);
    }
    match args.as_slice() {
        [command, expected, actual] if command == "notices-check" => {
            pgokf_verifier::artifact_checks::notices(Path::new(expected), Path::new(actual))
        }
        [command, spec] if command == "rpm-check" => {
            pgokf_verifier::artifact_checks::rpm_file(Path::new(spec))
        }
        [command, meta] if command == "pgxn-check" => {
            pgokf_verifier::artifact_checks::pgxn(&std::fs::read(meta).map_err(|e| e.to_string())?)
        }
        [command, value] if command == "digest-check" => {
            pgokf_verifier::artifact_checks::digest(value)
        }
        [command, directory] if command == "digests-check" => {
            pgokf_verifier::artifact_checks::digests(Path::new(directory))
        }
        [command, bindir, version, evidence] if command == "deb-smoke" => {
            pgokf_verifier::artifact_checks::deb_smoke(
                Path::new(bindir),
                version,
                Path::new(evidence),
            )
        }
        [command, root, pg_config, binary, evidence] if command == "mcp-external" => {
            pgokf_verifier::mcp::native::external_run(
                Path::new(root),
                Path::new(pg_config),
                Path::new(binary),
                Path::new(evidence),
            )
        }
        [command, root, pg_config, binary, evidence, flag, mutation]
            if command == "mcp-native" && flag == "--mutation" =>
        {
            pgokf_verifier::mcp::native::run_mutated(
                Path::new(root),
                Path::new(pg_config),
                Path::new(binary),
                Path::new(evidence),
                Some(pgokf_verifier::mcp::native::Mutation::parse(mutation)?),
            )
        }
        [command, root, pg_config, binary, evidence] if command == "mcp-native" => {
            pgokf_verifier::mcp::native::run(
                Path::new(root),
                Path::new(pg_config),
                Path::new(binary),
                Path::new(evidence),
            )
        }
        [command, root, pg_config, binary, evidence] if command == "mcp-golden" => {
            println!(
                "{} golden fixtures passed",
                pgokf_verifier::mcp::golden_live::run(
                    Path::new(root),
                    Path::new(pg_config),
                    Path::new(binary),
                    Path::new(evidence)
                )?
            );
            Ok(())
        }
        [command, rest @ ..] if command == "corpus" => pgokf_verifier::corpus::cli(rest),
        [command, directory] if command == "corpus-check" => {
            println!(
                "{}",
                pgokf_verifier::corpus::benchmark_check(Path::new(directory))?
            );
            Ok(())
        }
        [command, manifest, field] if command == "corpus-field" => {
            let data = std::fs::read(manifest).map_err(|e| e.to_string())?;
            let manifest: pgokf_verifier::corpus::Manifest =
                pgokf_verifier::json::from_slice(&data)?;
            let value = serde_json::to_value(manifest).map_err(|e| e.to_string())?;
            let value = value.get(field).ok_or("unknown manifest field")?;
            match value {
                serde_json::Value::String(s) => println!("{s}"),
                serde_json::Value::Number(n) => println!("{n}"),
                _ => return Err("non-scalar manifest field".into()),
            }
            Ok(())
        }
        [command, log, version] if command == "homebrew-observation" => {
            pgokf_verifier::homebrew_observation::validate(
                &std::fs::read_to_string(log).map_err(|e| e.to_string())?,
                version,
            )
        }
        [command] if command == "postgres-beta-image" => pgokf_verifier::postgres_pin::beta_image(
            &mut pgokf_verifier::owned_docker::Docker::default(),
        ),
        [command, major, tag] if command == "postgres-check" || command == "postgres-target" => {
            let major = major.parse().map_err(|_| "invalid PostgreSQL major")?;
            if command == "postgres-check" {
                pgokf_verifier::postgres_pin::installed(major, tag)
            } else {
                pgokf_verifier::postgres_pin::target(major, tag)
            }
        }
        [command, major, tag, version] if command == "postgres-candidate" => {
            pgokf_verifier::postgres_pin::package(
                major.parse().map_err(|_| "invalid PostgreSQL major")?,
                tag,
                version,
            )
        }

        [command, image, evidence] if command == "companions-smoke" => {
            pgokf_verifier::image_smoke::companions(
                &mut pgokf_verifier::owned_docker::Docker::default(),
                Path::new(evidence),
                image,
            )
        }
        [command, root] if command == "compose-check" => {
            pgokf_verifier::release_runtime::compose_check(Path::new(root))
        }
        [command, root, destination] if command == "source-manifest" => {
            let manifest = pgokf_verifier::release_runtime::source_manifest(Path::new(root))?;
            pgokf_verifier::release_runtime::write_json(Path::new(destination), &manifest)
        }
        [command, from, to, formula, destination] if command == "rollback-manifest" => {
            let manifest = pgokf_verifier::release_runtime::RollbackManifest {
                schema_version: 1,
                from: pgokf_verifier::json::from_slice(
                    &std::fs::read(from).map_err(|e| e.to_string())?,
                )?,
                to: pgokf_verifier::json::from_slice(
                    &std::fs::read(to).map_err(|e| e.to_string())?,
                )?,
                previous_formula: pgokf_verifier::json::from_slice(
                    &std::fs::read(formula).map_err(|e| e.to_string())?,
                )?,
            };
            pgokf_verifier::release_runtime::validate_rollback_at(
                Path::new("."),
                &mut pgokf_verifier::owned_docker::Docker::default(),
                &manifest,
            )?;
            pgokf_verifier::release_runtime::write_json(Path::new(destination), &manifest)
        }
        [command, file] if command == "historical-check" => {
            println!(
                "{}",
                serde_json::to_string_pretty(&pgokf_verifier::historical::validate(
                    &std::fs::read(file).map_err(|e| e.to_string())?
                )?)
                .map_err(|e| e.to_string())?
            );
            Ok(())
        }
        [command, root, pg_config] if command == "compatibility-parity" => {
            pgokf_verifier::upgrade::compatibility(Path::new(root), Path::new(pg_config))
        }
        [command, contract, capture] if command == "catalog-accept" => {
            pgokf_verifier::catalog::validate_acceptance(
                &pgokf_verifier::json::from_slice(
                    &std::fs::read(contract).map_err(|e| e.to_string())?,
                )?,
                &pgokf_verifier::json::from_slice(
                    &std::fs::read(capture).map_err(|e| e.to_string())?,
                )?,
                true,
            )
        }
        [command, root, output] if command == "policy-campaign" => {
            pgokf_verifier::policy_campaign::run(Path::new(root), Path::new(output))
        }

        [command, root, manifest, evidence] if command == "mutation-campaign" => {
            let bytes = std::fs::read(manifest).map_err(|e| e.to_string())?;
            let campaign = pgokf_verifier::json::from_slice(&bytes)?;
            pgokf_verifier::mutations::run(Path::new(root), &campaign, Path::new(evidence))
        }

        [command, output] if command == "ownership-controls" => {
            pgokf_verifier::owned_docker::live_controls(Path::new(output))
        }
        [command, key, value, receipt, image, rest @ ..] if command == "owned-create" => {
            let proof = pgokf_verifier::owned_docker::create(
                &mut pgokf_verifier::owned_docker::Docker::default(),
                &mut pgokf_verifier::owned_docker::DurableStore,
                key,
                value,
                Path::new(receipt),
                image,
                rest,
            )?;
            println!("{}", proof.container.id);
            Ok(())
        }
        [command, id, key, value, receipt, rest @ ..]
            if command == "owned-cleanup" && rest.len() <= 1 =>
        {
            let proof = rest
                .first()
                .map(|path| {
                    std::fs::read(path)
                        .map_err(|e| e.to_string())
                        .and_then(|bytes| pgokf_verifier::json::from_slice(&bytes))
                })
                .transpose()?;
            pgokf_verifier::owned_docker::cleanup(
                &mut pgokf_verifier::owned_docker::Docker::default(),
                &mut pgokf_verifier::owned_docker::DurableStore,
                id,
                key,
                value,
                Path::new(receipt),
                proof.as_ref(),
            )
        }
        [command, root, evidence] if command == "checkout-shapes" => {
            let report =
                pgokf_verifier::checkout_shapes::check(Path::new(root), Path::new(evidence))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
            );
            Ok(())
        }
        [command, version, destination] if command == "package-checksums" => {
            pgokf_verifier::package_archive::checksums(
                version,
                "https://github.com/timescale/pg_textsearch/releases/download",
                Path::new(destination),
            )
        }
        [command, root, image, version, optional, evidence] if command == "image-smoke" => {
            let optional = match optional.as_str() {
                "optional" => true,
                "plain" => false,
                _ => return Err("expected optional or plain".into()),
            };
            let report = pgokf_verifier::image_smoke::smoke(
                &mut pgokf_verifier::owned_docker::Docker::default(),
                Path::new(root),
                Path::new(evidence),
                image,
                version,
                optional,
            )?;
            println!(
                "{}",
                serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
            );
            Ok(())
        }
        [command, root] if command == "workflow-contexts" => {
            let mut contexts = pgokf_verifier::workflow::Contexts::new();
            for name in ["ci", "docs", "packages", "pgrx-test"] {
                let source = std::fs::read_to_string(
                    Path::new(root).join(format!(".github/workflows/{name}.yml")),
                )
                .map_err(|e| e.to_string())?;
                let workflow = pgokf_verifier::workflow::parse(&source)?;
                let name = workflow["name"].as_str().ok_or("workflow name missing")?;
                contexts.insert(
                    name.to_owned(),
                    pgokf_verifier::workflow::execution_context(&workflow)?,
                );
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&contexts).map_err(|e| e.to_string())?
            );
            Ok(())
        }
        [command, root] if command == "release-check" => {
            let root = Path::new(root);
            let identity = pgokf_verifier::release_identity::source_identity(root)?;
            pgokf_verifier::release_identity::distribution_identity(root, &identity)?;
            let formula = std::fs::read_to_string(root.join("packaging/homebrew/pgokf.rb"))
                .map_err(|e| e.to_string())?;
            pgokf_verifier::release_identity::authenticate_formula(
                &pgokf_verifier::release_identity::formula_tuple(&formula)?,
                &pgokf_verifier::release_identity::published_digests(),
            )?;
            println!(
                "{}",
                serde_json::to_string_pretty(&identity).map_err(|e| e.to_string())?
            );
            Ok(())
        }
        [command, root] if command == "release-resolve" => {
            let intent = pgokf_verifier::release_runtime::resolve(
                Path::new(root),
                &std::env::var("GITHUB_REF").unwrap_or_default(),
                &std::env::var("RELEASE_TAG_INPUT").unwrap_or_default(),
            )?;
            let path = std::env::var("GITHUB_OUTPUT").map_err(|e| e.to_string())?;
            pgokf_verifier::release_runtime::write_intent(Path::new(&path), &intent)
        }
        [command, root] if command == "history-check" => {
            pgokf_verifier::release_runtime::history_check(Path::new(root))
        }
        [command, root] if command == "homebrew-policy" => {
            let root = Path::new(root).canonicalize().map_err(|e| e.to_string())?;
            println!(
                "{}",
                pgokf_verifier::release_runtime::homebrew_policy(
                    &root,
                    &root.join("packaging/homebrew/pgokf.rb")
                )?
            );
            Ok(())
        }
        [command, root, formula] if command == "homebrew-policy" => {
            let root = Path::new(root).canonicalize().map_err(|e| e.to_string())?;
            println!(
                "{}",
                pgokf_verifier::release_runtime::homebrew_policy(&root, Path::new(formula))?
            );
            Ok(())
        }
        [command, root, evidence] if command == "homebrew-packaging" => {
            pgokf_verifier::release_runtime::homebrew_packaging(
                Path::new(root),
                Path::new(evidence),
            )
        }
        [command, source, tooling, version, major, tag, evidence]
            if command == "image-build" || command == "image-build-with-builder" =>
        {
            let build = if command == "image-build-with-builder" {
                pgokf_verifier::release_runtime::image_build_with_builder
            } else {
                pgokf_verifier::release_runtime::image_build
            };
            let manifest = build(
                Path::new(source),
                Path::new(tooling),
                version,
                major.parse().map_err(|_| "invalid PostgreSQL major")?,
                tag,
            )?;
            pgokf_verifier::release_runtime::write_json(Path::new(evidence), &manifest)
        }
        [command, source, tooling, tag, evidence] if command == "compatibility-image" => {
            let manifest = pgokf_verifier::release_runtime::compatibility_image(
                Path::new(source),
                Path::new(tooling),
                tag,
            )?;
            pgokf_verifier::release_runtime::write_json(Path::new(evidence), &manifest)
        }
        [command, manifest] if command == "rollback-check" => {
            let bytes = std::fs::read(manifest).map_err(|e| e.to_string())?;
            pgokf_verifier::release_runtime::validate_rollback_at(
                Path::new("."),
                &mut pgokf_verifier::owned_docker::Docker::default(),
                &pgokf_verifier::json::from_slice(&bytes)?,
            )
        }

        [command] if command == "--help" || command == "help" => {
            println!("{HELP}");
            Ok(())
        }
        [command, root] if command == "positional-check" => {
            pgokf_verifier::positional::scan(Path::new(root))
        }
        [command, root] if command == "positional-statements" => {
            println!(
                "{}",
                serde_json::to_string_pretty(&pgokf_verifier::positional::product_statements(
                    Path::new(root)
                )?)
                .map_err(|e| e.to_string())?
            );
            Ok(())
        }
        [command, root, version] if command == "migration-check" => {
            pgokf_verifier::migration::check_repository(Path::new(root), version)
        }
        [command, root] if command == "workflow-check" => {
            let root = Path::new(root);
            let contexts = std::fs::read(root.join("tests/workflow-contexts.json"))
                .map_err(|e| e.to_string())?;
            let contexts = pgokf_verifier::json::from_slice(&contexts)?;
            let read = |name| {
                let text =
                    std::fs::read_to_string(root.join(format!(".github/workflows/{name}.yml")))
                        .map_err(|e| e.to_string())?;
                pgokf_verifier::workflow::parse(&text)
            };
            let mut errors = pgokf_verifier::workflow::check_shell_policy(&read("ci")?, &contexts);
            errors.extend(pgokf_verifier::workflow::check_support(
                &read("packages")?,
                &read("pgrx-test")?,
                &contexts,
            ));
            errors.extend(pgokf_verifier::workflow::check_release(
                &read("packages")?,
                &read("ci")?,
                &read("pgrx-test")?,
            ));
            errors.extend(pgokf_verifier::workflow::check_pages_policy(
                &read("docs")?,
                &contexts,
            ));
            if errors.is_empty() {
                Ok(())
            } else {
                Err(errors.join("\n"))
            }
        }
        [command, contract, snapshot] if command == "catalog-validate" => {
            let contract = std::fs::read(contract).map_err(|e| e.to_string())?;
            let snapshot = std::fs::read(snapshot).map_err(|e| e.to_string())?;
            pgokf_verifier::catalog::validate_contract(
                &pgokf_verifier::json::from_slice(&snapshot)?,
                &pgokf_verifier::json::from_slice(&contract)?,
            )
        }
        [command, expected, actual] if command == "catalog-compare" => {
            let expected = std::fs::read_to_string(expected).map_err(|e| e.to_string())?;
            let actual = std::fs::read_to_string(actual).map_err(|e| e.to_string())?;
            pgokf_verifier::catalog::compare_inventory(&expected, &actual, false)
        }
        [command, flag, expected, actual]
            if command == "catalog-compare" && flag == "--allow-declared-order" =>
        {
            let expected = std::fs::read_to_string(expected).map_err(|e| e.to_string())?;
            let actual = std::fs::read_to_string(actual).map_err(|e| e.to_string())?;
            pgokf_verifier::catalog::compare_inventory(&expected, &actual, true)
        }
        [command, expected, actual] if command == "compare-install-sql" => {
            let expected = std::fs::read_to_string(expected).map_err(|e| e.to_string())?;
            let actual = std::fs::read_to_string(actual).map_err(|e| e.to_string())?;
            pgokf_verifier::release::compare_install_sql(&expected, &actual)
        }
        [command, root] if command == "compatibility-source" => {
            println!(
                "{}",
                pgokf_verifier::release::compatibility_source(Path::new(root))?
            );
            Ok(())
        }
        [command] if command == "source-guard" => pgokf_verifier::source::check(Path::new(".")),
        [command, root] if command == "source-guard" => {
            pgokf_verifier::source::check(Path::new(root))
        }
        [command, input] if command == "sql-statements" => {
            let text = std::fs::read_to_string(input).map_err(|e| e.to_string())?;
            println!(
                "{} statements",
                pgokf_verifier::sql::statements(&text)?.len()
            );
            Ok(())
        }
        _ => Err("invalid arguments; run pgokf-verifier --help".into()),
    }
}
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}
