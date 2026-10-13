//! API credentials stay in the environment or the macOS Keychain, never in
//! Bund's config, session database, command arguments, or diagnostic output.
#[cfg(target_os = "macos")]
use anyhow::Context;
use anyhow::Result;
use std::io::IsTerminal;

const SERVICE: &str = "com.connordavidson.bund";

trait CredentialStore {
    fn get(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>>;
    fn set(&self, service: &str, account: &str, secret: &[u8]) -> Result<()>;
    fn delete(&self, service: &str, account: &str) -> Result<()>;
}

struct NativeStore;

#[cfg(target_os = "macos")]
impl CredentialStore for NativeStore {
    fn get(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>> {
        use security_framework::passwords::{generic_password, PasswordOptions};
        match generic_password(PasswordOptions::new_generic_password(service, account)) {
            Ok(secret) => Ok(Some(secret)),
            Err(error) if error.code() == security_framework_sys::base::errSecItemNotFound => Ok(None),
            Err(error) => Err(error).context("macOS Keychain could not read Bund's API key; unlock the login Keychain and allow Bund access, or set the provider's API-key environment variable"),
        }
    }

    fn set(&self, service: &str, account: &str, secret: &[u8]) -> Result<()> {
        security_framework::passwords::set_generic_password(service, account, secret)
            .context("macOS Keychain could not save Bund's API key; unlock the login Keychain and allow Bund access")
    }

    fn delete(&self, service: &str, account: &str) -> Result<()> {
        match security_framework::passwords::delete_generic_password(service, account) {
            Ok(()) => Ok(()),
            Err(error) if error.code() == security_framework_sys::base::errSecItemNotFound => Ok(()),
            Err(error) => Err(error).context("macOS Keychain could not remove Bund's API key; unlock the login Keychain and allow Bund access"),
        }
    }
}

#[cfg(not(target_os = "macos"))]
impl CredentialStore for NativeStore {
    fn get(&self, _service: &str, _account: &str) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }

    fn set(&self, _service: &str, account: &str, _secret: &[u8]) -> Result<()> {
        Err(unsupported_keychain(account)?)
    }

    fn delete(&self, _service: &str, account: &str) -> Result<()> {
        Err(unsupported_keychain(account)?)
    }
}

fn environment_name(provider: &str) -> Result<&'static str> {
    match provider {
        "openai" => Ok("OPENAI_API_KEY"),
        "anthropic" => Ok("ANTHROPIC_API_KEY"),
        _ => anyhow::bail!("unsupported provider {provider:?}; choose openai or anthropic"),
    }
}

fn unsupported_keychain(provider: &str) -> Result<anyhow::Error> {
    let name = environment_name(provider)?;
    Ok(anyhow::anyhow!("Keychain credential storage is available only on macOS. Set {name} in your environment on this platform."))
}

fn environment_value(name: &str) -> Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => anyhow::bail!("{name} must contain valid UTF-8"),
    }
}

fn valid_credential(secret: &str) -> bool {
    !secret.is_empty() && !secret.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn resolve(
    provider: &str,
    allow_keychain: bool,
    read_environment: impl FnOnce(&str) -> Result<Option<String>>,
    store: &impl CredentialStore,
) -> Result<Option<(String, &'static str)>> {
    let name = environment_name(provider)?;
    if let Some(secret) = read_environment(name)?.filter(|secret| !secret.is_empty()) {
        anyhow::ensure!(
            valid_credential(&secret),
            "{name} must not contain whitespace or control characters"
        );
        return Ok(Some((secret, "environment")));
    }
    if !allow_keychain {
        return Ok(None);
    }
    let Some(bytes) = store.get(SERVICE, provider)? else {
        return Ok(None);
    };
    let secret = String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("Bund's stored {provider} API key is not valid UTF-8; replace it with `bund auth login --provider {provider}`"))?;
    anyhow::ensure!(valid_credential(&secret), "Bund's stored {provider} API key is empty or contains whitespace; replace it with `bund auth login --provider {provider}`");
    Ok(Some((secret, "macOS Keychain")))
}

/// Resolve only this provider. An explicit nonempty environment key wins;
/// callers can completely disable Keychain reads for isolated automation.
pub fn credential(provider: &str, allow_keychain: bool) -> Result<Option<String>> {
    Ok(resolve(provider, allow_keychain, environment_value, &NativeStore)?.map(|(key, _)| key))
}

/// Report a source label without returning or displaying the credential.
pub fn status(provider: &str) -> Result<&'static str> {
    Ok(resolve(provider, true, environment_value, &NativeStore)?
        .map(|(_, source)| source)
        .unwrap_or("not configured"))
}

fn login_with(
    provider: &str,
    terminal: bool,
    prompt: impl FnOnce(&str) -> Result<String>,
    store: &impl CredentialStore,
) -> Result<()> {
    let name = environment_name(provider)?;
    anyhow::ensure!(terminal, "Keychain login requires a terminal for hidden key entry. Run `bund auth login --provider {provider}` in a terminal, or set {name} for automation.");
    let secret = prompt(name)?;
    anyhow::ensure!(
        valid_credential(&secret),
        "API key must not be empty or contain whitespace or control characters; nothing was saved"
    );
    store.set(SERVICE, provider, secret.as_bytes())
}

#[cfg(target_os = "macos")]
fn hidden_prompt(name: &str) -> Result<String> {
    rpassword::prompt_password(format!("{name} (input hidden): "))
        .context("could not read API key from the terminal; nothing was saved")
}

#[cfg(not(target_os = "macos"))]
fn hidden_prompt(_name: &str) -> Result<String> {
    anyhow::bail!("hidden Keychain login is available only on macOS")
}

/// Prompt directly on the TTY with echo disabled, then create or update only
/// Bund's entry for this provider. Never accept the key as a command argument.
pub fn login(provider: &str) -> Result<()> {
    environment_name(provider)?;
    if !cfg!(target_os = "macos") {
        return Err(unsupported_keychain(provider)?);
    }
    login_with(
        provider,
        std::io::stdin().is_terminal(),
        hidden_prompt,
        &NativeStore,
    )
}

/// Remove only this provider's Bund entry. A missing entry is already logged out;
/// environment variables are deliberately unaffected.
pub fn logout(provider: &str) -> Result<()> {
    logout_with(provider, &NativeStore)
}

fn logout_with(provider: &str, store: &impl CredentialStore) -> Result<()> {
    environment_name(provider)?;
    store.delete(SERVICE, provider)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    #[derive(Default)]
    struct FakeStore {
        values: RefCell<HashMap<(String, String), Vec<u8>>>,
        calls: RefCell<Vec<String>>,
        fail: bool,
    }
    impl CredentialStore for FakeStore {
        fn get(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>> {
            self.calls
                .borrow_mut()
                .push(format!("get:{service}:{account}"));
            anyhow::ensure!(!self.fail, "fixture Keychain is locked");
            Ok(self
                .values
                .borrow()
                .get(&(service.into(), account.into()))
                .cloned())
        }
        fn set(&self, service: &str, account: &str, secret: &[u8]) -> Result<()> {
            self.calls
                .borrow_mut()
                .push(format!("set:{service}:{account}"));
            anyhow::ensure!(!self.fail, "fixture Keychain is locked");
            self.values
                .borrow_mut()
                .insert((service.into(), account.into()), secret.to_vec());
            Ok(())
        }
        fn delete(&self, service: &str, account: &str) -> Result<()> {
            self.calls
                .borrow_mut()
                .push(format!("delete:{service}:{account}"));
            anyhow::ensure!(!self.fail, "fixture Keychain is locked");
            self.values
                .borrow_mut()
                .remove(&(service.into(), account.into()));
            Ok(())
        }
    }

    #[test]
    fn environment_wins_without_any_keychain_read() {
        let store = FakeStore {
            fail: true,
            ..FakeStore::default()
        };
        let value = resolve(
            "openai",
            true,
            |name| {
                assert_eq!(name, "OPENAI_API_KEY");
                Ok(Some("fixture-env-key".into()))
            },
            &store,
        )
        .unwrap()
        .unwrap();
        assert_eq!(value, ("fixture-env-key".into(), "environment"));
        assert!(store.calls.borrow().is_empty());
    }

    #[test]
    fn empty_environment_falls_back_but_disabled_keychain_never_reads() {
        let store = FakeStore::default();
        store
            .set(SERVICE, "anthropic", b"fixture-stored-key")
            .unwrap();
        store.calls.borrow_mut().clear();
        let value = resolve("anthropic", true, |_| Ok(Some(String::new())), &store)
            .unwrap()
            .unwrap();
        assert_eq!(value, ("fixture-stored-key".into(), "macOS Keychain"));
        assert_eq!(*store.calls.borrow(), [format!("get:{SERVICE}:anthropic")]);
        store.calls.borrow_mut().clear();
        assert!(resolve("anthropic", false, |_| Ok(None), &store)
            .unwrap()
            .is_none());
        assert!(store.calls.borrow().is_empty());
    }

    #[test]
    fn missing_is_distinct_from_denied_or_corrupt_storage() {
        let store = FakeStore::default();
        assert!(resolve("openai", true, |_| Ok(None), &store)
            .unwrap()
            .is_none());
        let denied = FakeStore {
            fail: true,
            ..FakeStore::default()
        };
        assert!(resolve("openai", true, |_| Ok(None), &denied)
            .unwrap_err()
            .to_string()
            .contains("locked"));
        store.set(SERVICE, "openai", &[0xff]).unwrap();
        assert!(resolve("openai", true, |_| Ok(None), &store).is_err());
        store.set(SERVICE, "openai", b" ").unwrap();
        assert!(resolve("openai", true, |_| Ok(None), &store).is_err());
    }

    #[test]
    fn hidden_login_validates_without_persisting_or_printing_invalid_values() {
        let store = FakeStore::default();
        assert!(login_with(
            "openai",
            false,
            |_| panic!("must not prompt without TTY"),
            &store
        )
        .is_err());
        for secret in ["", " ", "secret with spaces", "secret\nline"] {
            let error = login_with("openai", true, |_| Ok(secret.into()), &store)
                .unwrap_err()
                .to_string();
            if secret.len() > 2 {
                assert!(!error.contains(secret));
            }
        }
        assert!(store.calls.borrow().is_empty());
        login_with(
            "openai",
            true,
            |name| {
                assert_eq!(name, "OPENAI_API_KEY");
                Ok("fixture-new-key".into())
            },
            &store,
        )
        .unwrap();
        assert_eq!(*store.calls.borrow(), [format!("set:{SERVICE}:openai")]);
        assert!(login_with(
            "openai",
            true,
            |_| Ok("fixture-key".into()),
            &FakeStore {
                fail: true,
                ..FakeStore::default()
            }
        )
        .is_err());
    }

    #[test]
    fn logout_scope_is_only_bund_and_selected_provider_and_missing_is_idempotent() {
        let store = FakeStore::default();
        store.set(SERVICE, "openai", b"fixture-one").unwrap();
        store.set(SERVICE, "anthropic", b"fixture-two").unwrap();
        store.set("other.app", "openai", b"fixture-other").unwrap();
        logout_with("openai", &store).unwrap();
        logout_with("openai", &store).unwrap();
        assert!(store.get(SERVICE, "openai").unwrap().is_none());
        assert!(store.get(SERVICE, "anthropic").unwrap().is_some());
        assert!(store.get("other.app", "openai").unwrap().is_some());
        assert!(logout_with(
            "openai",
            &FakeStore {
                fail: true,
                ..FakeStore::default()
            }
        )
        .is_err());
    }

    #[test]
    fn invalid_provider_or_environment_value_never_reaches_keychain() {
        let store = FakeStore::default();
        assert!(resolve(
            "unknown",
            true,
            |_| panic!("invalid provider must not read environment"),
            &store
        )
        .is_err());
        let error = resolve(
            "openai",
            true,
            |_| Ok(Some("fixture secret".into())),
            &store,
        )
        .unwrap_err()
        .to_string();
        assert!(!error.contains("fixture secret"));
        assert!(store.calls.borrow().is_empty());
    }
}
