//! Optional TLS for Apple's validator without changing system trust.
//!
//! The validator has no documented custom-CA option. Supply a certificate
//! already trusted by macOS; curl's `--cacert` does not configure Apple's client.
//! `RUSHLS_TEST_TLS_CERT` and `RUSHLS_TEST_TLS_KEY` select a leaf chain and key.
//! `RUSHLS_TEST_TLS_HOST` selects its hostname (default: `127.0.0.1`), which must
//! resolve to `127.0.0.1`, optionally also `::1`. A publicly trusted DNS-01 certificate
//! allows this without Keychain changes, privileged ports, or hosts-file edits.
//! An already-trusted CA can alternatively be supplied with
//! `RUSHLS_TEST_CA_CERT` and `RUSHLS_TEST_CA_KEY` to mint a localhost leaf.
//! `RUSHLS_TEST_TLS_MIN_VERSION` and `RUSHLS_TEST_TLS_MAX_VERSION` select
//! protocol bounds ("1.2" or "1.3", both default to "1.3").
//! Without TLS, transport findings remain audit failures.

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
    [
        "RUSHLS_TEST_TLS_CERT",
        "RUSHLS_TEST_TLS_KEY",
        "RUSHLS_TEST_TLS_HOST",
        "RUSHLS_TEST_TLS_MIN_VERSION",
        "RUSHLS_TEST_TLS_MAX_VERSION",
        "RUSHLS_TEST_CA_CERT",
        "RUSHLS_TEST_CA_KEY",
    ]
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
                min_version: tls_version("RUSHLS_TEST_TLS_MIN_VERSION")?,
                max_version: tls_version("RUSHLS_TEST_TLS_MAX_VERSION")?,
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

fn tls_version(
    name: &str,
) -> Result<rushls_tls::TlsVersion, Box<dyn std::error::Error + Send + Sync>> {
    match std::env::var(name) {
        Ok(value) => value
            .parse()
            .map_err(|error| format!("{name}: {error}").into()),
        Err(std::env::VarError::NotPresent) => Ok(rushls_tls::TlsVersion::Tls13),
        Err(error) => Err(error.into()),
    }
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
            min_version: tls_version("RUSHLS_TEST_TLS_MIN_VERSION")?,
            max_version: tls_version("RUSHLS_TEST_TLS_MAX_VERSION")?,
            ..TlsSettings::default()
        },
        ca_pem: Some(chain),
        directory: Some(directory),
    })
}

/// Keep the certificate hostname while ensuring validation reaches our local listener.
pub async fn origin_host(port: u16) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let host = std::env::var("RUSHLS_TEST_TLS_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    if host.is_empty()
        || !host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-".contains(&byte))
    {
        return Err("RUSHLS_TEST_TLS_HOST must be a DNS hostname or 127.0.0.1".into());
    }
    let addresses = tokio::net::lookup_host((host.as_str(), port))
        .await?
        .collect::<Vec<_>>();
    if !local_origin_addresses(&addresses) {
        return Err(
            "RUSHLS_TEST_TLS_HOST must resolve to 127.0.0.1 and only loopback addresses".into(),
        );
    }
    Ok(host)
}

// The origin listens on IPv4. Dual-stack clients can fall back from ::1,
// but DNS must never send validation requests to a non-loopback address.
fn local_origin_addresses(addresses: &[std::net::SocketAddr]) -> bool {
    addresses
        .iter()
        .any(|address| address.ip() == Ipv4Addr::LOCALHOST)
        && addresses.iter().all(|address| address.ip().is_loopback())
}

#[test]
fn certificate_dns_allows_dual_stack_but_requires_the_local_origin()
-> Result<(), Box<dyn std::error::Error>> {
    for (addresses, expected) in [
        (vec!["127.0.0.1:443"], true),
        (vec!["127.0.0.1:443", "[::1]:443"], true),
        (vec!["[::1]:443"], false),
        (vec!["127.0.0.1:443", "192.0.2.1:443"], false),
        (vec!["127.0.0.1:443", "[2001:db8::1]:443"], false),
        (vec![], false),
    ] {
        let addresses = addresses
            .into_iter()
            .map(str::parse)
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(local_origin_addresses(&addresses), expected);
    }
    Ok(())
}
