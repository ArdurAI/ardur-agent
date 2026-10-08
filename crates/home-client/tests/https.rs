mod support;
use home_client::{Error, HomeClient, PinnedTransport, pair_device};
use serde_json::json;
use support::{FakeHome, Mode};
#[tokio::test]
async fn disposable_pair_signed_status_bots_and_replay() {
    let home = FakeHome::start(false).await;
    let paired = pair_device(&home.code(), "Test CLI").await.unwrap();
    let client = HomeClient::new(paired).unwrap();
    assert_eq!(client.status().await.unwrap()["valid"], true);
    let bots = client.bots().await.unwrap();
    assert_eq!(bots["bots"][0]["id"], "bot");
    assert!(!bots.to_string().contains("sensitive-canary"));
    home.set(Mode::Replay);
    client.status().await.unwrap();
    assert!(matches!(client.status().await, Err(Error::Access)));
    home.set(Mode::Revoked);
    assert!(matches!(client.status().await, Err(Error::Access)));
}
#[tokio::test]
async fn wrong_or_expired_pin_sends_no_application_bytes() {
    for expired in [false, true] {
        let mut home = FakeHome::start(expired).await;
        if !expired {
            home.payload.certificate_fingerprint = "0".repeat(64)
        }
        let result = pair_device(&home.code(), "Test CLI").await;
        assert!(
            matches!(result, Err(Error::Identity)),
            "expected changed identity"
        );
        assert_eq!(
            home.decoded_bytes.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(home.count(), 0);
    }
}
#[tokio::test]
async fn home_proof_failures_prevent_pairing() {
    for mode in [
        Mode::WrongInstance,
        Mode::WrongFingerprint,
        Mode::BadSignature,
    ] {
        let home = FakeHome::start(false).await;
        home.set(mode);
        assert!(matches!(
            pair_device(&home.code(), "Test CLI").await,
            Err(Error::Identity)
        ));
        assert_eq!(home.count(), 1);
    }
}
#[tokio::test]
async fn invalid_nonce_prevents_signed_request() {
    for mode in [Mode::StaleNonce, Mode::BadNonce] {
        let home = FakeHome::start(false).await;
        let client = HomeClient::new(pair_device(&home.code(), "Test CLI").await.unwrap()).unwrap();
        home.set(mode);
        let before = home.count();
        assert!(matches!(client.status().await, Err(Error::Expired)));
        assert_eq!(home.count(), before + 1);
    }
}
#[tokio::test]
async fn redirects_bad_json_limits_and_remote_errors_are_not_echoed() {
    for mode in [
        Mode::Redirect,
        Mode::Malformed,
        Mode::Oversize,
        Mode::SecretError,
    ] {
        let home = FakeHome::start(false).await;
        home.set(mode);
        let error = pair_device(&home.code(), "Test CLI").await.err().unwrap();
        assert!(!error.to_string().contains("sensitive-canary"));
        assert_eq!(home.count(), 1);
    }
    let p = PinnedTransport::new(&"0".repeat(64)).unwrap();
    for url in [
        "http://127.0.0.1",
        "https://user:pass@home.test",
        "https://home.test?q=secret",
        "https://home.test#secret",
    ] {
        assert!(matches!(p.post(url, &json!({})).await, Err(Error::Input)));
    }
}

#[tokio::test]
async fn partial_http_headers_are_counted_as_application_bytes() {
    let home = FakeHome::start(false).await;
    let mut roots = rustls::RootCertStore::empty();
    roots.add(home.certificate.clone()).unwrap();
    let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let url = home_protocol::https_url(&home.payload.hints[0]).unwrap();
    let socket = tokio::net::TcpStream::connect(("127.0.0.1", url.port().unwrap()))
        .await
        .unwrap();
    let mut tls = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config))
        .connect(
            rustls::pki_types::ServerName::try_from("localhost").unwrap(),
            socket,
        )
        .await
        .unwrap();
    use tokio::io::AsyncWriteExt;
    tls.write_all(b"POST /").await.unwrap();
    tls.shutdown().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while home.decoded_bytes.load(std::sync::atomic::Ordering::SeqCst) != 6 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(home.count(), 0);
}
