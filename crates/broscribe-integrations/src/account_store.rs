//! Durable registry and versioned-secret transactions; caller holds the registry lock.
use super::*;

pub(super) const REGISTRY_FILE: &str = "accounts.json";

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct StoredAccount {
    pub view: MicrosoftAccount,
    #[serde(default)]
    pub reauth_notified: bool,
    #[serde(default)]
    pub readable_targets: Vec<CalendarTarget>,
    #[serde(default)]
    pub shared_enabled: bool,
    #[serde(default)]
    pub groups_enabled: bool,
    #[serde(default)]
    pub authorized_scopes: Vec<String>,
    pub secret_key: Option<String>,
    /// Advanced by every observed route denial: a sync whose reads started before it
    /// never publishes after it, also for a route that was never readable.
    #[serde(default)]
    pub denial_fence: u64,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct PendingSecret {
    pub account_id: AccountId,
    pub key: String,
    pub obsolete_key: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LegacyDiscardIntent {
    pub from_generation: u64,
    pub fenced_generation: u64,
}

/// Sign-in attempts live in the provider's attempt journal (`login.rs`); an `attempts`
/// list that an earlier build wrote here is ignored and disappears with the next save.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct Registry {
    pub version: u32,
    pub accounts: Vec<StoredAccount>,
    pub pending_secrets: Vec<PendingSecret>,
    pub legacy_quarantined: bool,
    pub legacy_checked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_discard: Option<LegacyDiscardIntent>,
}

pub(super) fn save(dir: &Path, registry: &Registry) -> Result<()> {
    write_json_private(&dir.join(REGISTRY_FILE), registry)
}

pub(super) fn validate_secret_key(dir: &Path, account_id: &AccountId, key: &str) -> Result<()> {
    let provider = dir.file_name().and_then(|name| name.to_str()).unwrap_or("");
    let mut parts = key.split('/');
    let valid = provider == "microsoft-graph"
        && parts.next() == Some("integration")
        && parts.next() == Some(provider)
        && parts.next() == Some("accounts")
        && parts.next() == Some(account_id.as_str())
        && parts.next() == Some("tokens")
        && parts.next().is_some_and(|version| {
            version.len() == 32
                && version
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        })
        && parts.next().is_none();
    if valid {
        Ok(())
    } else {
        Err(service_error(
            "invalid_secret_reference",
            "credential recovery reference is outside its account namespace",
        ))
    }
}

pub(super) fn recover(
    dir: &Path,
    registry: &mut Registry,
    secrets: &dyn SecretStore,
) -> Result<()> {
    for pending in &registry.pending_secrets {
        validate_secret_key(dir, &pending.account_id, &pending.key)?;
        if let Some(old) = &pending.obsolete_key {
            validate_secret_key(dir, &pending.account_id, old)?;
        }
    }
    while let Some(pending) = registry.pending_secrets.first().cloned() {
        let committed = registry
            .accounts
            .iter()
            .any(|a| a.secret_key.as_deref() == Some(&pending.key));
        if committed {
            if let Some(old) = pending.obsolete_key
                && !registry
                    .accounts
                    .iter()
                    .any(|a| a.secret_key.as_deref() == Some(&old))
            {
                secrets.delete(&old)?;
            }
        } else {
            secrets.delete(&pending.key)?;
        }
        registry.pending_secrets.remove(0);
        save(dir, registry)?;
    }
    Ok(())
}

/// Caller holds the provider-state lock followed by the registry lock. This distinct
/// journal never accepts credential keys or paths from persisted intent data.
pub(super) fn recover_legacy_discard(
    dir: &Path,
    registry: &mut Registry,
    secrets: &dyn SecretStore,
) -> Result<()> {
    let Some(intent) = registry.legacy_discard else {
        return Ok(());
    };
    let provider = dir.file_name().and_then(|name| name.to_str()).unwrap_or("");
    let id = match provider {
        "microsoft-graph" => IntegrationId::MicrosoftGraph,
        _ => {
            return Err(service_error(
                "invalid_legacy_discard",
                "legacy cleanup requires a Microsoft provider",
            ));
        }
    };
    if intent.from_generation.checked_add(1) != Some(intent.fenced_generation) {
        return Err(service_error(
            "invalid_legacy_discard",
            "invalid legacy discard generation fence",
        ));
    }
    let legacy: AccountState = read_json_opt(&dir.join(STATE_FILE))?.unwrap_or_default();
    if legacy.generation != intent.from_generation
        && (legacy.generation != intent.fenced_generation
            || legacy.status != AccountStatus::SignedOut)
    {
        return Err(IntegrationError::Superseded(id.to_string()));
    }
    if !registry.legacy_quarantined {
        registry.legacy_quarantined = true;
        save(dir, registry)?;
    }
    if legacy.generation == intent.from_generation {
        write_json_private(
            &dir.join(STATE_FILE),
            &AccountState {
                generation: intent.fenced_generation,
                ..AccountState::default()
            },
        )?;
    }
    secrets.delete(&tokens_secret_key(id))?;
    remove_file_if_exists(&dir.join(CALENDAR_FILE))?;
    remove_cached_photos(&dir.join(PHOTOS_DIR))?;
    registry.legacy_quarantined = false;
    registry.legacy_checked = true;
    save(dir, registry)?;
    registry.legacy_discard = None;
    save(dir, registry)
}

pub(super) fn publish(
    dir: &Path,
    registry: &mut Registry,
    secrets: &dyn SecretStore,
    stored: StoredAccount,
    tokens: &Tokens,
) -> Result<()> {
    let key = stored
        .secret_key
        .clone()
        .ok_or_else(|| service_error("account_not_found", "missing credential version"))?;
    let old = registry
        .accounts
        .iter()
        .find(|a| a.view.account == stored.view.account)
        .and_then(|a| a.secret_key.clone());
    validate_secret_key(dir, &stored.view.account.account_id, &key)?;
    if let Some(old) = &old {
        validate_secret_key(dir, &stored.view.account.account_id, old)?;
    }
    registry.pending_secrets.push(PendingSecret {
        account_id: stored.view.account.account_id.clone(),
        key: key.clone(),
        obsolete_key: old,
    });
    save(dir, registry)?;
    let json = serde_json::to_string(tokens)
        .map_err(|_| IntegrationError::Service("cannot encode credential".into()))?;
    secrets.set(&key, &json)?;
    if let Some(current) = registry
        .accounts
        .iter_mut()
        .find(|a| a.view.account == stored.view.account)
    {
        *current = stored;
    } else {
        registry.accounts.push(stored);
    }
    save(dir, registry)?;
    recover(dir, registry, secrets)
}

pub(super) fn account_path(data_dir: &Path, account: &AccountRef) -> PathBuf {
    account_dir(data_dir, integration_id(account.integration))
        .join("accounts")
        .join(account.account_id.as_str())
}

pub(super) fn secret_version(account: &AccountRef) -> String {
    format!(
        "integration/{}/accounts/{}/tokens/{}",
        integration_id(account.integration),
        account.account_id.as_str(),
        random_id()
    )
}

pub(super) fn random_id() -> String {
    format!("{:032x}", rand::random::<u128>())
}

pub(super) fn bump(value: u64) -> Result<u64> {
    value
        .checked_add(1)
        .ok_or_else(|| IntegrationError::Service("account generation exhausted".into()))
}
