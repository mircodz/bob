use super::{credential_from_tokens, now, AuthStore, Credential};
use crate::core::file_update::{atomic_replace, sibling, update_path, FileLock};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::future::Future;
use std::path::Path;

fn read_document(path: &Path) -> anyhow::Result<Value> {
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json!({"providers": {}}))
        }
        Err(error) => return Err(error.into()),
    };
    let document: Value = serde_json::from_slice(&raw).map_err(|_| {
        anyhow::anyhow!(
            "invalid credential store at {}; original file preserved",
            path.display()
        )
    })?;
    serde_json::from_value::<AuthStore>(document.clone()).map_err(|_| {
        anyhow::anyhow!(
            "invalid credential store at {}; original file preserved",
            path.display()
        )
    })?;
    anyhow::ensure!(document.is_object(), "credential store must be an object");
    if let Some(revisions) = document.get("_revisions") {
        anyhow::ensure!(
            revisions
                .as_object()
                .is_some_and(|values| values.values().all(Value::is_string)),
            "invalid credential revisions; original file preserved"
        );
    }
    Ok(document)
}

pub(super) fn load_at(path: &Path) -> anyhow::Result<AuthStore> {
    Ok(serde_json::from_value(read_document(path)?)?)
}

fn credential(document: &Value, provider: &str) -> anyhow::Result<Option<Credential>> {
    document
        .get("providers")
        .and_then(|providers| providers.get(provider))
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(Into::into)
}

fn publish(
    path: &Path,
    document: &mut Value,
    provider: &str,
    value: Option<Credential>,
) -> anyhow::Result<()> {
    let mut random = [0u8; 16];
    getrandom::getrandom(&mut random)
        .map_err(|error| anyhow::anyhow!("credential revision generation failed: {error}"))?;
    let revision: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let root = document.as_object_mut().unwrap();
    let providers = root
        .entry("providers")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .unwrap();
    if let Some(value) = value {
        providers.insert(provider.into(), serde_json::to_value(value)?);
    } else {
        providers.remove(provider);
    }
    root.entry("_revisions")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .unwrap()
        .insert(provider.into(), Value::String(revision));
    atomic_replace(path, &serde_json::to_vec_pretty(document)?, true)?;
    Ok(())
}

pub(super) async fn update_at<F>(path: &Path, provider: &str, change: F) -> anyhow::Result<bool>
where
    F: FnOnce(Option<Credential>) -> anyhow::Result<Option<Credential>>,
{
    let path = update_path(path)?;
    let _lock = FileLock::acquire_async(&sibling(&path, ".lock")).await?;
    let mut document = read_document(&path)?;
    let previous = credential(&document, provider)?;
    let existed = previous.is_some();
    let next = change(previous)?;
    if existed || next.is_some() {
        publish(&path, &mut document, provider, next)?;
    }
    Ok(existed)
}

pub(super) async fn store_tokens_at(
    path: &Path,
    provider: &str,
    tokens: &Value,
    extra_keys: &[&str],
) -> anyhow::Result<()> {
    let access = tokens["access_token"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("no access_token"))?
        .to_string();
    update_at(path, provider, |previous| {
        Ok(Some(credential_from_tokens(
            tokens,
            previous.as_ref(),
            extra_keys,
            now(),
            access,
        )))
    })
    .await?;
    Ok(())
}

pub(super) async fn access_token_with<F, Fut>(
    path: &Path,
    provider: &str,
    extra_keys: &[&str],
    missing: &str,
    refresh: F,
) -> anyhow::Result<String>
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = anyhow::Result<Value>>,
{
    let path = update_path(path)?;
    let key: String = Sha256::digest(provider.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let _refresh_lock =
        FileLock::acquire_async(&sibling(&path, &format!(".refresh-{key}.lock"))).await?;
    let snapshot = read_document(&path)?;
    let previous = credential(&snapshot, provider)?.ok_or_else(|| anyhow::anyhow!("{missing}"))?;
    let expires_at = previous
        .extra
        .get("expires_at")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    if !previous.token.is_empty() && expires_at > now().saturating_add(60) {
        return Ok(previous.token);
    }
    let refresh_token = previous
        .refresh_token
        .clone()
        .ok_or_else(|| anyhow::anyhow!("session expired and no refresh token; log in again"))?;
    let tokens = refresh(refresh_token).await?;
    let access = tokens["access_token"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("no access_token in refresh response"))?
        .to_string();
    let _document_lock = FileLock::acquire_async(&sibling(&path, ".lock")).await?;
    let mut current = read_document(&path)?;
    // The revision also detects logout/login ABA with identical credential values.
    if current["_revisions"][provider] != snapshot["_revisions"][provider]
        || current["providers"][provider] != snapshot["providers"][provider]
    {
        return credential(&current, provider)?
            .filter(|value| !value.token.is_empty())
            .map(|value| value.token)
            .ok_or_else(|| anyhow::anyhow!("credentials changed during refresh; log in again"));
    }
    let next = credential_from_tokens(&tokens, Some(&previous), extra_keys, now(), access.clone());
    publish(&path, &mut current, provider, Some(next))?;
    Ok(access)
}

#[cfg(test)]
#[path = "storage_tests.rs"]
mod tests;
