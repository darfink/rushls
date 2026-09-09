//! Server certificates for the Apple HLS suite, without touching system trust.
//!
//! `NSURLSession` (what `mediastreamvalidator` uses) will not trust a
//! self-signed leaf, and the only ways to make it trust one are to mutate the
//! login keychain or to authenticate an admin trust store change. Both need a
//! human at the machine, and the first leaves state behind when a test binary
//! is killed, so neither is done here.
//!
//! Instead the suite runs cleartext by default: `mediastreamvalidator` and
//! `hlsreport` validate an `http://` origin exactly as they validate an
//! `https://` one, and every rule they check is about playlists and segments
//! rather than about the transport. TLS is opt-in for the one thing cleartext
//! genuinely cannot cover — HTTP/2 delivery, which Apple's client reaches only
//! over TLS — and is enabled by pointing the suite at material the machine
//! already trusts:
//!
//! * `RUSHLS_TEST_TLS_CERT` + `RUSHLS_TEST_TLS_KEY` — a ready leaf chain for
//!   `127.0.0.1`, already trusted by this machine.
//! * `RUSHLS_TEST_CA_CERT` + `RUSHLS_TEST_CA_KEY` — an already-trusted CA the
//!   suite mints a short-lived `127.0.0.1` leaf from. Preferred: a leaf is
//!   generated per process, so nothing long-lived is checked out or reused.
//!
//! Either way the trust decision was made once, by a person, outside the test
//! suite. Nothing here installs, removes, or reorders a keychain.

use std::{
    fs,
    net::{IpAddr, Ipv4Addr},
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use rcgen::{
    CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, Issuer, KeyPair,
    KeyUsagePurpose, SanType,
};
use rushls::server::http::TlsSettings;

/// Where a test's certificate came from, for the skip message.
pub struct SharedHttps {
    pub settings: TlsSettings,
    /// Handed to `curl` while polling for the playlist. `None` when the leaf
    /// chains to a root `curl` already has.
    pub ca_pem: Option<PathBuf>,
    directory: Option<PathBuf>,
}

impl Drop for SharedHttps {
    fn drop(&mut self) {
        if let Some(directory) = &self.directory {
            let _ = fs::remove_dir_all(directory);
        }
    }
}

/// TLS material for the whole test binary, or `None` to serve cleartext.
///
/// One leaf per process rather than per test: minting is cheap, but a fresh
/// listener certificate per case would multiply the material a reader has to
/// account for without testing anything the first one does not.
pub fn shared_https() -> Result<Option<Arc<SharedHttps>>, Box<dyn std::error::Error + Send + Sync>>
{
    static ONCE: OnceLock<Result<Option<Arc<SharedHttps>>, String>> = OnceLock::new();
    match ONCE.get_or_init(|| load().map_err(|error| error.to_string())) {
        Ok(value) => Ok(value.clone()),
        Err(error) => Err(error.clone().into()),
    }
}

/// Whether the suite was asked for TLS but could not honour it.
///
/// Distinct from "TLS was never requested": a machine that set the variables
/// and got cleartext anyway should hear why rather than see a green run.
pub fn tls_requested() -> bool {
    ["RUSHLS_TEST_TLS_CERT", "RUSHLS_TEST_CA_CERT"]
        .iter()
        .any(|name| std::env::var_os(name).is_some())
}

fn load() -> Result<Option<Arc<SharedHttps>>, Box<dyn std::error::Error + Send + Sync>> {
    if let (Some(certificate), Some(key)) =
        (var("RUSHLS_TEST_TLS_CERT"), var("RUSHLS_TEST_TLS_KEY"))
    {
        let certificate = PathBuf::from(certificate);
        let key = PathBuf::from(key);
        if !certificate.is_file() || !key.is_file() {
            return Err("RUSHLS_TEST_TLS_CERT/KEY do not both name a file".into());
        }
        return Ok(Some(Arc::new(SharedHttps {
            settings: TlsSettings {
                certificate: certificate.clone(),
                key,
                ..TlsSettings::default()
            },
            // A supplied chain is trusted by the machine, so `curl` needs the
            // chain itself only when it does not share that trust store.
            ca_pem: Some(certificate),
            directory: None,
        })));
    }

    let (Some(ca_certificate), Some(ca_key)) =
        (var("RUSHLS_TEST_CA_CERT"), var("RUSHLS_TEST_CA_KEY"))
    else {
        return Ok(None);
    };
    Ok(Some(Arc::new(mint_leaf(
        &PathBuf::from(ca_certificate),
        &PathBuf::from(ca_key),
    )?)))
}

fn var(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Signs a throwaway `127.0.0.1` leaf with an already-trusted CA.
fn mint_leaf(
    ca_certificate: &std::path::Path,
    ca_key: &std::path::Path,
) -> Result<SharedHttps, Box<dyn std::error::Error + Send + Sync>> {
    let ca_pem = fs::read_to_string(ca_certificate)
        .map_err(|error| format!("RUSHLS_TEST_CA_CERT: {error}"))?;
    let key_pem =
        fs::read_to_string(ca_key).map_err(|error| format!("RUSHLS_TEST_CA_KEY: {error}"))?;
    let issuer = Issuer::from_ca_cert_pem(&ca_pem, KeyPair::from_pem(&key_pem)?)?;

    let mut leaf = CertificateParams::new(Vec::<String>::new())?;
    leaf.subject_alt_names = vec![
        SanType::DnsName("localhost".try_into()?),
        SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST)),
    ];
    leaf.distinguished_name = DistinguishedName::new();
    leaf.distinguished_name
        .push(DnType::CommonName, "localhost");
    leaf.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    leaf.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let leaf_key = KeyPair::generate()?;
    let leaf_certificate = leaf.signed_by(&leaf_key, &issuer)?;

    let directory = std::env::temp_dir().join(format!(
        "rushls-apple-hls-tls-{}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    fs::create_dir_all(&directory)?;
    let certificate = directory.join("cert.pem");
    let key = directory.join("key.pem");
    let chain = directory.join("ca.pem");
    // Leaf first, then the issuing CA, matching what TlsSettings documents.
    fs::write(&certificate, format!("{}{ca_pem}", leaf_certificate.pem()))?;
    fs::write(&key, leaf_key.serialize_pem())?;
    fs::write(&chain, &ca_pem)?;

    Ok(SharedHttps {
        settings: TlsSettings {
            certificate,
            key,
            ..TlsSettings::default()
        },
        ca_pem: Some(chain),
        directory: Some(directory),
    })
}
