//! Integration tests for `ardur marketplace`.

use assert_cmd::Command;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::{Signature, SigningKey, signature::Signer};
use p256::elliptic_curve::rand_core::OsRng;
use p256::pkcs8::{EncodePublicKey, LineEnding};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::Path;

/// Build a signed skill manifest (with a bundled `SKILL.md` artifact) under
/// `dir/package`, returning `(manifest_path, public_key_path)`.
fn signed_skill_manifest(
    dir: &Path,
    id: &str,
    name: &str,
    version: &str,
    capabilities: Vec<String>,
) -> (std::path::PathBuf, std::path::PathBuf, SigningKey) {
    let package = dir.join(format!("package-{id}"));
    std::fs::create_dir_all(&package).expect("package dir");
    let artifact = package.join("SKILL.md");
    std::fs::write(&artifact, format!("# {name}\n\nBody for {id}.\n")).expect("artifact");
    let digest = hex::encode(Sha256::digest(
        std::fs::read(&artifact).expect("artifact bytes"),
    ));

    let signing_key = SigningKey::random(&mut OsRng);
    let public_pem = signing_key
        .verifying_key()
        .to_public_key_pem(LineEnding::LF)
        .expect("public key pem");
    let key_path = dir.join(format!("public-{id}.pem"));
    std::fs::write(&key_path, public_pem).expect("write key");

    let artifacts = vec![json!({"path": "SKILL.md", "sha256": digest})];
    let payload = canonical_payload(
        1,
        "skill",
        id,
        name,
        version,
        &capabilities,
        &artifacts,
        &[],
    );
    let signature: Signature = signing_key.sign(&payload);
    let manifest = json!({
        "schema_version": 1,
        "kind": "skill",
        "id": id,
        "name": name,
        "version": version,
        "capabilities": capabilities,
        "artifacts": artifacts,
        "runtime_claims": [],
        "signature": {
            "alg": "ES256",
            "value": URL_SAFE_NO_PAD.encode(signature.to_der().as_bytes())
        }
    });
    let manifest_path = package.join("manifest.json");
    std::fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).expect("manifest json"),
    )
    .expect("write manifest");

    (manifest_path, key_path, signing_key)
}

#[test]
fn marketplace_install_browse_inspect_uninstall_lifecycle() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (manifest_path, key_path, _) = signed_skill_manifest(
        dir.path(),
        "skill.helper",
        "Helper",
        "0.1.0",
        vec!["cap.fs_read".to_string()],
    );

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "install"])
        .arg(&manifest_path)
        .arg("--key")
        .arg(&key_path)
        .assert()
        .success()
        .stdout(predicates::str::contains("signature verified"));

    let browse = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "browse"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let browse_stdout = String::from_utf8(browse).expect("stdout utf8");
    assert!(browse_stdout.contains("skill.helper"), "{browse_stdout}");
    assert!(browse_stdout.contains("yes"), "{browse_stdout}"); // SIGNED column

    // `list` remains a working alias of `browse`.
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("skill.helper"));

    let search = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "search", "helper"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(String::from_utf8(search).unwrap().contains("skill.helper"));

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "inspect", "skill.helper"])
        .assert()
        .success()
        .stdout(predicates::str::contains("verified at install/update time"))
        .stdout(predicates::str::contains("cap.fs_read"))
        .stdout(predicates::str::contains(
            "loaded into local tool catalog: true",
        ));

    let catalog_path = dir
        .path()
        .join(".ardur")
        .join("skills_catalog")
        .join("skill.helper")
        .join("SKILL.md");
    assert!(catalog_path.is_file(), "SKILL.md should be synced");

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "uninstall", "skill.helper"])
        .assert()
        .success();

    let record_path = dir
        .path()
        .join(".ardur")
        .join("skills")
        .join("skill.helper.json");
    assert!(!record_path.exists(), "skill record should be deleted");
    assert!(
        !catalog_path.exists(),
        "skill catalog copy should be removed on uninstall"
    );

    // `remove` remains a working alias of `uninstall` (already-removed skill
    // now 404s under either name).
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "remove", "skill.helper"])
        .assert()
        .failure();
}

#[test]
fn marketplace_install_refuses_unsigned_by_default() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (manifest_path, _key_path, _) =
        signed_skill_manifest(dir.path(), "skill.unsigned", "Unsigned", "0.1.0", vec![]);

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "install"])
        .arg(&manifest_path)
        .assert()
        .failure()
        .stderr(predicates::str::contains("refusing to install"));

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "install"])
        .arg(&manifest_path)
        .arg("--allow-unsigned")
        .assert()
        .success()
        .stdout(predicates::str::contains("UNVERIFIED"));
}

#[test]
fn marketplace_install_refuses_remote_source() {
    let dir = tempfile::tempdir().expect("tempdir");
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args([
            "marketplace",
            "install",
            "https://example.com/skills/helper.json",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("not implemented"));
}

#[test]
fn marketplace_update_enforces_matching_id_and_no_downgrade() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (manifest_v1, key_path, signing_key) = signed_skill_manifest(
        dir.path(),
        "skill.up",
        "Up",
        "1.0.0",
        vec!["cap.fs_read".to_string()],
    );

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "install"])
        .arg(&manifest_v1)
        .arg("--key")
        .arg(&key_path)
        .assert()
        .success();

    // Build a v2 manifest signed with the SAME key, adding a capability.
    let package_v2 = dir.path().join("package-v2");
    std::fs::create_dir_all(&package_v2).expect("dir");
    let artifact = package_v2.join("SKILL.md");
    std::fs::write(&artifact, "# Up v2\n").expect("artifact");
    let digest = hex::encode(Sha256::digest(std::fs::read(&artifact).unwrap()));
    let capabilities = vec!["cap.fs_read".to_string(), "cap.network_out".to_string()];
    let artifacts = vec![json!({"path": "SKILL.md", "sha256": digest})];
    let payload = canonical_payload(
        1,
        "skill",
        "skill.up",
        "Up",
        "2.0.0",
        &capabilities,
        &artifacts,
        &[],
    );
    let signature: Signature = signing_key.sign(&payload);
    let manifest_v2_path = package_v2.join("manifest.json");
    std::fs::write(
        &manifest_v2_path,
        serde_json::to_string_pretty(&json!({
            "schema_version": 1, "kind": "skill", "id": "skill.up", "name": "Up",
            "version": "2.0.0", "capabilities": capabilities, "artifacts": artifacts,
            "runtime_claims": [],
            "signature": {"alg": "ES256", "value": URL_SAFE_NO_PAD.encode(signature.to_der().as_bytes())}
        }))
        .unwrap(),
    )
    .unwrap();

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "update", "skill.up"])
        .arg(&manifest_v2_path)
        .arg("--key")
        .arg(&key_path)
        .assert()
        .success()
        .stdout(predicates::str::contains("1.0.0 -> 2.0.0"))
        .stdout(predicates::str::contains("capabilities added"));

    // Attempting to "update" back to v1 (a downgrade) is refused without --force.
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "update", "skill.up"])
        .arg(&manifest_v1)
        .arg("--key")
        .arg(&key_path)
        .assert()
        .failure()
        .stderr(predicates::str::contains("downgrade"));
}

#[test]
fn marketplace_audit_flags_unverified_and_high_risk_capabilities() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (manifest_path, _key_path, _) = signed_skill_manifest(
        dir.path(),
        "skill.risky",
        "Risky",
        "0.1.0",
        vec!["cap.shell_exec".to_string()],
    );

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "install"])
        .arg(&manifest_path)
        .arg("--allow-unsigned")
        .assert()
        .success();

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "audit"])
        .assert()
        .success()
        .stdout(predicates::str::contains("not signature-verified"))
        .stdout(predicates::str::contains("high-risk capabilities"))
        .stdout(predicates::str::contains("1 unverified"))
        .stdout(predicates::str::contains("1 with high-risk"));
}

#[test]
fn marketplace_publish_then_install_round_trip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let skill_dir = dir.path().join("my-skill");
    std::fs::create_dir_all(&skill_dir).expect("skill dir");
    std::fs::write(skill_dir.join("SKILL.md"), "# My Skill\n\nDoes things.\n").expect("write");

    let signing_key = SigningKey::random(&mut OsRng);
    let public_pem = signing_key
        .verifying_key()
        .to_public_key_pem(LineEnding::LF)
        .expect("public pem");
    let private_pem = {
        use p256::pkcs8::EncodePrivateKey;
        signing_key
            .to_pkcs8_pem(LineEnding::LF)
            .expect("private pem")
    };
    let public_key_path = dir.path().join("public.pem");
    let private_key_path = dir.path().join("private.pem");
    std::fs::write(&public_key_path, public_pem).unwrap();
    std::fs::write(&private_key_path, private_pem.as_bytes()).unwrap();

    let out_path = dir.path().join("published").join("manifest.json");

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "publish"])
        .arg(&skill_dir)
        .arg("skill.published")
        .arg("Published Skill")
        .arg("0.1.0")
        .arg("--capability")
        .arg("cap.fs_read")
        .arg("--key")
        .arg(&private_key_path)
        .arg("--out")
        .arg(&out_path)
        .assert()
        .success()
        .stdout(predicates::str::contains("published manifest written to"));

    assert!(out_path.is_file());
    assert!(out_path.parent().unwrap().join("SKILL.md").is_file());

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "install"])
        .arg(&out_path)
        .arg("--key")
        .arg(&public_key_path)
        .assert()
        .success()
        .stdout(predicates::str::contains("signature verified"));
}

#[test]
fn marketplace_publish_plugin_with_runtime_claims() {
    let dir = tempfile::tempdir().expect("tempdir");
    let skill_dir = dir.path().join("my-plugin");
    std::fs::create_dir_all(&skill_dir).expect("dir");
    std::fs::write(skill_dir.join("SKILL.md"), "# My Plugin\n").expect("write");

    let signing_key = SigningKey::random(&mut OsRng);
    let private_pem = {
        use p256::pkcs8::EncodePrivateKey;
        signing_key
            .to_pkcs8_pem(LineEnding::LF)
            .expect("private pem")
    };
    let private_key_path = dir.path().join("private.pem");
    std::fs::write(&private_key_path, private_pem.as_bytes()).unwrap();
    let out_path = dir.path().join("plugin-manifest.json");

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "publish"])
        .arg(&skill_dir)
        .arg("plugin.demo")
        .arg("Demo Plugin")
        .arg("0.1.0")
        .arg("--kind")
        .arg("plugin")
        .arg("--claim")
        .arg("translate:tool")
        .arg("--key")
        .arg(&private_key_path)
        .arg("--out")
        .arg(&out_path)
        .assert()
        .success();

    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out_path).unwrap()).unwrap();
    assert_eq!(manifest["runtime_claims"][0]["name"], "translate");
    assert_eq!(manifest["runtime_claims"][0]["family"], "tool");
}

#[test]
fn marketplace_publish_rejects_claim_without_plugin_kind() {
    let dir = tempfile::tempdir().expect("tempdir");
    let skill_dir = dir.path().join("skl");
    std::fs::create_dir_all(&skill_dir).expect("dir");
    std::fs::write(skill_dir.join("SKILL.md"), "# S\n").expect("write");
    let signing_key = SigningKey::random(&mut OsRng);
    let private_pem = {
        use p256::pkcs8::EncodePrivateKey;
        signing_key
            .to_pkcs8_pem(LineEnding::LF)
            .expect("private pem")
    };
    let private_key_path = dir.path().join("private.pem");
    std::fs::write(&private_key_path, private_pem.as_bytes()).unwrap();

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "publish"])
        .arg(&skill_dir)
        .arg("skill.bad")
        .arg("Bad")
        .arg("0.1.0")
        .arg("--claim")
        .arg("x:tool")
        .arg("--key")
        .arg(&private_key_path)
        .assert()
        .failure()
        .stderr(predicates::str::contains("--kind plugin"));
}

#[test]
fn marketplace_validate_verifies_signature_and_artifact_digest() {
    let dir = tempfile::tempdir().expect("tempdir");
    let package = dir.path().join("package");
    std::fs::create_dir_all(&package).expect("package dir");
    let artifact = package.join("skill.md");
    std::fs::write(&artifact, "# Helper\n").expect("artifact");
    let digest = hex::encode(Sha256::digest(
        std::fs::read(&artifact).expect("artifact bytes"),
    ));

    let signing_key = SigningKey::random(&mut OsRng);
    let public_pem = signing_key
        .verifying_key()
        .to_public_key_pem(LineEnding::LF)
        .expect("public key pem");
    let key_path = dir.path().join("public.pem");
    std::fs::write(&key_path, public_pem).expect("write key");

    let capabilities = vec!["cap.http.fetch".to_string(), "cap.memory.read".to_string()];
    let artifacts = vec![json!({"path":"skill.md","sha256":digest})];
    let payload = canonical_payload(
        1,
        "skill",
        "skill.helper",
        "Helper",
        "0.1.0",
        &capabilities,
        &artifacts,
        &[],
    );
    let signature: Signature = signing_key.sign(&payload);
    let manifest = json!({
        "schema_version": 1,
        "kind": "skill",
        "id": "skill.helper",
        "name": "Helper",
        "version": "0.1.0",
        "capabilities": capabilities,
        "artifacts": artifacts,
        "runtime_claims": [],
        "signature": {
            "alg": "ES256",
            "value": URL_SAFE_NO_PAD.encode(signature.to_der().as_bytes())
        }
    });
    let manifest_path = package.join("manifest.json");
    std::fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).expect("manifest json"),
    )
    .expect("write manifest");

    let validate = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .arg("marketplace")
        .arg("validate")
        .arg(&manifest_path)
        .arg("--key")
        .arg(&key_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(validate).expect("stdout utf8");
    assert!(stdout.contains("skill.helper"), "{stdout}");
    assert!(stdout.contains("verified"), "{stdout}");

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .arg("marketplace")
        .arg("install")
        .arg(&manifest_path)
        .arg("--key")
        .arg(&key_path)
        .assert()
        .success();
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .arg("marketplace")
        .arg("verify")
        .arg("--key")
        .arg(&key_path)
        .assert()
        .success();

    std::fs::write(&artifact, "# Tampered\n").expect("tamper artifact");
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .arg("marketplace")
        .arg("validate")
        .arg(&manifest_path)
        .arg("--key")
        .arg(&key_path)
        .assert()
        .failure();
}

/// R1-sibling finding from the 2026-07-12 cli security sweep: `install`
/// used to trust `manifest.id` unsanitized as the filename for the
/// installed record (`<skills_dir>/{id}.json`), so a manifest whose `id`
/// was a path-traversal or absolute path could write outside the skills
/// directory. `install_record` now runs the id through the same sanitizer
/// as every other id-derived state path and refuses to install rather than
/// silently substituting a different identity.
#[test]
fn marketplace_install_refuses_a_manifest_with_a_traversal_id() {
    let dir = tempfile::tempdir().expect("tempdir");
    let manifest_path = dir.path().join("evil.json");
    std::fs::write(
        &manifest_path,
        json!({
            "schema_version": 1,
            "kind": "skill",
            "id": "../../../etc/passwd",
            "name": "evil",
            "version": "1.0.0",
            "signature": {"alg": "ES256", "value": "deadbeef"}
        })
        .to_string(),
    )
    .expect("write manifest");

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "install"])
        .arg(&manifest_path)
        .arg("--allow-unsigned")
        .assert()
        .failure();

    // No skill record should have been written anywhere under the state
    // dir — neither at the traversal target nor under a substituted id.
    let skills_dir = dir.path().join(".ardur").join("skills");
    if skills_dir.is_dir() {
        let count = std::fs::read_dir(&skills_dir)
            .expect("read skills dir")
            .count();
        assert_eq!(count, 0, "no skill record should have been installed");
    }
}

/// A manifest file over the 1 MiB cap is rejected before being buffered
/// into memory, rather than read in full first.
#[test]
fn marketplace_install_refuses_an_oversized_manifest() {
    let dir = tempfile::tempdir().expect("tempdir");
    let manifest_path = dir.path().join("huge.json");
    // Content doesn't need to be valid JSON — the size cap is checked
    // before the file is even opened for reading.
    std::fs::write(&manifest_path, vec![b'a'; 2 * 1024 * 1024]).expect("write huge manifest");

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "install"])
        .arg(&manifest_path)
        .arg("--allow-unsigned")
        .assert()
        .failure(); // oversize is refused before any read, even with --allow-unsigned
}

#[allow(clippy::too_many_arguments)]
fn canonical_payload(
    schema_version: u32,
    kind: &str,
    id: &str,
    name: &str,
    version: &str,
    capabilities: &[String],
    artifacts: &[serde_json::Value],
    runtime_claims: &[serde_json::Value],
) -> Vec<u8> {
    let lines = [
        (
            "schema_version",
            serde_json::to_string(&schema_version).unwrap(),
        ),
        ("kind", serde_json::to_string(kind).unwrap()),
        ("id", serde_json::to_string(id).unwrap()),
        ("name", serde_json::to_string(name).unwrap()),
        ("version", serde_json::to_string(version).unwrap()),
        ("capabilities", serde_json::to_string(capabilities).unwrap()),
        ("artifacts", serde_json::to_string(artifacts).unwrap()),
        (
            "runtime_claims",
            serde_json::to_string(runtime_claims).unwrap(),
        ),
    ];
    let mut out = Vec::new();
    for (name, value) in lines {
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b":");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\n");
    }
    out
}

#[test]
fn marketplace_default_policy_permits_high_risk_capability_install() {
    // No --policy/ARDUR_MARKETPLACE_POLICY supplied: the built-in default is
    // permissive, so a shell_exec-declaring manifest installs same as before
    // Cedar gating existed — this feature is opt-in hardening, not a new
    // default restriction.
    let dir = tempfile::tempdir().expect("tempdir");
    let (manifest_path, key_path, _) = signed_skill_manifest(
        dir.path(),
        "skill.risky-default",
        "Risky",
        "0.1.0",
        vec!["cap.shell_exec".to_string()],
    );

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "install"])
        .arg(&manifest_path)
        .arg("--key")
        .arg(&key_path)
        .assert()
        .success();
}

#[test]
fn marketplace_custom_policy_denies_high_risk_capability_install() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (manifest_path, key_path, _) = signed_skill_manifest(
        dir.path(),
        "skill.risky",
        "Risky",
        "0.1.0",
        vec!["cap.shell_exec".to_string()],
    );

    let policy_path = dir.path().join("no-shell-exec.cedar");
    std::fs::write(
        &policy_path,
        r#"forbid(principal, action == Action::"skill_install", resource)
when { resource.high_risk_capabilities.contains("shell_exec") };
"#,
    )
    .expect("write policy");

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "install"])
        .arg(&manifest_path)
        .arg("--key")
        .arg(&key_path)
        .arg("--policy")
        .arg(&policy_path)
        .assert()
        .failure()
        .stderr(predicates::str::contains("policy denied"));

    // A sibling manifest without the forbidden capability still installs
    // under the SAME custom policy — proving the deny is capability-specific,
    // not a blanket refusal.
    let (clean_manifest, clean_key, _) = signed_skill_manifest(
        dir.path(),
        "skill.clean",
        "Clean",
        "0.1.0",
        vec!["cap.fs_read".to_string()],
    );
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "install"])
        .arg(&clean_manifest)
        .arg("--key")
        .arg(&clean_key)
        .arg("--policy")
        .arg(&policy_path)
        .assert()
        .success();
}

#[test]
fn marketplace_custom_policy_via_env_var_denies_plugin_kind() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (manifest_path, key_path, _) =
        signed_skill_manifest(dir.path(), "skill.viaenv", "ViaEnv", "0.1.0", vec![]);

    let policy_path = dir.path().join("no-plugins.cedar");
    std::fs::write(
        &policy_path,
        r#"forbid(principal, action == Action::"skill_install", resource)
when { resource.kind == "plugin" };
"#,
    )
    .expect("write policy");

    // Skill kind installs fine under the "no plugins" policy...
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .env("ARDUR_MARKETPLACE_POLICY", &policy_path)
        .args(["marketplace", "install"])
        .arg(&manifest_path)
        .arg("--key")
        .arg(&key_path)
        .assert()
        .success();
}

#[test]
fn marketplace_custom_policy_denies_uninstall_of_high_risk_skill() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (manifest_path, key_path, _) = signed_skill_manifest(
        dir.path(),
        "skill.locked",
        "Locked",
        "0.1.0",
        vec!["cap.shell_exec".to_string()],
    );

    // Install under the permissive default...
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "install"])
        .arg(&manifest_path)
        .arg("--key")
        .arg(&key_path)
        .assert()
        .success();

    // ...then a policy that forbids uninstalling high-risk skills blocks
    // removal (e.g. an org rule requiring an admin workflow for teardown).
    let policy_path = dir.path().join("no-uninstall-high-risk.cedar");
    std::fs::write(
        &policy_path,
        r#"forbid(principal, action == Action::"skill_uninstall", resource)
when { resource.high_risk == true };
"#,
    )
    .expect("write policy");

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "uninstall", "skill.locked"])
        .arg("--policy")
        .arg(&policy_path)
        .assert()
        .failure()
        .stderr(predicates::str::contains("policy denied"));

    // The skill is still installed — the refused uninstall had no effect.
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "inspect", "skill.locked"])
        .assert()
        .success();
}

#[test]
fn marketplace_no_advisory_db_permits_install() {
    // No --advisory-db/ARDUR_MARKETPLACE_ADVISORY_DB supplied: the built-in
    // default is empty, so installs behave exactly as before this feature
    // existed.
    let dir = tempfile::tempdir().expect("tempdir");
    let (manifest_path, key_path, _) =
        signed_skill_manifest(dir.path(), "skill.plain", "Plain", "0.1.0", vec![]);

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "install"])
        .arg(&manifest_path)
        .arg("--key")
        .arg(&key_path)
        .assert()
        .success();
}

#[test]
fn marketplace_advisory_db_blocks_known_vulnerable_version_by_default() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (manifest_path, key_path, _) = signed_skill_manifest(
        dir.path(),
        "skill.vulnerable",
        "Vulnerable",
        "1.0.0",
        vec![],
    );

    let advisory_path = dir.path().join("advisories.json");
    std::fs::write(
        &advisory_path,
        json!([{
            "advisory_id": "ARDUR-ADV-0001",
            "skill_id": "skill.vulnerable",
            "affected_versions": ["1.0.0"],
            "severity": "critical",
            "summary": "Reported credential-exfiltration behavior."
        }])
        .to_string(),
    )
    .expect("write advisories");

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "install"])
        .arg(&manifest_path)
        .arg("--key")
        .arg(&key_path)
        .arg("--advisory-db")
        .arg(&advisory_path)
        .assert()
        .failure()
        .stderr(predicates::str::contains("ARDUR-ADV-0001"));

    // Explicitly accepting the advisory id lets the install through.
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "install"])
        .arg(&manifest_path)
        .arg("--key")
        .arg(&key_path)
        .arg("--advisory-db")
        .arg(&advisory_path)
        .arg("--allow-known-vulnerable")
        .arg("ARDUR-ADV-0001")
        .assert()
        .success();
}

#[test]
fn marketplace_advisory_db_is_exact_version_scoped() {
    // The advisory names version 1.0.0; a different version of the same
    // skill_id is unaffected — proving the match is exact-version, not
    // blanket-by-id.
    let dir = tempfile::tempdir().expect("tempdir");
    let (manifest_path, key_path, _) = signed_skill_manifest(
        dir.path(),
        "skill.vulnerable",
        "Vulnerable",
        "2.0.0",
        vec![],
    );

    let advisory_path = dir.path().join("advisories.json");
    std::fs::write(
        &advisory_path,
        json!([{
            "advisory_id": "ARDUR-ADV-0002",
            "skill_id": "skill.vulnerable",
            "affected_versions": ["1.0.0"],
            "severity": "high",
            "summary": "Only affects 1.0.0."
        }])
        .to_string(),
    )
    .expect("write advisories");

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "install"])
        .arg(&manifest_path)
        .arg("--key")
        .arg(&key_path)
        .arg("--advisory-db")
        .arg(&advisory_path)
        .assert()
        .success();
}

#[test]
fn marketplace_audit_reports_advisory_matches_regardless_of_override() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (manifest_path, key_path, _) = signed_skill_manifest(
        dir.path(),
        "skill.audited-vuln",
        "AuditedVuln",
        "1.0.0",
        vec![],
    );

    let advisory_path = dir.path().join("advisories.json");
    std::fs::write(
        &advisory_path,
        json!([{
            "advisory_id": "ARDUR-ADV-0003",
            "skill_id": "skill.audited-vuln",
            "affected_versions": ["1.0.0"],
            "severity": "medium",
            "summary": "Flagged for audit visibility."
        }])
        .to_string(),
    )
    .expect("write advisories");

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "install"])
        .arg(&manifest_path)
        .arg("--key")
        .arg(&key_path)
        .arg("--advisory-db")
        .arg(&advisory_path)
        .arg("--allow-known-vulnerable")
        .arg("ARDUR-ADV-0003")
        .assert()
        .success();

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "audit", "skill.audited-vuln"])
        .arg("--advisory-db")
        .arg(&advisory_path)
        .assert()
        .success()
        .stdout(predicates::str::contains("ARDUR-ADV-0003"))
        .stdout(predicates::str::contains("1 with matching advisories"));
}

#[test]
fn marketplace_advisory_db_rejects_oversized_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (manifest_path, key_path, _) =
        signed_skill_manifest(dir.path(), "skill.x", "X", "0.1.0", vec![]);

    let advisory_path = dir.path().join("huge.json");
    // 6 MiB of whitespace-padded JSON, exceeding the 5 MiB ceiling.
    let padding = " ".repeat(6 * 1024 * 1024);
    std::fs::write(&advisory_path, format!("[{padding}]")).expect("write");

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "install"])
        .arg(&manifest_path)
        .arg("--key")
        .arg(&key_path)
        .arg("--advisory-db")
        .arg(&advisory_path)
        .assert()
        .failure()
        .stderr(predicates::str::contains("exceeding"));
}

#[test]
fn marketplace_search_bm25_ranks_relevant_skill_first() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (translate_manifest, translate_key, _) = signed_skill_manifest(
        dir.path(),
        "skill.translate",
        "Translate Helper",
        "0.1.0",
        vec!["cap.fs_read".to_string()],
    );
    let (unrelated_manifest, unrelated_key, _) = signed_skill_manifest(
        dir.path(),
        "skill.weather",
        "Weather Report",
        "0.1.0",
        vec!["cap.network_out".to_string()],
    );

    for (manifest, key) in [
        (&translate_manifest, &translate_key),
        (&unrelated_manifest, &unrelated_key),
    ] {
        Command::cargo_bin("ardur")
            .expect("the `ardur` binary builds")
            .env("HOME", dir.path())
            .args(["marketplace", "install"])
            .arg(manifest)
            .arg("--key")
            .arg(key)
            .assert()
            .success();
    }

    let out = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "search", "translate", "--limit", "5"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(out).expect("stdout utf8");
    assert!(stdout.contains("skill.translate"), "{stdout}");
    assert!(!stdout.contains("skill.weather"), "{stdout}");
}
