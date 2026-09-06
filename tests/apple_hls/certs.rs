//! A test CA plus a localhost leaf, and a temporary macOS keychain that
//! trusts the CA.
//!
//! `NSURLSession` (what `mediastreamvalidator` uses) will not trust a
//! self-signed leaf. The leaf is therefore signed by a generated CA, and that
//! CA is installed as a trust root in a disposable user keychain for the
//! duration of the test process.

use std::{
    fs,
    net::{IpAddr, Ipv4Addr},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, SanType,
};
use rushls::server::http::TlsSettings;

pub struct SharedHttps {
    pub settings: TlsSettings,
    pub ca_pem: PathBuf,
}

struct ProcessCleanup {
    previous: Vec<String>,
    keychain: PathBuf,
    keychain_dir: PathBuf,
    tls_dir: PathBuf,
}

static CLEANUP: parking_lot::Mutex<Option<ProcessCleanup>> = parking_lot::Mutex::new(None);

extern "C" fn restore_process_trust() {
    let Some(cleanup) = CLEANUP.lock().take() else {
        return;
    };
    restore_user_keychains(&cleanup.previous);
    let _ = Command::new("security")
        .args(["delete-keychain", &path_str(&cleanup.keychain)])
        .status();
    let _ = fs::remove_dir_all(&cleanup.keychain_dir);
    let _ = fs::remove_dir_all(&cleanup.tls_dir);
}

/// One CA, leaf, and keychain for the whole test binary.
///
/// macOS rate-limits `add-trusted-cert`. Installing a root per test fails
/// after a handful of cases; sharing one process-wide root does not.
pub fn shared_https() -> Result<Option<Arc<SharedHttps>>, Box<dyn std::error::Error + Send + Sync>>
{
    static ONCE: OnceLock<Result<Option<Arc<SharedHttps>>, String>> = OnceLock::new();
    match ONCE.get_or_init(|| install_shared().map_err(|error| error.to_string())) {
        Ok(value) => Ok(value.clone()),
        Err(error) => Err(error.clone().into()),
    }
}

fn install_shared() -> Result<Option<Arc<SharedHttps>>, Box<dyn std::error::Error + Send + Sync>> {
    let tls = write_localhost_https()?;
    let Some(trust) = trust_ca(&tls.ca_pem)? else {
        let _ = fs::remove_dir_all(&tls.directory);
        return Ok(None);
    };
    *CLEANUP.lock() = Some(ProcessCleanup {
        previous: trust.previous,
        keychain: trust.keychain,
        keychain_dir: trust.directory,
        tls_dir: tls.directory,
    });
    // OnceLock never drops, so restore the user keychain list when the process
    // exits rather than leaving a disposable trust root in the search path.
    // SAFETY: `restore_process_trust` only locks a process-static mutex and
    // runs `security` / filesystem cleanup; it is safe to call at exit.
    if unsafe { libc::atexit(restore_process_trust) } != 0 {
        restore_process_trust();
        return Err("could not register Apple HLS keychain restore".into());
    }
    Ok(Some(Arc::new(SharedHttps {
        settings: tls.settings,
        ca_pem: tls.ca_pem,
    })))
}

struct TlsMaterial {
    settings: TlsSettings,
    ca_pem: PathBuf,
    directory: PathBuf,
}

/// Writes a CA and a `localhost` / `127.0.0.1` leaf under a unique temp dir.
fn write_localhost_https() -> Result<TlsMaterial, Box<dyn std::error::Error + Send + Sync>> {
    let directory = unique_temp_dir("rushls-apple-hls-tls")?;
    fs::create_dir_all(&directory)?;

    let mut ca_params = CertificateParams::new(Vec::<String>::new())?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    ca_params.distinguished_name = DistinguishedName::new();
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "rushls Apple HLS test CA");
    let ca_key = KeyPair::generate()?;
    let ca_cert = ca_params.self_signed(&ca_key)?;
    let issuer = Issuer::new(ca_params, ca_key);

    let mut leaf_params = CertificateParams::new(Vec::<String>::new())?;
    leaf_params.subject_alt_names = vec![
        SanType::DnsName("localhost".try_into()?),
        SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST)),
    ];
    leaf_params.distinguished_name = DistinguishedName::new();
    leaf_params
        .distinguished_name
        .push(DnType::CommonName, "localhost");
    leaf_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let leaf_key = KeyPair::generate()?;
    let leaf_cert = leaf_params.signed_by(&leaf_key, &issuer)?;

    let ca_pem = directory.join("ca.pem");
    let certificate = directory.join("cert.pem");
    let key = directory.join("key.pem");
    fs::write(&ca_pem, ca_cert.pem())?;
    // Leaf first, then the issuing CA, matching what TlsSettings documents.
    fs::write(
        &certificate,
        format!("{}{}", leaf_cert.pem(), ca_cert.pem()),
    )?;
    fs::write(&key, leaf_key.serialize_pem())?;

    Ok(TlsMaterial {
        settings: TlsSettings {
            certificate,
            key,
            ..TlsSettings::default()
        },
        ca_pem,
        directory,
    })
}

/// Trusts `ca_pem` in a temporary user keychain, restoring the search list on drop.
///
/// Returns `Ok(None)` when the platform cannot install a trust root without a
/// GUI prompt; callers skip rather than fail CI or a locked-down laptop.
fn trust_ca(ca_pem: &Path) -> Result<Option<TrustedCa>, Box<dyn std::error::Error + Send + Sync>> {
    if cfg!(not(target_os = "macos")) {
        return Ok(None);
    }

    let directory = unique_temp_dir("rushls-apple-hls-keychain")?;
    fs::create_dir_all(&directory)?;
    let keychain = directory.join("rushls-apple-hls.keychain-db");
    let password = "rushls-apple-hls";

    let previous = list_user_keychains()?;
    if let Err(reason) = run_security(&["create-keychain", "-p", password, &path_str(&keychain)]) {
        let _ = fs::remove_dir_all(&directory);
        eprintln!("skipping Apple HLS tests: {reason}");
        return Ok(None);
    }
    let _ = run_security(&[
        "set-keychain-settings",
        "-lut",
        "21600",
        &path_str(&keychain),
    ]);
    if let Err(reason) = run_security(&["unlock-keychain", "-p", password, &path_str(&keychain)]) {
        abandon_keychain(&keychain, &directory, &[]);
        eprintln!("skipping Apple HLS tests: {reason}");
        return Ok(None);
    }

    let mut search = vec![path_str(&keychain)];
    search.extend(previous.iter().cloned());
    let mut list_args = vec![
        "list-keychains".to_owned(),
        "-d".to_owned(),
        "user".to_owned(),
        "-s".to_owned(),
    ];
    list_args.extend(search);
    if let Err(reason) = run_security_owned(&list_args) {
        abandon_keychain(&keychain, &directory, &previous);
        eprintln!("skipping Apple HLS tests: {reason}");
        return Ok(None);
    }

    // `-d` is the user domain. A GUI authorization sheet means we skip.
    if let Err(reason) = run_security(&[
        "add-trusted-cert",
        "-d",
        "-r",
        "trustRoot",
        "-p",
        "ssl",
        "-k",
        &path_str(&keychain),
        &path_str(ca_pem),
    ]) {
        abandon_keychain(&keychain, &directory, &previous);
        eprintln!("skipping Apple HLS tests: {reason}");
        return Ok(None);
    }

    Ok(Some(TrustedCa {
        keychain,
        previous,
        directory,
    }))
}

struct TrustedCa {
    keychain: PathBuf,
    previous: Vec<String>,
    directory: PathBuf,
}

fn unique_temp_dir(prefix: &str) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    Ok(std::env::temp_dir().join(format!("{prefix}-{nonce}")))
}

fn path_str(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn run_security(args: &[&str]) -> Result<(), String> {
    let output = Command::new("security")
        .args(args)
        .output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "security {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn run_security_owned(args: &[String]) -> Result<(), String> {
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    run_security(&borrowed)
}

fn abandon_keychain(keychain: &Path, directory: &Path, previous: &[String]) {
    restore_user_keychains(previous);
    let _ = Command::new("security")
        .args(["delete-keychain", &path_str(keychain)])
        .status();
    let _ = fs::remove_dir_all(directory);
}

fn list_user_keychains() -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
    let output = Command::new("security")
        .args(["list-keychains", "-d", "user"])
        .output()?;
    if !output.status.success() {
        return Err("security list-keychains failed".into());
    }
    Ok(parse_keychain_list(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn parse_keychain_list(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| line.trim_matches('"').to_owned())
        .collect()
}

fn restore_user_keychains(previous: &[String]) {
    if previous.is_empty() {
        return;
    }
    let mut args = vec![
        "list-keychains".to_owned(),
        "-d".to_owned(),
        "user".to_owned(),
        "-s".to_owned(),
    ];
    args.extend(previous.iter().cloned());
    let _ = Command::new("security").args(args).status();
}
