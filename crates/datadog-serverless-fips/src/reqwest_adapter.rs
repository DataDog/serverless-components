use reqwest::ClientBuilder;
use std::error::Error;
#[cfg(feature = "fips")]
use tracing::debug;

/// Creates a reqwest client builder with TLS configuration.
/// When the "fips" feature is enabled, it uses a FIPS-compliant TLS configuration.
/// Otherwise, it uses reqwest's default rustls TLS implementation.
#[cfg(not(feature = "fips"))]
pub fn create_reqwest_client_builder() -> Result<ClientBuilder, Box<dyn Error>> {
    // Just return the default builder with rustls TLS
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

    // FIPS typically requires TLS 1.2 or higher
    let versions = rustls::ALL_VERSIONS.to_vec();
    let config_builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&versions)
        .map_err(|_| "Failed to set protocol versions")?;

    let config = config_builder
        .with_root_certificates(root_cert_store)
        .with_no_client_auth();

    if !config.fips() {
        return Err("The final TLS configuration is not FIPS-compliant".into());
    }
    debug!("Client builder is configured with FIPS.");

    Ok(reqwest::Client::builder().use_preconfigured_tls(config))
}
