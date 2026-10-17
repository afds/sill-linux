//! This host's identity and the devices it trusts, kept in `$XDG_STATE_HOME/sill-linux/`
//! (0700): `host-key.pk8` (its P-256 key, PKCS#8, 0600), `recognition-key` (32 bytes, 0600) and
//! `paired.json` (the paired devices' fingerprints, 0600). The key is what devices pin, so it
//! stays the same across runs: a new one means every device pairs again (Sill's "new key").

use crate::pairing::{self, b64url};
use anyhow::{anyhow, Context, Result};
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PairedDevice {
    /// base64url of the device key's fingerprint.
    pub fingerprint: String,
    pub name: String,
    pub model: Option<String>,
    /// Unix seconds.
    pub paired_at: f64,
    /// "code" or "qr".
    pub method: String,
}

pub struct Identity {
    pub dir: PathBuf,
    pub key_pkcs8: Vec<u8>,
    pub cert_der: Vec<u8>,
    pub fingerprint: [u8; 32],
    pub mac_id: String,
    pub recognition_key: [u8; 32],
}

pub fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("sill-linux")
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    fs::rename(&tmp, path)?;
    Ok(())
}

impl Identity {
    /// Loads the identity from `dir`, making the key and recognition key on first use.
    pub fn load_or_create(dir: &Path) -> Result<Identity> {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).with_context(|| format!("creating {}", dir.display()))?;
        let rng = SystemRandom::new();
        let key_path = dir.join("host-key.pk8");
        let key_pkcs8 = match fs::read(&key_path) {
            Ok(k) => k,
            Err(_) => {
                let k = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng)
                    .map_err(|_| anyhow!("cannot make a P-256 key"))?;
                write_private(&key_path, k.as_ref())?;
                k.as_ref().to_vec()
            }
        };
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &key_pkcs8, &rng)
            .map_err(|_| anyhow!("{} is not a P-256 key", key_path.display()))?;
        let fingerprint = pairing::fingerprint(pair.public_key().as_ref());

        let rk_path = dir.join("recognition-key");
        let recognition_key: [u8; 32] = match fs::read(&rk_path).ok().and_then(|v| v.try_into().ok()) {
            Some(k) => k,
            None => {
                let mut k = [0u8; 32];
                rng.fill(&mut k).map_err(|_| anyhow!("random source"))?;
                write_private(&rk_path, &k)?;
                k
            }
        };

        // A certificate made fresh at every launch from the stored key: the pin is the key.
        let signing = rcgen::KeyPair::try_from(key_pkcs8.as_slice()).context("loading the key for the certificate")?;
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new())?;
        params.distinguished_name = rcgen::DistinguishedName::new();
        params.distinguished_name.push(rcgen::DnType::CommonName, hex16(&rng));
        let cert = params.self_signed(&signing).context("making the certificate")?;

        Ok(Identity {
            dir: dir.to_path_buf(),
            cert_der: cert.der().to_vec(),
            mac_id: pairing::mac_id(&fingerprint),
            key_pkcs8,
            fingerprint,
            recognition_key,
        })
    }

    pub fn fingerprint_b64(&self) -> String {
        b64url(&self.fingerprint)
    }

    /// A TXT `r` for this host, with a fresh nonce.
    pub fn recognition_tag(&self) -> String {
        let mut nonce = [0u8; 6];
        SystemRandom::new().fill(&mut nonce).expect("random source");
        pairing::recognition_tag(&self.recognition_key, nonce)
    }

    pub fn load_paired(&self) -> Vec<PairedDevice> {
        fs::read(self.dir.join("paired.json")).ok().and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or_default()
    }

    pub fn save_paired(&self, devices: &[PairedDevice]) -> Result<()> {
        write_private(&self.dir.join("paired.json"), &serde_json::to_vec_pretty(devices)?)
    }
}

/// 16 random hex characters: the certificate names nothing, as Sill's own.
fn hex16(rng: &SystemRandom) -> String {
    let mut b = [0u8; 8];
    rng.fill(&mut b).expect("random source");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_stable() {
        let dir = std::env::temp_dir().join(format!("sill-linux-test-{}", uuid::Uuid::new_v4()));
        let a = Identity::load_or_create(&dir).unwrap();
        let b = Identity::load_or_create(&dir).unwrap();
        assert_eq!(a.fingerprint, b.fingerprint);
        assert_eq!(a.recognition_key, b.recognition_key);
        assert_eq!(a.mac_id.len(), 16);
        // The certificate's key is the stored key.
        assert_eq!(crate::tls::spki_fingerprint(&a.cert_der), Some(a.fingerprint));
        let mode = |p: &str| fs::metadata(dir.join(p)).unwrap().permissions().mode() & 0o777;
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(mode("host-key.pk8"), 0o600);
        assert_eq!(mode("recognition-key"), 0o600);
        let d = PairedDevice { fingerprint: "x".into(), name: "iPad".into(), model: None, paired_at: 1.0, method: "code".into() };
        a.save_paired(&[d.clone()]).unwrap();
        assert_eq!(b.load_paired(), vec![d]);
        fs::remove_dir_all(dir).unwrap();
    }
}
