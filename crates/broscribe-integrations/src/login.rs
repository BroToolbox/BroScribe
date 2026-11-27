//! Interactive OAuth sign-ins (Google and registered Microsoft accounts) that another
//! process can cancel, and whose terminal result stays readable after the caller navigates
//! away.
//!
//! The caller names every sign-in with a [`LoginAttemptId`]. `login-attempts.json` sits
//! beside the provider's state and changes only under the provider state lock, which every
//! credential commit also holds, so a cancel and a commit are ordered: a cancelled attempt
//! never publishes credentials, never replaces a working account and never changes its
//! generation, selection or cache. Cancelling an ID nobody registered yet leaves a
//! tombstone, so a sign-in that starts after its own cancellation is refused.
//!
//! A process that disappears is never presumed cancelled: its entry stays `running` until
//! [`ATTEMPT_LIFETIME`] ends, then reads `failed` with `login_abandoned`, and its late
//! commit is refused. A commit whose process stopped half-way is resolved against the
//! credential version actually committed. Terminal results stay readable for
//! [`OUTCOME_RETENTION`]; the journal never keeps more than [`MAX_ENTRIES`] entries and
//! admits at most [`MAX_RUNNING`] concurrent sign-ins per provider.

use std::fmt;
use std::str::FromStr;

use super::*;

const ATTEMPTS_FILE: &str = "login-attempts.json";
/// Longer than the browser callback wait plus the token exchange and identity reads.
pub(crate) const ATTEMPT_LIFETIME: chrono::Duration = chrono::Duration::minutes(15);
/// How long a finished sign-in's result stays readable.
pub(crate) const OUTCOME_RETENTION: chrono::Duration = chrono::Duration::hours(1);
pub(crate) const MAX_ENTRIES: usize = 64;
pub(crate) const MAX_RUNNING: usize = 8;

/// Caller-chosen name of one interactive sign-in: 1–64 ASCII letters, digits or hyphens.
/// Single use: a finished or cancelled ID never starts another sign-in.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct LoginAttemptId(String);

impl LoginAttemptId {
    /// A fresh random ID (32 lowercase hex digits).
    pub fn generate() -> Self {
        Self(format!("{:032x}", rand::random::<u128>()))
    }

    pub fn parse(value: &str) -> Result<Self> {
        if (1..=64).contains(&value.len())
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            Ok(Self(value.to_owned()))
        } else {
            Err(IntegrationError::Core(broscribe_core::Error::InvalidInput(
                "a sign-in attempt ID has 1 to 64 ASCII letters, digits or hyphens".into(),
            )))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for LoginAttemptId {
    type Err = IntegrationError;
    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

impl TryFrom<String> for LoginAttemptId {
    type Error = IntegrationError;
    fn try_from(value: String) -> Result<Self> {
        Self::parse(&value)
    }
}

impl From<LoginAttemptId> for String {
    fn from(id: LoginAttemptId) -> Self {
        id.0
    }
}

impl fmt::Display for LoginAttemptId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where one sign-in stands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum LoginAttemptState {
    /// Waiting for the browser, exchanging the code, verifying or saving the account.
    Running,
    /// Cancelled before it committed: nothing from it was or will ever be saved.
    Cancelled,
    /// Its credentials were committed.
    Succeeded,
    /// It ended without committing anything; a sanitized code and message.
    Failed { code: String, message: String },
    /// Its process stopped while saving and its credential version is not the committed
    /// one now; read the account status for what is current.
    Interrupted,
    /// Never registered here, or its result is older than the retention.
    NotFound,
}

/// The answer of [`Integrations::cancel_login`] and [`Integrations::login_attempt`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginAttemptStatus {
    pub integration: IntegrationId,
    pub attempt: LoginAttemptId,
    #[serde(flatten)]
    pub state: LoginAttemptState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
enum Phase {
    Running,
    /// The commit under the provider lock is publishing this credential version.
    Committing {
        secret_key: String,
    },
    Finished {
        outcome: LoginAttemptState,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    id: LoginAttemptId,
    started_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    phase: Phase,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Journal {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    attempts: Vec<Entry>,
}

impl Journal {
    fn read(dir: &Path) -> Result<Self> {
        let journal: Self = read_json_opt(&dir.join(ATTEMPTS_FILE))?.unwrap_or_default();
        if journal.version > 1 {
            return Err(service_error(
                "login_attempts_version",
                "the sign-in attempt journal version is unsupported",
            ));
        }
        Ok(journal)
    }

    fn entry(&self, id: &LoginAttemptId) -> Option<&Entry> {
        self.attempts.iter().find(|e| &e.id == id)
    }

    /// `Ok` only while `id` may still commit.
    fn require_running(&self, id: &LoginAttemptId, now: DateTime<Utc>) -> Result<()> {
        match self.entry(id) {
            Some(e)
                if e.phase == Phase::Running && !outlived(e.started_at, now, ATTEMPT_LIFETIME) =>
            {
                Ok(())
            }
            Some(Entry {
                phase:
                    Phase::Finished {
                        outcome: LoginAttemptState::Cancelled,
                    },
                ..
            }) => Err(cancelled()),
            _ => Err(service_error(
                "login_expired",
                "this sign-in is no longer current and saved nothing; start a new sign-in",
            )),
        }
    }
}

/// `since` lies `limit` or more away from `now`, either way: a clock moved back never
/// keeps an attempt alive.
fn outlived(since: DateTime<Utc>, now: DateTime<Utc>, limit: chrono::Duration) -> bool {
    (now - since).abs() >= limit
}

/// What a cancelled sign-in returns to its own caller.
fn cancelled() -> IntegrationError {
    service_error(
        "login_cancelled",
        "sign-in was cancelled; the existing account, its calendars and its data were kept",
    )
}

/// The journal of one provider, loaded and saved while the caller holds its state lock.
struct Attempts {
    dir: PathBuf,
    journal: Journal,
    written: Journal,
}

impl Attempts {
    /// Resolves commits that a stopped process left half-done against `committed`, retires
    /// attempts that outlived [`ATTEMPT_LIFETIME`] and bounds the journal.
    fn load(
        dir: &Path,
        now: DateTime<Utc>,
        committed: impl Fn(&str) -> Result<bool>,
    ) -> Result<Self> {
        let written = Journal::read(dir)?;
        let mut journal = written.clone();
        journal.version = 1;
        for entry in &mut journal.attempts {
            let outcome = match &entry.phase {
                Phase::Committing { secret_key } => {
                    if committed(secret_key)? {
                        LoginAttemptState::Succeeded
                    } else {
                        LoginAttemptState::Interrupted
                    }
                }
                Phase::Running if outlived(entry.started_at, now, ATTEMPT_LIFETIME) => {
                    LoginAttemptState::Failed {
                        code: "login_abandoned".into(),
                        message: "the sign-in stopped without finishing and saved nothing".into(),
                    }
                }
                _ => continue,
            };
            entry.phase = Phase::Finished { outcome };
            entry.updated_at = now;
        }
        journal.attempts.retain(|e| {
            !matches!(e.phase, Phase::Finished { .. })
                || !outlived(e.updated_at, now, OUTCOME_RETENTION)
        });
        let mut attempts = Self {
            dir: dir.to_owned(),
            journal,
            written,
        };
        attempts.bound();
        Ok(attempts)
    }

    /// Drops the oldest finished results beyond [`MAX_ENTRIES`]; running ones stay.
    fn bound(&mut self) {
        while self.journal.attempts.len() > MAX_ENTRIES {
            let Some(oldest) = self
                .journal
                .attempts
                .iter()
                .enumerate()
                .filter(|(_, e)| matches!(e.phase, Phase::Finished { .. }))
                .min_by_key(|(_, e)| e.updated_at)
                .map(|(index, _)| index)
            else {
                break;
            };
            self.journal.attempts.remove(oldest);
        }
    }

    fn save(&mut self) -> Result<()> {
        if self.journal != self.written {
            write_json_private(&self.dir.join(ATTEMPTS_FILE), &self.journal)?;
            self.written = self.journal.clone();
        }
        Ok(())
    }

    fn entry_mut(&mut self, id: &LoginAttemptId) -> Option<&mut Entry> {
        self.journal.attempts.iter_mut().find(|e| &e.id == id)
    }

    fn state(&self, id: &LoginAttemptId) -> LoginAttemptState {
        match self.journal.entry(id).map(|e| &e.phase) {
            None => LoginAttemptState::NotFound,
            Some(Phase::Running | Phase::Committing { .. }) => LoginAttemptState::Running,
            Some(Phase::Finished { outcome }) => outcome.clone(),
        }
    }

    fn begin(&mut self, id: &LoginAttemptId, now: DateTime<Utc>) -> Result<()> {
        if let Some(entry) = self.journal.entry(id) {
            return Err(match entry.phase {
                Phase::Finished {
                    outcome: LoginAttemptState::Cancelled,
                } => cancelled(),
                _ => service_error(
                    "login_attempt_used",
                    "this sign-in attempt ID was already used; start a new sign-in with a new ID",
                ),
            });
        }
        let running = self
            .journal
            .attempts
            .iter()
            .filter(|e| !matches!(e.phase, Phase::Finished { .. }))
            .count();
        if running >= MAX_RUNNING {
            return Err(service_error(
                "busy",
                "too many sign-ins are in progress; finish or cancel one first",
            ));
        }
        self.journal.attempts.push(Entry {
            id: id.clone(),
            started_at: now,
            updated_at: now,
            phase: Phase::Running,
        });
        self.bound();
        Ok(())
    }

    /// Ends `id` as cancelled unless it already finished; returns where it stands.
    fn cancel(&mut self, id: &LoginAttemptId, now: DateTime<Utc>) -> LoginAttemptState {
        let tombstone = Phase::Finished {
            outcome: LoginAttemptState::Cancelled,
        };
        match self.entry_mut(id) {
            None => {
                self.journal.attempts.push(Entry {
                    id: id.clone(),
                    started_at: now,
                    updated_at: now,
                    phase: tombstone,
                });
                self.bound();
            }
            Some(entry) if entry.phase == Phase::Running => {
                entry.phase = tombstone;
                entry.updated_at = now;
            }
            Some(_) => {}
        }
        self.state(id)
    }

    /// Marks `id` committing `secret_key` once it is the running attempt.
    fn start_commit(
        &mut self,
        id: &LoginAttemptId,
        secret_key: &str,
        now: DateTime<Utc>,
    ) -> Result<()> {
        self.journal.require_running(id, now)?;
        if let Some(entry) = self.entry_mut(id) {
            entry.phase = Phase::Committing {
                secret_key: secret_key.to_owned(),
            };
            entry.updated_at = now;
        }
        self.save()
    }

    /// Records a terminal outcome of an attempt that has none yet.
    fn finish(&mut self, id: &LoginAttemptId, outcome: LoginAttemptState, now: DateTime<Utc>) {
        if let Some(entry) = self.entry_mut(id)
            && !matches!(entry.phase, Phase::Finished { .. })
        {
            entry.phase = Phase::Finished { outcome };
            entry.updated_at = now;
        }
    }
}

/// Google and registered Microsoft accounts are the only interactive OAuth sign-ins.
fn interactive(id: IntegrationId) -> Result<()> {
    if matches!(id, IntegrationId::Google | IntegrationId::MicrosoftGraph) {
        Ok(())
    } else {
        Err(service_error(
            "login_unsupported",
            "only Google and Microsoft 365 sign-ins can be cancelled",
        ))
    }
}

/// A failure as a terminal attempt result.
fn failure(error: &IntegrationError) -> LoginAttemptState {
    let public = error.public();
    LoginAttemptState::Failed {
        code: public.code,
        message: public.message,
    }
}

impl Integrations {
    /// Cancels the sign-in `attempt` of `id` from any process. `Cancelled` guarantees that
    /// nothing from it is ever committed, including for an attempt not registered yet;
    /// an attempt that already finished keeps and returns its terminal result. The
    /// running sign-in stops waiting for the browser and closes its listener promptly.
    /// A sign-in whose process ended is not presumed cancelled.
    pub fn cancel_login(
        &self,
        id: IntegrationId,
        attempt: &LoginAttemptId,
    ) -> Result<LoginAttemptStatus> {
        let state = self.attempt_tx(id, |attempts, now| Ok(attempts.cancel(attempt, now)))?;
        Ok(LoginAttemptStatus {
            integration: id,
            attempt: attempt.clone(),
            state,
        })
    }

    /// Where the sign-in `attempt` of `id` stands, also after the caller navigated away.
    pub fn login_attempt(
        &self,
        id: IntegrationId,
        attempt: &LoginAttemptId,
    ) -> Result<LoginAttemptStatus> {
        let state = self.attempt_tx(id, |attempts, _| Ok(attempts.state(attempt)))?;
        Ok(LoginAttemptStatus {
            integration: id,
            attempt: attempt.clone(),
            state,
        })
    }

    /// One journal transaction under the provider's thread and file locks.
    fn attempt_tx<T>(
        &self,
        id: IntegrationId,
        f: impl FnOnce(&mut Attempts, DateTime<Utc>) -> Result<T>,
    ) -> Result<T> {
        interactive(id)?;
        let _mutex = self.coordinators[&id]
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = account_dir(&self.data_dir, id);
        let _file = FileLock::acquire(&dir.join(STATE_LOCK), LOCK_WAIT)?;
        let mut attempts = self.attempts_in(id, &dir)?;
        let value = f(&mut attempts, Utc::now())?;
        attempts.save()?;
        Ok(value)
    }

    /// The journal of `id`; the caller holds the provider state lock.
    fn attempts_in(&self, id: IntegrationId, dir: &Path) -> Result<Attempts> {
        Attempts::load(dir, Utc::now(), |key| self.secret_committed(id, dir, key))
    }

    /// `key` is the committed credential version on disk now.
    fn secret_committed(&self, id: IntegrationId, dir: &Path, key: &str) -> Result<bool> {
        if id == IntegrationId::Google {
            let state: Option<AccountState> = read_json_opt(&dir.join(STATE_FILE))?;
            Ok(state.and_then(|s| s.secret_key).as_deref() == Some(key))
        } else {
            accounts::microsoft_secret_committed(dir, key)
        }
    }

    /// Runs one interactive sign-in under `attempt`: registered before anything else
    /// happens, and its terminal result recorded however it ends.
    pub(super) fn run_attempt<T>(
        &self,
        id: IntegrationId,
        attempt: &LoginAttemptId,
        sign_in: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        self.attempt_tx(id, |attempts, now| attempts.begin(attempt, now))?;
        let result = sign_in();
        let outcome = match &result {
            Ok(_) => LoginAttemptState::Succeeded,
            Err(error) => failure(error),
        };
        if let Err(error) = self.attempt_tx(id, |attempts, now| {
            attempts.finish(attempt, outcome, now);
            Ok(())
        }) {
            // The entry stays running and is retired as abandoned after its lifetime.
            tracing::warn!(integration = %id, code = error.code(), "sign-in result not recorded");
        }
        result
    }

    /// `Ok` while `attempt` may still commit; the caller holds the provider state lock.
    pub(super) fn require_attempt_in(
        &self,
        id: IntegrationId,
        dir: &Path,
        attempt: &LoginAttemptId,
    ) -> Result<()> {
        self.attempts_in(id, dir)?
            .journal
            .require_running(attempt, Utc::now())
    }

    /// Lock-free check between the stages of a sign-in: `Ok` while `attempt` may still
    /// commit. The commit repeats it under the lock.
    pub(super) fn ensure_attempt(&self, id: IntegrationId, attempt: &LoginAttemptId) -> Result<()> {
        Journal::read(&account_dir(&self.data_dir, id))?.require_running(attempt, Utc::now())
    }

    /// Commits a verified sign-in. The caller holds the provider state lock (and, for
    /// Microsoft, the registry lock). `attempt` must still be running; it is marked as
    /// committing `secret_key` durably, then `publish` runs and the journal records
    /// whether that credential version is the committed one now.
    pub(super) fn commit_attempt<T>(
        &self,
        id: IntegrationId,
        dir: &Path,
        attempt: &LoginAttemptId,
        secret_key: &str,
        publish: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let mut attempts = self.attempts_in(id, dir)?;
        attempts.start_commit(attempt, secret_key, Utc::now())?;
        let result = publish();
        // Unknown when the check cannot read: the next journal load resolves it.
        if let Ok(committed) = self.secret_committed(id, dir, secret_key) {
            let outcome = match (&result, committed) {
                (_, true) => LoginAttemptState::Succeeded,
                (Err(error), false) => failure(error),
                (Ok(_), false) => LoginAttemptState::Interrupted,
            };
            attempts.finish(attempt, outcome, Utc::now());
            if let Err(error) = attempts.save() {
                tracing::warn!(integration = %id, code = error.code(), "sign-in result not recorded");
            }
        }
        result
    }

    /// The browser part of a sign-in under `attempt`. Stops when the attempt is cancelled
    /// or outlives its lifetime: before the browser opens, while waiting for the callback
    /// (the listener closes at once), before the code exchange and after it, so a late
    /// token is dropped without being stored.
    pub(super) fn perform_oauth(
        &self,
        oauth: &OAuthClient,
        attempt: &LoginAttemptId,
    ) -> Result<Tokens> {
        let id = oauth.id;
        match id.flow() {
            SignInFlow::LoopbackPkce => {
                // No browser for a sign-in whose token exchange cannot connect.
                self.http.ready()?;
                self.ensure_attempt(id, attempt)?;
                let listener = loopback::LoopbackListener::bind()?;
                let redirect = listener.redirect_uri();
                let pkce = pkce::Pkce::new();
                let state = pkce::new_state();
                self.browser
                    .open(&oauth.authorize_url(&redirect, &state, &pkce.challenge))?;
                let code = listener.wait_for_code(&state, loopback::CALLBACK_TIMEOUT, &|| {
                    self.ensure_attempt(id, attempt)
                })?;
                self.ensure_attempt(id, attempt)?;
                let tokens = oauth.exchange_code(&code, &pkce.verifier, &redirect)?;
                self.ensure_attempt(id, attempt)?;
                Ok(tokens)
            }
            SignInFlow::SystemPermission => Err(service_error(
                "account_required",
                "native calendars do not use OAuth sign-in",
            )),
        }
    }
}
