//! Long-term device identity and the book of paired peers.
//!
//! Each device holds one persistent X25519 [`Identity`] (generated once, stored
//! locally, 0600). Pairing records the *peer's* static public key in a
//! [`DeviceBook`] — a TOML file of trusted devices. Reconnects authenticate
//! against these stored keys (TOFU, like SSH `known_hosts`): the handshake only
//! completes if the peer proves possession of the private key matching a stored
//! public key, so no shared secret is ever typed after the first pairing.

use anyhow::{Context, Result};
use base64::Engine;
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use x25519_dalek::{PublicKey, StaticSecret};

/// A device's long-term X25519 identity. The secret never leaves the device.
pub struct Identity {
    secret: StaticSecret,
}

impl Identity {
    /// Generate a fresh identity from the OS RNG.
    pub fn generate() -> Self {
        Identity {
            secret: StaticSecret::random_from_rng(OsRng),
        }
    }

    /// This identity's public key — the value a peer stores to recognize us.
    pub fn public(&self) -> PublicKey {
        PublicKey::from(&self.secret)
    }

    /// Borrow the static secret to drive a handshake. (Consumers clone it into
    /// the handshake; the secret itself stays owned here.)
    pub fn secret(&self) -> StaticSecret {
        self.secret.clone()
    }

    /// Load the identity from `path`, or generate + save a new one if absent.
    /// This is the normal entry point: `Identity::load_or_create(~/.bob/identity.key)`.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if path.exists() {
            Self::load(path)
        } else {
            let id = Self::generate();
            id.save(path)?;
            Ok(id)
        }
    }

    /// Load a raw 32-byte secret from disk.
    pub fn load(path: &Path) -> Result<Self> {
        let bytes =
            std::fs::read(path).with_context(|| format!("reading identity {}", path.display()))?;
        let arr: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("identity file is not 32 bytes"))?;
        Ok(Identity {
            secret: StaticSecret::from(arr),
        })
    }

    /// Persist the raw secret with owner-only permissions. On unix the file is
    /// created with mode 0600 from the start, so there is never a window where
    /// the key sits at default permissions.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_owner_only(path, &self.secret.to_bytes())
    }
}

#[cfg(unix)]
fn write_owner_only(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_owner_only(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes)?;
    Ok(())
}

/// One trusted peer: a friendly name, its static public key, and when we paired.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Device {
    /// Human label, e.g. "Mirco's iPhone".
    pub name: String,
    /// The peer's X25519 static public key, base64 (standard, no padding).
    pub public_key: String,
    /// ISO-ish timestamp string recorded at pairing (opaque to this crate).
    #[serde(default)]
    pub paired_at: String,
}

impl Device {
    /// Build a device entry from a live public key.
    pub fn new(name: impl Into<String>, public: &PublicKey, paired_at: impl Into<String>) -> Self {
        Device {
            name: name.into(),
            public_key: b64_encode(public.as_bytes()),
            paired_at: paired_at.into(),
        }
    }

    /// Decode the stored key back into an X25519 public key.
    pub fn public(&self) -> Result<PublicKey> {
        let bytes = b64_decode(&self.public_key)?;
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("device key is not 32 bytes"))?;
        Ok(PublicKey::from(arr))
    }
}

/// The set of trusted devices, persisted as `[[device]]` entries in a TOML file
/// (e.g. `~/.bob/devices.toml`).
#[derive(Default, Serialize, Deserialize)]
pub struct DeviceBook {
    #[serde(default, rename = "device")]
    devices: Vec<Device>,
    #[serde(skip)]
    path: PathBuf,
}

impl DeviceBook {
    /// Load the book at `path` (empty if the file doesn't exist yet).
    pub fn load(path: &Path) -> Result<Self> {
        let mut book = if path.exists() {
            let text = std::fs::read_to_string(path)?;
            toml::from_str::<DeviceBook>(&text)
                .with_context(|| format!("parsing {}", path.display()))?
        } else {
            DeviceBook::default()
        };
        book.path = path.to_path_buf();
        Ok(book)
    }

    /// Find a trusted device by the peer static public key presented in a
    /// handshake. `None` means "unknown device" → reject the connection.
    pub fn find_by_public(&self, public: &PublicKey) -> Option<&Device> {
        let target = b64_encode(public.as_bytes());
        self.devices.iter().find(|d| d.public_key == target)
    }

    /// Whether we already trust this public key.
    pub fn is_trusted(&self, public: &PublicKey) -> bool {
        self.find_by_public(public).is_some()
    }

    /// Add (or replace by name) a device, then persist.
    pub fn add(&mut self, device: Device) -> Result<()> {
        self.devices.retain(|d| d.name != device.name);
        self.devices.push(device);
        self.save()
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        std::fs::write(&self.path, text)?;
        Ok(())
    }
}

fn b64_encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD_NO_PAD.encode(bytes)
}

fn b64_decode(s: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(s)
        .context("invalid base64 key")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("bob-secure-test-{}-{}", std::process::id(), name));
        p
    }

    #[test]
    fn identity_round_trips_through_disk() {
        let path = tmp("identity.key");
        let _ = std::fs::remove_file(&path);
        let id = Identity::load_or_create(&path).unwrap();
        let pub1 = id.public();
        // Second load returns the SAME key.
        let id2 = Identity::load_or_create(&path).unwrap();
        assert_eq!(id2.public().as_bytes(), pub1.as_bytes());
        std::fs::remove_file(&path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn identity_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let path = tmp("perm.key");
        let _ = std::fs::remove_file(&path);
        Identity::generate().save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn device_book_add_find_remove() {
        let path = tmp("devices.toml");
        let _ = std::fs::remove_file(&path);
        let peer = Identity::generate();
        let peer_pub = peer.public();

        let mut book = DeviceBook::load(&path).unwrap();
        assert!(!book.is_trusted(&peer_pub));
        book.add(Device::new("Phone", &peer_pub, "2026-07-29"))
            .unwrap();
        assert!(book.is_trusted(&peer_pub));

        // Reload from disk: the trust persists.
        let book2 = DeviceBook::load(&path).unwrap();
        assert_eq!(book2.find_by_public(&peer_pub).unwrap().name, "Phone");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn device_key_round_trips() {
        let id = Identity::generate();
        let d = Device::new("x", &id.public(), "now");
        assert_eq!(d.public().unwrap().as_bytes(), id.public().as_bytes());
    }
}
