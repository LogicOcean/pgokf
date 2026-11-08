use pgokf_verifier::{migration, positional};
use std::path::Path;
fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
}
#[test]
fn shipped_migration_graph_and_safety() {
    migration::check_repository(root(), env!("CARGO_PKG_VERSION")).unwrap();
}
#[test]
fn shipped_positional_source() {
    positional::scan(root()).unwrap();
}
#[test]
fn product_registry_matches_resolved_source() {
    let value: serde_json::Value = pgokf_verifier::json::from_slice(
        &std::fs::read(root().join("tests/upgrade/web-identity-statements.json")).unwrap(),
    )
    .unwrap();
    let mut expected: Vec<_> = value["statements"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["sql"].as_str().unwrap().to_owned())
        .collect();
    expected.sort();
    assert_eq!(positional::product_statements(root()).unwrap(), expected);
}

#[test]
fn hostile_source_constructions_are_not_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("crates/probe/src");
    std::fs::create_dir_all(&source).unwrap();
    for code in [
        "const Q: &str = \"-- harmless\\nSELECT * FROM pgokf_web.users\";",
        "fn q(db: Db) { let a=\"SELECT * FROM \"; let b=\"pgokf_web.users\"; db.query(&(a.to_owned()+b)); }",
        "fn q(db: Db, table: &str) { db.query(&format!(\"SELECT * FROM {}\", table)); }",
        "const Q: &str = \"DO $$ BEGIN EXECUTE 'SELECT * FROM ' || 'pgokf_web.users'; END $$;\";",
        "fn q(db: Db) { db.query(env!(\"SQL\")); }",
        "fn q(db: Db) { db.query(build_query()); }",
    ] {
        std::fs::write(source.join("lib.rs"), code).unwrap();
        assert!(positional::scan(dir.path()).is_err(), "{code}");
    }
}

#[test]
fn tracked_target_named_product_source_is_scanned() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("crates/probe/src/target");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(
        source.join("lib.rs"),
        "const Q: &str = \"SELECT * FROM pgokf_web.users\";",
    )
    .unwrap();
    assert!(positional::scan(dir.path()).is_err());
}

#[test]
fn mutating_strings_and_nested_macros_are_not_proof() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("crates/probe/src");
    std::fs::create_dir_all(&source).unwrap();
    for code in [
        "fn q(db: Db) { let mut q = \"SELECT * FROM \".to_owned(); q.push_str(\"pgokf_web.users\"); db.query(&q); }",
        "fn q(db: Db) { assert!({ db.query(&format!(\"SELECT * FROM {}\", table)).is_ok() }); }",
        "fn q(db: Db) { let results = vec![db.query(unknown())]; }",
        "fn q() { hidden_sql!(); }",
    ] {
        std::fs::write(source.join("lib.rs"), code).unwrap();
        assert!(positional::scan(dir.path()).is_err(), "{code}");
    }
}

#[test]
fn install_binding_fresh_orders_and_drift_fixture() {
    use pgokf_verifier::release;
    let read = |path: &str| std::fs::read_to_string(root().join(path)).unwrap();
    release::check_install_binding(
        &read("crates/extension/sql/pgokf--0.3.1.sql"),
        &read("crates/extension/src/lib.rs"),
    )
    .unwrap();
    positional::check_exceptions(
        &std::fs::read(root().join("tests/upgrade/catalog-exceptions.json")).unwrap(),
    )
    .unwrap();
    let path = root().join("crates/extension/src/catalog");
    let sources = positional::rust_strings(
        &read("crates/extension/src/catalog/web_identity.rs"),
        &path,
        root(),
    )
    .unwrap();
    let fresh = sources
        .iter()
        .find(|s| s.contains("CREATE TABLE pgokf_web.users"))
        .unwrap();
    assert_eq!(
        positional::table_columns(fresh, "pgokf_web.users").unwrap(),
        [
            "name",
            "role",
            "password_hash",
            "display_name",
            "provider",
            "created_at",
            "updated_at"
        ]
    );
    assert_eq!(
        positional::table_columns(fresh, "pgokf_web.sessions").unwrap(),
        [
            "nonce",
            "subject",
            "mode",
            "provider",
            "expires_at",
            "created_at"
        ]
    );
    let fixture = read("tests/upgrade/live-drift.sql");
    positional::check_live_drift_fixture(&fixture, fresh).unwrap();
    for needle in [
        "ALTER TABLE pgokf_web.users DROP COLUMN display_name, DROP COLUMN provider;",
        "ALTER TABLE pgokf_web.sessions DROP COLUMN provider;",
        "COMMENT ON SCHEMA pgokf_web IS",
    ] {
        assert!(
            positional::check_live_drift_fixture(&fixture.replace(needle, "-- removed"), fresh)
                .is_err()
        );
    }
}

#[test]
fn unknown_sql_execution_and_spi_inputs_refuse() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("crates/probe/src");
    std::fs::create_dir_all(&src).unwrap();
    for code in [
        "fn q() { Spi::run(&format!(\"SELECT * FROM {}\", table)); }",
        "fn q(client: Client) { client.select(build(), None, &[]); }",
        "const Q: &str = \"DO $$ BEGIN EXECUTE unknown_statement; END $$;\";",
        "const Q: &str = \"DO $$ BEGIN EXECUTE 'SELECT * FROM ' || 'pgokf_web.users'; END $$;\";",
    ] {
        std::fs::write(src.join("lib.rs"), code).unwrap();
        assert!(positional::scan(dir.path()).is_err(), "{code}");
    }
}

#[test]
fn alternative_and_alias_mutations_cannot_hide_sql() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("crates/probe/src");
    std::fs::create_dir_all(&src).unwrap();
    for code in [
        "fn q(db: Db) { let mut q=\"SELECT * FROM \".to_owned(); { q.push_str(\"pgokf_web.users\"); } db.query(&q); }",
        "fn q(db: Db) { let mut q=\"SELECT * FROM \".to_owned(); q += \"pgokf_web.users\"; db.query(&q); }",
        "fn q(db: Db) { let mut q=\"SELECT * FROM \".to_owned(); let r=&mut q; r.push_str(\"pgokf_web.users\"); db.query(&q); }",
        "fn q(db: Db, condition: bool) { let prefix=if condition {\"SELECT * FROM \"} else {\"SELECT name FROM \"}; db.query(&(prefix.to_owned()+\"pgokf_web.users\")); }",
        "const Q: &str=\"DO $a$ BEGIN DO $b$ BEGIN EXECUTE 'SELECT * FROM pgokf_web.users'; END $b$; END $a$;\";",
    ] {
        std::fs::write(src.join("lib.rs"), code).unwrap();
        assert!(positional::scan(dir.path()).is_err(), "{code}");
    }
}

#[test]
fn conditional_cartesian_composition_is_complete() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("crates/probe/src");
    std::fs::create_dir_all(&src).unwrap();
    for code in [
        r#"fn q(client: Client, flag: bool) { let a=if flag {"SELECT * FROM pgokf_web."}else{"SELECT id FROM pgokf."}; let b=if flag {"users"}else{"bundles"}; client.query(&(a.to_owned()+b), &[]); }"#,
        r#"fn q(client: Client, flag: bool) { let pattern=if flag {"XX"}else{"YY"}; client.query(&"SELECT * FROM pgokf_XX.users".replace(pattern,"web"), &[]); }"#,
        r#"fn q(client: Client, flag: bool) { let a=if flag {"pgokf_web"}else{"pgokf"}; let b=if flag {"users"}else{"bundles"}; client.query(&format!("SELECT * FROM {}.{}",a,b), &[]); }"#,
    ] {
        std::fs::write(src.join("lib.rs"), code).unwrap();
        assert!(positional::scan(dir.path()).is_err(), "{code}");
    }
}

#[test]
fn generated_attributes_and_unknown_text_refuse() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("crates/probe/src");
    std::fs::create_dir_all(&src).unwrap();
    for code in [
        "#[hidden_sql] fn q() {}",
        "#[derive(HiddenSql)] struct Q;",
        "#[evil::main] fn q() {}",
        "#[derive(evil::Debug)] struct Q;",
        "fn q() { evil::format!(\"SELECT 1\"); }",
        "use evil::format; fn q() { format!(\"SELECT 1\"); }",
    ] {
        syn::parse_file(code).unwrap();
        std::fs::write(src.join("lib.rs"), code).unwrap();
        assert!(positional::scan(dir.path()).is_err(), "{code}");
    }
    std::fs::remove_file(src.join("lib.rs")).unwrap();
    std::fs::write(src.join("query.txt"), "SELECT * FROM pgokf_web.users").unwrap();
    assert!(positional::scan(dir.path()).is_err());
}

#[test]
fn do_language_and_string_bodies_are_executable() {
    for source in [
        "DO 'BEGIN PERFORM * FROM pgokf_web.users; END';",
        "DO LANGUAGE plpgsql $$ BEGIN PERFORM * FROM pgokf_web.users; END $$;",
        "DO $$ arbitrary_code $$ LANGUAGE unknown_language;",
    ] {
        assert!(positional::check_sql(source).is_err(), "{source}");
    }
    for source in [
        "DO 'BEGIN DELETE FROM pgokf.skills; END';",
        "DO LANGUAGE plpgsql $$ BEGIN DELETE FROM pgokf.skills; END $$;",
        "CREATE FUNCTION f() RETURNS void LANGUAGE unknown_language AS 'arbitrary_code';",
        r#"CREATE FUNCTION f() RETURNS void LANGUAGE "unknown_language" AS 'arbitrary_code';"#,
    ] {
        assert!(
            migration::check_development_edge(
                &format!("{source} SELECT pgokf_private.register_dump_relations();"),
                &[]
            )
            .is_err(),
            "{source}"
        );
    }
}
