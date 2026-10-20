use reqwest::ClientBuilder;
use std::error::Error;
#[cfg(feature = "fips")]
use std::sync::Arc;
#[cfg(feature = "fips")]
use tracing::debug;

#[cfg(feature = "fips")]
use crate::certificate_distrust::DistrustServerCertVerifier;

/// Creates a reqwest client builder with TLS configuration.
/// When the "fips" feature is enabled, it uses a FIPS-compliant TLS configuration.
/// Otherwise, it uses reqwest's default rustls TLS implementation.
#[cfg(not(feature = "fips"))]
pub fn create_reqwest_client_builder() -> Result<ClientBuilder, Box<dyn Error>> {
    // Just return the default builder with rustls TLS. This is the one place we should be okay
    // to call reqwest::Client::builder().
    #[allow(clippy::disallowed_methods)]
    Ok(reqwest::Client::builder().use_rustls_tls())
}

/// Creates a reqwest client builder with FIPS-compliant TLS configuration.
/// This version loads native root certificates and verifies FIPS compliance.
#[cfg(feature = "fips")]
pub fn create_reqwest_client_builder() -> Result<ClientBuilder, Box<dyn Error>> {
    // Get the runtime crypto provider that should have been configured at the start of the
    // application using something like rustls::crypto::default_fips_provider().install_default()
    let provider =
        rustls::crypto::CryptoProvider::get_default().ok_or("No crypto provider configured")?;

    if !provider.fips() {
        return Err("Crypto provider is not FIPS-compliant".into());
    }

    let mut root_cert_store = rustls::RootCertStore::empty();
    let native_certs = rustls_native_certs::load_native_certs();
    let mut valid_count = 0;
    for cert in native_certs.certs {
        match root_cert_store.add(cert) {
            Ok(()) => valid_count += 1,
            Err(err) => {
                debug!("Failed to parse certificate: {:?}", err);
            }
        }
    }
    if valid_count == 0 {
        return Err("No valid certificates found in native root store".into());
    }

    let config = fips_client_config(root_cert_store, provider.clone())?;
    debug!("Client builder is configured with FIPS.");

    // This is the one place that it is okay to call reqwest::Client::builder().
    #[allow(clippy::disallowed_methods)]
    Ok(reqwest::Client::builder().use_preconfigured_tls(config))
}

/// Builds the FIPS TLS configuration, rejecting certificates that use known compromised keys.
#[cfg(feature = "fips")]
fn fips_client_config(
    root_cert_store: rustls::RootCertStore,
    provider: Arc<rustls::crypto::CryptoProvider>,
) -> Result<rustls::ClientConfig, Box<dyn Error>> {
    // The verifier must use the FIPS provider: `ClientConfig::fips()` does not inspect it.
    let verifier = DistrustServerCertVerifier::new(Arc::new(root_cert_store), provider.clone())?;

    // FIPS typically requires TLS 1.2 or higher
    let versions = rustls::ALL_VERSIONS.to_vec();
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&versions)
        .map_err(|_| "Failed to set protocol versions")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();

    if !config.fips() {
        return Err("The final TLS configuration is not FIPS-compliant".into());
    }
    Ok(config)
}

#[cfg(all(test, feature = "fips"))]
mod tests {
    use rustls::{
        CertificateError, ClientConfig, ClientConnection, RootCertStore, ServerConfig,
        ServerConnection, SupportedProtocolVersion,
        crypto::CryptoProvider,
        pki_types::{
            CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, pem::PemObject as _,
        },
        server::{ClientHello, ResolvesServerCert},
        sign::CertifiedKey,
        version::{TLS12, TLS13},
    };

    use super::*;

    const ROOTS: &[u8] = include_bytes!("../tests/data/distrusted/roots.pem");
    const AFFECTED_CHAIN: &[u8] = include_bytes!("../tests/data/distrusted/dv-g3b-emudhra.pem");
    const AFFECTED_HOST: &str = "dv-g3b-emudhra.we-have-a-problem.com";

    fn provider() -> Arc<CryptoProvider> {
        Arc::new(rustls::crypto::default_fips_provider())
    }

    // Serves the captured chain with an unrelated generated key. The client must reject the chain
    // before it checks the server's handshake signature.
    #[derive(Debug)]
    struct CapturedChain(Arc<CertifiedKey>);

    impl ResolvesServerCert for CapturedChain {
        fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
            Some(Arc::clone(&self.0))
        }
    }

    fn handshake(
        client_config: ClientConfig,
        version: &'static SupportedProtocolVersion,
    ) -> Result<(), rustls::Error> {
        let chain = CertificateDer::pem_slice_iter(AFFECTED_CHAIN)
            .collect::<Result<Vec<_>, _>>()
            .expect("valid certificate chain");
        let pkcs8 = aws_lc_rs::signature::EcdsaKeyPair::generate_pkcs8(
            &aws_lc_rs::signature::ECDSA_P256_SHA256_ASN1_SIGNING,
            &aws_lc_rs::rand::SystemRandom::new(),
        )
        .expect("generated key");
        let key = provider()
            .key_provider
            .load_private_key(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                pkcs8.as_ref().to_vec(),
            )))
            .expect("loadable key");
        let server_config = ServerConfig::builder_with_provider(provider())
            .with_protocol_versions(&[version])
            .expect("supported protocol version")
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(CapturedChain(Arc::new(CertifiedKey::new(
                chain, key,
            )))));
        let mut server = ServerConnection::new(Arc::new(server_config)).unwrap();
        let mut client = ClientConnection::new(
            Arc::new(client_config),
            ServerName::try_from(AFFECTED_HOST).unwrap(),
        )
        .unwrap();
        for _ in 0..10 {
            let mut to_server = Vec::new();
            client.write_tls(&mut to_server).unwrap();
            server.read_tls(&mut to_server.as_slice()).unwrap();
            server.process_new_packets()?;
            let mut to_client = Vec::new();
            server.write_tls(&mut to_client).unwrap();
            client.read_tls(&mut to_client.as_slice()).unwrap();
            client.process_new_packets()?;
            if !client.is_handshaking() && !server.is_handshaking() {
                return Ok(());
            }
        }
        panic!("handshake did not finish");
    }

    #[test]
    fn fips_client_config_rejects_distrusted_keys_in_tls12_and_tls13_handshakes() {
        let mut roots = RootCertStore::empty();
        for root in CertificateDer::pem_slice_iter(ROOTS) {
            roots
                .add(root.expect("valid root PEM"))
                .expect("valid root certificate");
        }
        for version in [&TLS12, &TLS13] {
            let config = fips_client_config(roots.clone(), provider())
                .expect("FIPS-compliant configuration");
            assert_eq!(
                handshake(config, version),
                Err(rustls::Error::InvalidCertificate(
                    CertificateError::ApplicationVerificationFailure
                ))
            );
        }
    }
}
