//! The TLS door, as Sill's `RemoteTLS` builds the host's end: TLS 1.3 only, no tickets or
//! resumption, a client certificate required, ALPNs `sill/1` and `sill-pair/1`. Trust is the
//! key alone: any P-256 client certificate passes the handshake (names, dates and chains mean
//! nothing), and the door decides afterwards by its fingerprint and the negotiated ALPN.

use crate::identity::Identity;
use crate::pairing;
use anyhow::{Context, Result};
use rustls::client::danger::HandshakeSignatureValid;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::server::NoServerSessionStorage;
use rustls::{DigitallySignedStruct, DistinguishedName, Error, SignatureScheme};
use std::sync::Arc;

pub const SESSION_ALPN: &[u8] = b"sill/1";
pub const PAIRING_ALPN: &[u8] = b"sill-pair/1";

/// One DER element at the start of `d`: (tag, contents, the rest after it).
fn der_element(d: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = d.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (first as usize, rest)
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 || rest.len() < n {
            return None;
        }
        (rest[..n].iter().fold(0usize, |a, &b| (a << 8) | b as usize), &rest[n..])
    };
    (rest.len() >= len).then(|| (tag, &rest[..len], &rest[len..]))
}

/// The 65-byte uncompressed point of a certificate's key when it is P-256, else None.
pub fn p256_point(cert_der: &[u8]) -> Option<Vec<u8>> {
    let (_, cert, _) = der_element(cert_der)?; // Certificate SEQUENCE
    let (_, mut tbs, _) = der_element(cert)?; // tbsCertificate SEQUENCE
    if tbs.first() == Some(&0xa0) {
        tbs = der_element(tbs)?.2; // [0] version
    }
    for _ in 0..5 {
        tbs = der_element(tbs)?.2; // serial, signature, issuer, validity, subject
    }
    let (_, spki, _) = der_element(tbs)?;
    let (_, algorithm, rest) = der_element(spki)?;
    let (_, bits, _) = der_element(rest)?;
    let ec = [0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
    let p256 = [0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
    if algorithm != [&ec[..], &p256[..]].concat() || bits.len() != 66 || bits[0] != 0 || bits[1] != 4 {
        return None;
    }
    Some(bits[1..].to_vec())
}

/// The fingerprint of a certificate's key (SHA-256 of its SPKI) when it is P-256.
pub fn spki_fingerprint(cert_der: &[u8]) -> Option<[u8; 32]> {
    p256_point(cert_der).map(|p| pairing::fingerprint(&p))
}

/// Any P-256 client certificate; its signature over the handshake is checked with its own key.
#[derive(Debug)]
struct AnyP256Client;

impl ClientCertVerifier for AnyP256Client {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(&self, end_entity: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: UnixTime) -> Result<ClientCertVerified, Error> {
        match p256_point(end_entity) {
            Some(_) => Ok(ClientCertVerified::assertion()),
            None => Err(Error::General("not a P-256 key".into())),
        }
    }

    fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::General("TLS 1.3 only".into()))
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, Error> {
        if dss.scheme != SignatureScheme::ECDSA_NISTP256_SHA256 {
            return Err(Error::General("unsupported signature scheme".into()));
        }
        let point = p256_point(cert).ok_or_else(|| Error::General("not a P-256 key".into()))?;
        ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_ASN1, &point)
            .verify(message, dss.signature())
            .map(|_| HandshakeSignatureValid::assertion())
            .map_err(|_| Error::General("bad client signature".into()))
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ECDSA_NISTP256_SHA256]
    }
}

pub fn server_config(identity: &Identity) -> Result<Arc<rustls::ServerConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .context("TLS 1.3")?
        .with_client_cert_verifier(Arc::new(AnyP256Client))
        .with_single_cert(
            vec![CertificateDer::from(identity.cert_der.clone())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.key_pkcs8.clone())),
        )
        .context("the host certificate")?;
    config.alpn_protocols = vec![SESSION_ALPN.to_vec(), PAIRING_ALPN.to_vec()];
    config.send_tls13_tickets = 0;
    config.session_storage = Arc::new(NoServerSessionStorage {});
    Ok(Arc::new(config))
}
