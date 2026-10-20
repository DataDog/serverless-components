// Copyright 2025-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Rejects compromised public keys in presented server certificates and configured trust anchors.
//!
//! The upstream WebPKI fix requires an aws-lc-rs version outside the current FIPS dependency
//! constraints. Remove this wrapper when the upstream fix is compatible with the approved crypto
//! dependencies.
//!
//! This check is deliberately conservative: a blocked certificate anywhere in the presented chain
//! rejects the connection, even if path building could ignore it or find an alternative valid path.
//! Every presented certificate must also parse successfully, including unused certificates.

use std::{error::Error as StdError, sync::Arc};

use rustls::{
    CertificateError, DigitallySignedStruct, DistinguishedName, Error, RootCertStore,
    SignatureScheme,
    client::{
        WebPkiServerVerifier,
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    },
    crypto::CryptoProvider,
    pki_types::{CertificateDer, ServerName, TrustAnchor, UnixTime},
    server::ParsedCertificate,
};

// Compare public keys to also reject reissued and cross-signed certificates using the same keys.
// These fixed, complete DER SPKIs have a two-byte SEQUENCE header followed by 118 bytes of
// contents. See data/distrusted/README.md for provenance and encoding.
const DISTRUSTED_SPKIS: &[&[u8; 120]] = &[
    include_bytes!("data/distrusted/EMDVTLSCAG3B.spki.der"),
    include_bytes!("data/distrusted/EMDVTLSCAG3C.spki.der"),
    include_bytes!("data/distrusted/EMEVTLSCAG3B.spki.der"),
    include_bytes!("data/distrusted/EMEVTLSCAG3C.spki.der"),
    include_bytes!("data/distrusted/EMOVTLSCAG3B.spki.der"),
    include_bytes!("data/distrusted/EMOVTLSCAG3C.spki.der"),
    include_bytes!("data/distrusted/EMOVCSCAG3B.spki.der"),
    include_bytes!("data/distrusted/EMOVCSCAG3C.spki.der"),
];

#[derive(Debug)]
pub(crate) struct DistrustServerCertVerifier {
    inner: Arc<WebPkiServerVerifier>,
}

impl DistrustServerCertVerifier {
    pub(crate) fn new(
        mut roots: Arc<RootCertStore>,
        provider: Arc<CryptoProvider>,
    ) -> Result<Self, Box<dyn StdError>> {
        let is_allowed = |anchor: &TrustAnchor<'_>| {
            !DISTRUSTED_SPKIS
                .iter()
                .any(|spki| anchor.subject_public_key_info.as_ref() == &spki[2..])
        };
        if roots.roots.iter().any(|anchor| !is_allowed(anchor)) {
            // A server need not send its trust anchor, so inspecting the presented chain alone is
            // insufficient.
            Arc::make_mut(&mut roots).roots.retain(is_allowed);
        }

        let inner = WebPkiServerVerifier::builder_with_provider(roots, provider)
            .build()
            .map_err(|error| {
                format!("Failed to build TLS verifier after applying certificate distrust: {error}")
            })?;
        Ok(Self { inner })
    }
}

impl ServerCertVerifier for DistrustServerCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        for cert in std::iter::once(end_entity).chain(intermediates) {
            let spki = ParsedCertificate::try_from(cert)?.subject_public_key_info();
            if DISTRUSTED_SPKIS
                .iter()
                .any(|blocked| spki.as_ref() == blocked.as_slice())
            {
                return Err(Error::InvalidCertificate(
                    CertificateError::ApplicationVerificationFailure,
                ));
            }
        }

        self.inner
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }

    fn requires_raw_public_keys(&self) -> bool {
        self.inner.requires_raw_public_keys()
    }

    fn root_hint_subjects(&self) -> Option<&[DistinguishedName]> {
        self.inner.root_hint_subjects()
    }
}

#[cfg(test)]
mod tests {
    // Regression tests with captured public certificate chains and unchanged trusted roots.
    // See tests/data/distrusted/README.md for the fixtures.

    use std::{slice, time::Duration};

    use rustls::pki_types::pem::PemObject as _;

    use super::*;

    // Freeze validation at capture time so certificate expiry cannot change the regression.
    const VALIDATION_TIME: UnixTime =
        UnixTime::since_unix_epoch(Duration::from_secs(1_792_507_200));
    const ROOTS: &[u8] = include_bytes!("../tests/data/distrusted/roots.pem");
    const UNAFFECTED: &[u8] = include_bytes!("../tests/data/distrusted/unaffected.pem");
    const AFFECTED_CHAINS: &[(&str, &[u8])] = &[
        (
            "dv-g3b-emudhra.we-have-a-problem.com",
            include_bytes!("../tests/data/distrusted/dv-g3b-emudhra.pem"),
        ),
        (
            "dv-g3c-emudhra.we-have-a-problem.com",
            include_bytes!("../tests/data/distrusted/dv-g3c-emudhra.pem"),
        ),
        (
            "ov-g3b-emudhra.we-have-a-problem.com",
            include_bytes!("../tests/data/distrusted/ov-g3b-emudhra.pem"),
        ),
        (
            "ov-g3c-emudhra.we-have-a-problem.com",
            include_bytes!("../tests/data/distrusted/ov-g3c-emudhra.pem"),
        ),
        (
            "ev-g3b-emudhra.we-have-a-problem.com",
            include_bytes!("../tests/data/distrusted/ev-g3b-emudhra.pem"),
        ),
        (
            "ev-g3c-emudhra.we-have-a-problem.com",
            include_bytes!("../tests/data/distrusted/ev-g3c-emudhra.pem"),
        ),
        (
            "dv-g3b-aaa-emudhra.we-have-a-problem.com",
            include_bytes!("../tests/data/distrusted/dv-g3b-aaa-emudhra.pem"),
        ),
        (
            "dv-g3c-aaa-emudhra.we-have-a-problem.com",
            include_bytes!("../tests/data/distrusted/dv-g3c-aaa-emudhra.pem"),
        ),
        (
            "ov-g3b-aaa-emudhra.we-have-a-problem.com",
            include_bytes!("../tests/data/distrusted/ov-g3b-aaa-emudhra.pem"),
        ),
        (
            "ov-g3c-aaa-emudhra.we-have-a-problem.com",
            include_bytes!("../tests/data/distrusted/ov-g3c-aaa-emudhra.pem"),
        ),
        (
            "ev-g3b-aaa-emudhra.we-have-a-problem.com",
            include_bytes!("../tests/data/distrusted/ev-g3b-aaa-emudhra.pem"),
        ),
        (
            "ev-g3c-aaa-emudhra.we-have-a-problem.com",
            include_bytes!("../tests/data/distrusted/ev-g3c-aaa-emudhra.pem"),
        ),
    ];

    fn provider() -> Arc<CryptoProvider> {
        Arc::new(rustls::crypto::default_fips_provider())
    }

    fn roots() -> RootCertStore {
        let mut roots = RootCertStore::empty();
        for root in CertificateDer::pem_slice_iter(ROOTS) {
            roots
                .add(root.expect("valid root PEM"))
                .expect("valid root certificate");
        }
        roots
    }

    fn chain(pem: &[u8]) -> Vec<CertificateDer<'_>> {
        CertificateDer::pem_slice_iter(pem)
            .collect::<Result<Vec<_>, _>>()
            .expect("valid certificate chain")
    }

    fn unprotected(roots: RootCertStore) -> Arc<WebPkiServerVerifier> {
        WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider())
            .build()
            .expect("valid verifier")
    }

    fn protected(roots: RootCertStore) -> DistrustServerCertVerifier {
        DistrustServerCertVerifier::new(Arc::new(roots), provider()).expect("valid verifier")
    }

    fn verify(
        verifier: &dyn ServerCertVerifier,
        host: &str,
        chain: &[CertificateDer<'_>],
    ) -> Result<(), Error> {
        let (leaf, intermediates) = chain.split_first().expect("nonempty chain");
        verifier
            .verify_server_cert(
                leaf,
                intermediates,
                &ServerName::try_from(host).expect("valid DNS name"),
                &[],
                VALIDATION_TIME,
            )
            .map(|_| ())
    }

    fn assert_distrusted(result: Result<(), Error>) {
        assert_eq!(
            result,
            Err(Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure
            ))
        );
    }

    #[test]
    fn rejects_all_twelve_affected_chains_accepted_by_unmodified_webpki() {
        let baseline = unprotected(roots());
        let verifier = protected(roots());
        for &(host, pem) in AFFECTED_CHAINS {
            let chain = chain(pem);
            verify(baseline.as_ref(), host, &chain).expect(host);
            assert_distrusted(verify(&verifier, host, &chain));
        }
    }

    #[test]
    fn accepts_unaffected_chain_with_the_same_roots() {
        let chain = chain(UNAFFECTED);
        verify(&protected(roots()), "testovg1.emsign.com", &chain)
            .expect("unaffected chain is valid");
    }

    #[test]
    fn rejects_blocked_key_when_presented_as_the_leaf() {
        let chain = chain(AFFECTED_CHAINS[0].1);
        assert_distrusted(verify(
            &protected(roots()),
            AFFECTED_CHAINS[0].0,
            &chain[1..],
        ));
    }

    #[test]
    fn rejects_unused_blocked_intermediate_even_when_a_valid_path_exists() {
        let mut control = chain(UNAFFECTED);
        let affected = chain(AFFECTED_CHAINS[0].1);
        control.push(affected[1].clone());
        verify(
            unprotected(roots()).as_ref(),
            "testovg1.emsign.com",
            &control,
        )
        .expect("blocked extra is not on the path");
        assert_distrusted(verify(&protected(roots()), "testovg1.emsign.com", &control));
    }

    #[test]
    fn rejects_blocked_trust_anchor_omitted_by_the_server() {
        let affected = chain(AFFECTED_CHAINS[0].1);
        let mut roots = roots();
        roots
            .add(affected[1].clone())
            .expect("intermediate can be trusted explicitly");
        verify(
            unprotected(roots.clone()).as_ref(),
            AFFECTED_CHAINS[0].0,
            &affected[..1],
        )
        .expect("unmodified verifier trusts the explicitly configured intermediate");
        let error = verify(&protected(roots), AFFECTED_CHAINS[0].0, &affected[..1])
            .expect_err("blocked anchor removed");
        assert_eq!(
            error,
            Error::InvalidCertificate(CertificateError::UnknownIssuer)
        );
    }

    #[test]
    fn rejects_configuration_when_all_anchors_are_blocked() {
        let affected = chain(AFFECTED_CHAINS[0].1);
        let mut roots = RootCertStore::empty();
        roots.add(affected[1].clone()).expect("valid intermediate");
        let error = DistrustServerCertVerifier::new(Arc::new(roots), provider())
            .expect_err("no usable trust anchors");
        assert!(error.to_string().contains("no root trust anchors"));
    }

    #[test]
    fn malformed_certificates_never_fall_back_to_success() {
        let verifier = protected(roots());
        let malformed = CertificateDer::from(&b"not a DER certificate"[..]);
        assert!(
            verify(
                &verifier,
                "testovg1.emsign.com",
                slice::from_ref(&malformed)
            )
            .is_err()
        );
        let mut control = chain(UNAFFECTED);
        control.push(malformed);
        assert!(verify(&verifier, "testovg1.emsign.com", &control).is_err());
    }

    #[test]
    fn preserves_hostname_expiry_and_certificate_signature_validation() {
        let verifier = protected(roots());
        let control = chain(UNAFFECTED);
        let name = ServerName::try_from("testovg1.emsign.com").unwrap();
        assert!(verify(&verifier, "wrong.example", &control).is_err());
        assert!(
            verifier
                .verify_server_cert(
                    &control[0],
                    &control[1..],
                    &name,
                    &[],
                    UnixTime::since_unix_epoch(Duration::from_secs(4_000_000_000)),
                )
                .is_err()
        );

        let mut damaged_leaf = control[0].to_vec();
        *damaged_leaf.last_mut().unwrap() ^= 1;
        let error = verifier
            .verify_server_cert(
                &damaged_leaf.into(),
                &control[1..],
                &name,
                &[],
                VALIDATION_TIME,
            )
            .expect_err("certificate signature must still be checked");
        assert!(matches!(
            error,
            Error::InvalidCertificate(CertificateError::BadSignature)
        ));
        assert_eq!(
            verifier.supported_verify_schemes(),
            unprotected(roots()).supported_verify_schemes()
        );
    }
}
