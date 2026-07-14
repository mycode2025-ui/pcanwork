//! TLS support for Modbus/TCP Security: build a client `TlsConnector` and a
//! server `TlsAcceptor` from user-supplied certificates. The crypto provider is
//! `ring` (pinned in Cargo to avoid the aws-lc-rs C build on Windows).

use std::sync::Arc;

use anyhow::{anyhow, bail, Context as _};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use tokio_rustls::rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio_rustls::{TlsAcceptor, TlsConnector};

/// Client-side TLS options.
#[derive(Clone, Default)]
pub struct TlsClientCfg {
    /// PEM CA file to trust; empty → system/native roots.
    pub ca_file: String,
    /// Skip certificate verification entirely (testing with self-signed certs).
    pub skip_verify: bool,
    /// SNI / expected server name; empty → use the connection host.
    pub domain: String,
    /// Client certificate (PEM) for mutual TLS; empty → no client auth.
    pub client_cert: String,
    /// Client private key (PEM) for mutual TLS.
    pub client_key: String,
    /// Cipher strength: 0=auto, 1=AES-128, 2=AES-256, 3=ChaCha20-256.
    pub cipher: i32,
    /// Protocol version: 0=auto (1.2+1.3), 1=TLS 1.2 only, 2=TLS 1.3 only.
    pub version: i32,
}

/// Server-side TLS options.
#[derive(Clone, Default)]
pub struct TlsServerCfg {
    pub cert_file: String, // PEM certificate chain
    pub key_file: String,  // PEM private key (PKCS#8 / PKCS#1 / SEC1)
    /// Require + verify a client certificate (mutual TLS).
    pub require_client_cert: bool,
    /// PEM CA used to verify client certificates.
    pub client_ca: String,
    /// Cipher strength: 0=auto, 1=AES-128, 2=AES-256, 3=ChaCha20-256.
    pub cipher: i32,
    /// Protocol version: 0=auto (1.2+1.3), 1=TLS 1.2 only, 2=TLS 1.3 only.
    pub version: i32,
}

/// Make sure a process-wide crypto provider is installed (idempotent).
fn ensure_provider() {
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
}

/// A crypto provider whose cipher suites are filtered to the requested strength
/// (0=auto/all, 1=AES-128, 2=AES-256, 3=ChaCha20-256).
fn provider(cipher: i32) -> Arc<tokio_rustls::rustls::crypto::CryptoProvider> {
    use tokio_rustls::rustls::CipherSuite::*;
    let mut p = tokio_rustls::rustls::crypto::ring::default_provider();
    if cipher != 0 {
        p.cipher_suites.retain(|cs| match cipher {
            1 => matches!(
                cs.suite(),
                TLS13_AES_128_GCM_SHA256
                    | TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256
                    | TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256
            ),
            2 => matches!(
                cs.suite(),
                TLS13_AES_256_GCM_SHA384
                    | TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384
                    | TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384
            ),
            3 => matches!(
                cs.suite(),
                TLS13_CHACHA20_POLY1305_SHA256
                    | TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256
                    | TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256
            ),
            _ => true,
        });
    }
    Arc::new(p)
}

/// Protocol versions to allow for the selector (0=auto, 1=1.2 only, 2=1.3 only).
fn versions(version: i32) -> &'static [&'static tokio_rustls::rustls::SupportedProtocolVersion] {
    use tokio_rustls::rustls::version::{TLS12, TLS13};
    static AUTO: &[&tokio_rustls::rustls::SupportedProtocolVersion] = &[&TLS13, &TLS12];
    static V12: &[&tokio_rustls::rustls::SupportedProtocolVersion] = &[&TLS12];
    static V13: &[&tokio_rustls::rustls::SupportedProtocolVersion] = &[&TLS13];
    match version {
        1 => V12,
        2 => V13,
        _ => AUTO,
    }
}

/// Human description of a negotiated TLS session, e.g. "TLS1.3 · AES-256-GCM".
pub fn describe(common: &tokio_rustls::rustls::CommonState) -> String {
    use tokio_rustls::rustls::ProtocolVersion;
    let ver = match common.protocol_version() {
        Some(ProtocolVersion::TLSv1_3) => "TLS1.3",
        Some(ProtocolVersion::TLSv1_2) => "TLS1.2",
        _ => "TLS",
    };
    let suite = common
        .negotiated_cipher_suite()
        .map(|s| format!("{:?}", s.suite()))
        .unwrap_or_default();
    format!("{ver} · {suite}")
}

fn load_certs(path: &str) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    let data = std::fs::read(path).with_context(|| format!("read cert {path}"))?;
    let mut rd = std::io::BufReader::new(&data[..]);
    let certs = rustls_pemfile::certs(&mut rd).collect::<Result<Vec<_>, _>>()?;
    if certs.is_empty() {
        bail!("no certificates found in {path}");
    }
    Ok(certs)
}

fn load_key(path: &str) -> anyhow::Result<PrivateKeyDer<'static>> {
    let data = std::fs::read(path).with_context(|| format!("read key {path}"))?;
    let mut rd = std::io::BufReader::new(&data[..]);
    rustls_pemfile::private_key(&mut rd)?.ok_or_else(|| anyhow!("no private key found in {path}"))
}

/// Build a TLS connector for the client side.
pub fn client_connector(cfg: &TlsClientCfg) -> anyhow::Result<TlsConnector> {
    ensure_provider();
    // Stage 1: how we verify the SERVER cert (cipher strength filtered).
    let base = ClientConfig::builder_with_provider(provider(cfg.cipher))
        .with_protocol_versions(versions(cfg.version))?;
    let wants_client = if cfg.skip_verify {
        base.dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
    } else {
        let mut roots = RootCertStore::empty();
        if !cfg.ca_file.trim().is_empty() {
            for c in load_certs(&cfg.ca_file)? {
                roots.add(c)?;
            }
        } else {
            let native = rustls_native_certs::load_native_certs();
            for c in native.certs {
                let _ = roots.add(c);
            }
            if roots.is_empty() {
                bail!("no trusted roots available — provide a CA file or enable skip-verify");
            }
        }
        base.with_root_certificates(roots)
    };
    // Stage 2: optionally present a CLIENT cert (mutual TLS).
    let config = if !cfg.client_cert.trim().is_empty() {
        let certs = load_certs(&cfg.client_cert)?;
        let key = load_key(&cfg.client_key)?;
        wants_client.with_client_auth_cert(certs, key)?
    } else {
        wants_client.with_no_client_auth()
    };
    Ok(TlsConnector::from(Arc::new(config)))
}

/// Build a TLS acceptor for the server side.
pub fn server_acceptor(cfg: &TlsServerCfg) -> anyhow::Result<TlsAcceptor> {
    ensure_provider();
    let certs = load_certs(&cfg.cert_file)?;
    let key = load_key(&cfg.key_file)?;
    let base = ServerConfig::builder_with_provider(provider(cfg.cipher))
        .with_protocol_versions(versions(cfg.version))?;
    let config = if cfg.require_client_cert {
        let mut roots = RootCertStore::empty();
        for c in load_certs(&cfg.client_ca)? {
            roots.add(c)?;
        }
        let verifier =
            tokio_rustls::rustls::server::WebPkiClientVerifier::builder(Arc::new(roots)).build()?;
        base.with_client_cert_verifier(verifier)
            .with_single_cert(certs, key)?
    } else {
        base.with_no_client_auth().with_single_cert(certs, key)?
    };
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Resolve the SNI / server name to validate against.
pub fn server_name(cfg: &TlsClientCfg, host: &str) -> anyhow::Result<ServerName<'static>> {
    let d = if cfg.domain.trim().is_empty() {
        host
    } else {
        cfg.domain.trim()
    };
    ServerName::try_from(d.to_string()).map_err(|e| anyhow!("invalid server name '{d}': {e}"))
}

/// Dangerous verifier that accepts any server certificate (skip-verify mode).
#[derive(Debug)]
struct NoVerify;

impl tokio_rustls::rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<tokio_rustls::rustls::client::danger::ServerCertVerified, tokio_rustls::rustls::Error>
    {
        Ok(tokio_rustls::rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> Result<
        tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
        tokio_rustls::rustls::Error,
    > {
        Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> Result<
        tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
        tokio_rustls::rustls::Error,
    > {
        Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<tokio_rustls::rustls::SignatureScheme> {
        tokio_rustls::rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
