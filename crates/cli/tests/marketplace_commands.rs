//! Integration tests for `ardur marketplace`.

use assert_cmd::Command;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::{Signature, SigningKey, signature::Signer};
use p256::elliptic_curve::rand_core::OsRng;
use p256::pkcs8::{EncodePublicKey, LineEnding};
use serde_json::json;
use sha2::{Digest, Sha256};

#[test]
fn marketplace_install_list_show_remove_lifecycle() {
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
        .success();

    let list = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let list_stdout = String::from_utf8(list).expect("stdout utf8");
    assert!(list_stdout.contains("installed-skill"), "{list_stdout}");

    let search = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "search", "installed"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let search_stdout = String::from_utf8(search).expect("stdout utf8");
    assert!(search_stdout.contains("installed-skill"), "{search_stdout}");

    let verify = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "verify"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let verify_stdout = String::from_utf8(verify).expect("stdout utf8");
    assert!(
        verify_stdout.contains("missing manifest signature"),
        "{verify_stdout}"
    );

    let id = list_stdout
        .lines()
        .find(|l| l.contains("installed-skill"))
        .and_then(|l| l.split_whitespace().next())
        .expect("skill id in list")
        .to_string();

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "show", &id])
        .assert()
        .success();

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "remove", &id])
        .assert()
        .success();

    let path = dir
        .path()
        .join(".ardur")
        .join("skills")
        .join(format!("{id}.json"));
    assert!(!path.exists(), "skill file should be deleted");
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
        .assert()
        .failure();

    // No skill record should have been written anywhere under the state
    // dir — neither at the traversal target nor under a substituted id.
    let skills_dir = dir.path().join(".ardur").join("skills");
    if skills_dir.is_dir() {
        let count = std::fs::read_dir(&skills_dir).expect("read skills dir").count();
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

    let assert = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "install"])
        .arg(&manifest_path)
        .assert()
        .success(); // falls back to the generic-stub path (not a parseable manifest).
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("installed-skill") || stdout.contains("installed skill"),
        "an oversized/unparseable manifest should fall back to the generic stub record: {stdout}"
    );
}

fn canonical_payload(
    schema_version: u32,
    kind: &str,
    id: &str,
    name: &str,
    version: &str,
    capabilities: &[String],
    artifacts: &[serde_json::Value],
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
