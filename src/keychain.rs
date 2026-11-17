//! Secrets in the OS keychain: the macOS Keychain and the Windows Credential Manager.
//!
//! Behind [`SecretStore`], so the code that keeps a credential signed in runs the same
//! against [`MemoryKeychain`] in tests, and on Linux, where this gateway uses no
//! keychain at all ([`OsKeychain`] reports it as unavailable there).
//!
//! One generic password per entry, under the service [`SERVICE`]. The secret is text
//! and is never logged: errors carry the platform's status code, not the value.

use anyhow::Result;
use std::collections::HashMap;
use std::sync::Mutex;

/// The service (macOS) or target prefix (Windows) every entry is filed under.
pub const SERVICE: &str = "open-live-ingest";

/// Somewhere a secret can be kept between runs, by name.
pub trait SecretStore: Send + Sync {
    /// The secret stored as `account`, or None when there is none.
    fn get(&self, account: &str) -> Result<Option<String>>;
    /// Stores `secret` as `account`, replacing any earlier one.
    fn set(&self, account: &str, secret: &str) -> Result<()>;
    /// Removes `account`. Removing one that is not there is not an error.
    fn delete(&self, account: &str) -> Result<()>;
}

/// A store in memory, gone with the process. For tests, and for an app's own tests.
#[derive(Debug, Default)]
pub struct MemoryKeychain(Mutex<HashMap<String, String>>);

impl MemoryKeychain {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SecretStore for MemoryKeychain {
    fn get(&self, account: &str) -> Result<Option<String>> {
        Ok(self.0.lock().unwrap().get(account).cloned())
    }

    fn set(&self, account: &str, secret: &str) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .insert(account.to_string(), secret.to_string());
        Ok(())
    }

    fn delete(&self, account: &str) -> Result<()> {
        self.0.lock().unwrap().remove(account);
        Ok(())
    }
}

/// The current user's keychain: the login keychain on macOS, the Credential Manager
/// on Windows. Elsewhere every call fails with a message saying so.
#[derive(Debug, Clone)]
pub struct OsKeychain {
    service: String,
}

impl Default for OsKeychain {
    fn default() -> Self {
        Self::with_service(SERVICE)
    }
}

impl OsKeychain {
    /// Entries under another service name than [`SERVICE`]; tests use their own.
    pub fn with_service(service: &str) -> Self {
        Self {
            service: service.to_string(),
        }
    }

    /// Whether this platform has a keychain this type can use.
    pub fn is_available() -> bool {
        cfg!(any(target_os = "macos", windows))
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use anyhow::{anyhow, Result};
    use security_framework::passwords::{
        delete_generic_password, generic_password, set_generic_password, PasswordOptions,
    };

    /// `errSecItemNotFound`.
    const NOT_FOUND: i32 = -25300;

    fn failed(what: &str, err: security_framework::base::Error) -> anyhow::Error {
        anyhow!("{what} the macOS Keychain failed (OSStatus {})", err.code())
    }

    pub fn get(service: &str, account: &str) -> Result<Option<String>> {
        match generic_password(PasswordOptions::new_generic_password(service, account)) {
            Ok(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| anyhow!("the macOS Keychain entry {service}/{account} is not text")),
            Err(err) if err.code() == NOT_FOUND => Ok(None),
            Err(err) => Err(failed("reading from", err)),
        }
    }

    pub fn set(service: &str, account: &str, secret: &str) -> Result<()> {
        set_generic_password(service, account, secret.as_bytes())
            .map_err(|err| failed("writing to", err))
    }

    pub fn delete(service: &str, account: &str) -> Result<()> {
        match delete_generic_password(service, account) {
            Ok(()) => Ok(()),
            Err(err) if err.code() == NOT_FOUND => Ok(()),
            Err(err) => Err(failed("deleting from", err)),
        }
    }
}

#[cfg(windows)]
mod platform {
    //! A generic credential per entry, named `<service>/<account>`, persisted for this
    //! user on this machine (not roaming).
    use anyhow::{anyhow, bail, Result};
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_NOT_FOUND};
    use windows_sys::Win32::Security::Credentials::{
        CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_MAX_CREDENTIAL_BLOB_SIZE,
        CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn target(service: &str, account: &str) -> Vec<u16> {
        wide(&format!("{service}/{account}"))
    }

    fn failed(what: &str, code: u32) -> anyhow::Error {
        anyhow!("{what} the Windows Credential Manager failed (error {code})")
    }

    pub fn get(service: &str, account: &str) -> Result<Option<String>> {
        let name = target(service, account);
        let mut found: *mut CREDENTIALW = std::ptr::null_mut();
        // SAFETY: `name` is NUL-terminated and outlives the call; on success `found`
        // points at a credential the system allocated, freed below with CredFree.
        let ok = unsafe { CredReadW(name.as_ptr(), CRED_TYPE_GENERIC, 0, &mut found) };
        if ok == 0 {
            // SAFETY: no other call in between that could reset the thread's error.
            let code = unsafe { GetLastError() };
            if code == ERROR_NOT_FOUND {
                return Ok(None);
            }
            return Err(failed("reading from", code));
        }
        // SAFETY: CredReadW succeeded, so `found` is valid and its blob holds
        // `CredentialBlobSize` bytes (or is null when the size is zero).
        let bytes = unsafe {
            let cred = &*found;
            let bytes = if cred.CredentialBlob.is_null() || cred.CredentialBlobSize == 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(cred.CredentialBlob, cred.CredentialBlobSize as usize)
                    .to_vec()
            };
            CredFree(found as *const _);
            bytes
        };
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| anyhow!("the Credential Manager entry {service}/{account} is not text"))
    }

    pub fn set(service: &str, account: &str, secret: &str) -> Result<()> {
        if secret.len() > CRED_MAX_CREDENTIAL_BLOB_SIZE as usize {
            bail!(
                "the secret is {} bytes; the Windows Credential Manager holds at most {CRED_MAX_CREDENTIAL_BLOB_SIZE}",
                secret.len()
            );
        }
        let mut name = target(service, account);
        let mut user = wide(account);
        let mut blob = secret.as_bytes().to_vec();
        let cred = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: name.as_mut_ptr(),
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            UserName: user.as_mut_ptr(),
            ..Default::default()
        };
        // SAFETY: every pointer in `cred` points into a buffer that lives until after
        // the call; CredWriteW copies what it keeps.
        let ok = unsafe { CredWriteW(&cred, 0) };
        if ok == 0 {
            // SAFETY: as in `get`.
            return Err(failed("writing to", unsafe { GetLastError() }));
        }
        Ok(())
    }

    pub fn delete(service: &str, account: &str) -> Result<()> {
        let name = target(service, account);
        // SAFETY: `name` is NUL-terminated and outlives the call.
        let ok = unsafe { CredDeleteW(name.as_ptr(), CRED_TYPE_GENERIC, 0) };
        if ok == 0 {
            // SAFETY: as in `get`.
            let code = unsafe { GetLastError() };
            if code != ERROR_NOT_FOUND {
                return Err(failed("deleting from", code));
            }
        }
        Ok(())
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use anyhow::{bail, Result};

    fn unavailable<T>() -> Result<T> {
        bail!("this platform has no keychain the gateway can use; set open_live.api_key, OSC_ACCESS_TOKEN or log in with the OSC CLI instead")
    }

    pub fn get(_service: &str, _account: &str) -> Result<Option<String>> {
        unavailable()
    }

    pub fn set(_service: &str, _account: &str, _secret: &str) -> Result<()> {
        unavailable()
    }

    pub fn delete(_service: &str, _account: &str) -> Result<()> {
        unavailable()
    }
}

impl SecretStore for OsKeychain {
    fn get(&self, account: &str) -> Result<Option<String>> {
        platform::get(&self.service, account)
    }

    fn set(&self, account: &str, secret: &str) -> Result<()> {
        platform::set(&self.service, account, secret)
    }

    fn delete(&self, account: &str) -> Result<()> {
        platform::delete(&self.service, account)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_memory_store_keeps_replaces_and_forgets() {
        let store = MemoryKeychain::new();
        assert_eq!(store.get("a").unwrap(), None);
        store.set("a", "one").unwrap();
        store.set("a", "two").unwrap();
        assert_eq!(store.get("a").unwrap().as_deref(), Some("two"));
        store.delete("a").unwrap();
        store.delete("a").unwrap();
        assert_eq!(store.get("a").unwrap(), None);
    }

    /// Runs against the real Credential Manager on the Windows CI leg, under a service
    /// name of its own so it never touches a real entry.
    #[cfg(windows)]
    #[test]
    fn the_windows_credential_manager_round_trips_an_entry() {
        let store =
            OsKeychain::with_service(&format!("open-live-ingest-test-{}", std::process::id()));
        store.delete("osc").unwrap();
        assert_eq!(store.get("osc").unwrap(), None);
        store.set("osc", r#"{"kind":"token","token":"t"}"#).unwrap();
        store.set("osc", "replaced").unwrap();
        assert_eq!(store.get("osc").unwrap().as_deref(), Some("replaced"));
        store.delete("osc").unwrap();
        assert_eq!(store.get("osc").unwrap(), None);
        assert!(store.set("osc", &"x".repeat(4000)).is_err());
    }

    /// Runs against the real login keychain on the macOS CI leg, under a service name
    /// of its own so it never touches a real entry. Skipped outside CI, where writing
    /// to the login keychain could put up a prompt.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_macos_keychain_round_trips_an_entry() {
        if std::env::var_os("CI").is_none() {
            return;
        }
        let store =
            OsKeychain::with_service(&format!("open-live-ingest-test-{}", std::process::id()));
        store.delete("osc").unwrap();
        assert_eq!(
            store.get("osc").unwrap(),
            None,
            "errSecItemNotFound is None"
        );
        store.set("osc", r#"{"kind":"token","token":"t"}"#).unwrap();
        store.set("osc", "replaced").unwrap();
        assert_eq!(store.get("osc").unwrap().as_deref(), Some("replaced"));
        store.delete("osc").unwrap();
        assert_eq!(store.get("osc").unwrap(), None);
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    #[test]
    fn elsewhere_the_os_keychain_says_it_is_unavailable() {
        assert!(!OsKeychain::is_available());
        let err = OsKeychain::default().get("osc").unwrap_err();
        assert!(format!("{err:#}").contains("no keychain"), "{err:#}");
    }
}
