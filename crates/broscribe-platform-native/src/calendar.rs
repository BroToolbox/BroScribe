//! Live calendar authority for consumers of durable calendar snapshots
//! (workers, CLI): the installed agent owns the EventKit grant, calendar
//! opt-ins and remote account generations, so this asks it over its private
//! endpoint and never touches EventKit or provider credentials. Every
//! failure is an error, never a guessed generation; an agent that predates
//! a method fails closed as `helper_outdated`.

use std::time::Duration;

use broscribe_ipc::{Call, IpcError};
use broscribe_platform::{CalendarAuthority, CalendarProvider, Error, Result};
use serde_json::Value;

use crate::ipc::{CallError, Client, Endpoint};

/// Total time of one authority request; the agent answers from memory.
pub const AUTHORITY_TIMEOUT: Duration = Duration::from_secs(10);

/// Service codes the agent may answer with that keep their meaning here.
const KNOWN_CODES: [&str; 7] = [
    "calendar_denied",
    "calendar_restricted",
    "calendar_write_only",
    "calendar_pending",
    "helper_required",
    "helper_outdated",
    "busy",
];

/// The agent's read-only calendar authority calls over its endpoint.
pub struct HelperCalendarAuthority {
    endpoint: Endpoint,
    timeout: Duration,
}

impl HelperCalendarAuthority {
    pub fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            timeout: AUTHORITY_TIMEOUT,
        }
    }

    /// One read-only call; `what` names the authority in errors.
    fn ask(&self, call: Call, what: &str) -> Result<Value> {
        let mut client = match Client::connect_daemon(&self.endpoint, self.timeout) {
            Ok(Some(client)) => client,
            Ok(None) => {
                return Err(helper_required(&format!(
                    "BroScribe Agent is not running, so the {what} is unknown"
                )));
            }
            // An endpoint that is not private cannot be trusted.
            Err(e) => return Err(Error::io("BroScribe Agent endpoint", e)),
        };
        match client.call(call) {
            Ok(value) => Ok(value),
            Err(CallError::Remote(error)) => Err(remote(error)),
            Err(e) => Err(helper_required(&format!(
                "BroScribe Agent did not answer, so the {what} is unknown: {e}"
            ))),
        }
    }
}

fn authority_call(provider: CalendarProvider) -> Call {
    match provider {
        CalendarProvider::EventKit => Call::SystemCalendarAuthority,
    }
}

impl CalendarAuthority for HelperCalendarAuthority {
    fn active_generation(&self, provider: CalendarProvider) -> Result<Option<u64>> {
        generation(
            self.ask(authority_call(provider), "calendar grant")?,
            "calendar authority",
        )
    }

    fn remote_authority(
        &self,
        account: &broscribe_core::calendar::AccountRef,
    ) -> Result<Option<broscribe_core::calendar::AccountAuthority>> {
        let value = self.ask(
            Call::MicrosoftCalendarAuthority {
                account: account.clone(),
            },
            "remote calendar authority",
        )?;
        let invalid =
            || Error::Internal("BroScribe Agent sent an invalid remote calendar authority".into());
        if !value
            .as_object()
            .is_some_and(|map| map.len() == 1 && map.contains_key("authority"))
        {
            return Err(invalid());
        }
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Answer {
            authority: Option<broscribe_core::calendar::AccountAuthority>,
        }
        let answer: Answer = serde_json::from_value(value).map_err(|_| invalid())?;
        if answer
            .authority
            .as_ref()
            .is_some_and(|live| live.account != *account)
        {
            return Err(Error::Internal(
                "BroScribe Agent answered for another calendar account".into(),
            ));
        }
        Ok(answer.authority)
    }

    fn google_authority(&self) -> Result<Option<broscribe_core::calendar::GoogleCalendarStamp>> {
        google_stamp(self.ask(Call::GoogleCalendarAuthority, "Google calendar authority")?)
    }
}

/// Exactly `{"authority": {"generation": u64, "selection_generation": u64} | null}`:
/// both counters or nothing. A bare or partial generation (an older agent's scalar
/// answer) is refused rather than completed with a guessed selection.
fn google_stamp(value: Value) -> Result<Option<broscribe_core::calendar::GoogleCalendarStamp>> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Stamp {
        generation: u64,
        selection_generation: u64,
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Answer {
        authority: Option<Stamp>,
    }
    let invalid =
        || Error::Internal("BroScribe Agent sent an invalid Google calendar authority".into());
    if !value
        .as_object()
        .is_some_and(|map| map.len() == 1 && map.contains_key("authority"))
    {
        return Err(invalid());
    }
    let answer: Answer = serde_json::from_value(value).map_err(|_| invalid())?;
    Ok(answer
        .authority
        .map(|stamp| broscribe_core::calendar::GoogleCalendarStamp {
            generation: stamp.generation,
            selection_generation: stamp.selection_generation,
        }))
}

/// Exactly `{"generation": u64 | null}`; anything more or less is refused.
fn generation(value: Value, what: &str) -> Result<Option<u64>> {
    let invalid = || Error::Internal(format!("BroScribe Agent sent an invalid {what}"));
    let Value::Object(map) = value else {
        return Err(invalid());
    };
    if map.len() != 1 {
        return Err(invalid());
    }
    match map.get("generation") {
        Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number.as_u64().map(Some).ok_or_else(invalid),
        _ => Err(invalid()),
    }
}

fn remote(error: IpcError) -> Error {
    // These calls have no free-form parameters: a refused request is an
    // agent without the method (or protocol), never a reason to guess.
    if error.code == IpcError::INVALID_REQUEST || error.code == IpcError::UNSUPPORTED_VERSION {
        return Error::Service {
            code: "helper_outdated",
            message: format!(
                "BroScribe Agent does not support this calendar authority; update and restart it ({})",
                error.message
            ),
        };
    }
    match KNOWN_CODES.iter().find(|code| **code == error.code) {
        Some(&code) => Error::Service {
            code,
            message: error.message,
        },
        None => error.into_core(),
    }
}

fn helper_required(message: &str) -> Error {
    Error::Service {
        code: "helper_required",
        message: message.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use broscribe_ipc::Response;
    use serde_json::json;

    use super::*;
    use crate::ipc::tests::{FakeDaemon, Reply};

    fn authority(endpoint: Endpoint) -> HelperCalendarAuthority {
        HelperCalendarAuthority {
            endpoint,
            timeout: Duration::from_secs(5),
        }
    }

    /// A synthetic agent answering every request with `answer`.
    fn agent(answer: std::result::Result<Value, IpcError>) -> FakeDaemon {
        FakeDaemon::start(move |request| {
            Reply::Respond(match &answer {
                Ok(value) => Response::ok(request.id.clone(), value.clone()),
                Err(error) => Response::err(request.id.clone(), error.clone()),
            })
        })
    }

    /// The calls the agent received.
    fn calls(daemon: &FakeDaemon) -> Vec<Call> {
        daemon.seen().into_iter().map(|(_, call)| call).collect()
    }

    fn ask(answer: std::result::Result<Value, IpcError>) -> Result<Option<u64>> {
        let daemon = agent(answer);
        let result =
            authority(daemon.endpoint.clone()).active_generation(CalendarProvider::EventKit);
        assert_eq!(calls(&daemon), vec![Call::SystemCalendarAuthority]);
        result
    }

    fn ask_google(
        answer: std::result::Result<Value, IpcError>,
    ) -> Result<Option<broscribe_core::calendar::GoogleCalendarStamp>> {
        let daemon = agent(answer);
        let result = authority(daemon.endpoint.clone()).google_authority();
        let calls = calls(&daemon);
        assert_eq!(calls, vec![Call::GoogleCalendarAuthority]);
        assert!(!calls[0].is_mutating());
        result
    }

    #[test]
    fn reports_the_agents_generation_or_none() {
        assert_eq!(ask(Ok(json!({"generation": 7}))).unwrap(), Some(7));
        assert_eq!(ask(Ok(json!({"generation": null}))).unwrap(), None);
    }

    /// Google authority is the account generation AND the calendar selection
    /// generation, read-only; a scalar, partial, extended or substituted answer
    /// is never completed into a pair.
    #[test]
    fn google_authority_is_read_only_and_requires_the_exact_pair() {
        use broscribe_core::calendar::GoogleCalendarStamp;
        let live = GoogleCalendarStamp {
            generation: 3,
            selection_generation: 9,
        };
        assert_eq!(
            ask_google(Ok(
                json!({"authority": {"generation": 3, "selection_generation": 9}})
            ))
            .unwrap(),
            Some(live)
        );
        assert_eq!(ask_google(Ok(json!({"authority": null}))).unwrap(), None);
        for value in [
            // An agent still answering the old scalar generation.
            json!({"generation": 3}),
            json!({"generation": null}),
            json!({"authority": {"generation": 3}}),
            json!({"authority": {"selection_generation": 9}}),
            json!({"authority": 3}),
            json!({"authority": {"generation": 3, "selection_generation": 9, "account": "a"}}),
            json!({"authority": {"generation": 3, "selection_generation": 9}, "generation": 3}),
            json!({"authority": {"generation": -1, "selection_generation": 9}}),
            json!({"authority": {"generation": 3, "selection_generation": "9"}}),
            json!({}),
            json!(null),
        ] {
            assert_eq!(
                ask_google(Ok(value.clone())).unwrap_err().code(),
                "internal",
                "{value}"
            );
        }
    }

    #[test]
    fn an_old_agent_without_google_authority_fails_closed() {
        for code in [IpcError::INVALID_REQUEST, IpcError::UNSUPPORTED_VERSION] {
            let error = ask_google(Err(IpcError::new(
                code,
                "unknown variant `integrations.google_calendar_authority`",
            )))
            .unwrap_err();
            assert_eq!(error.code(), "helper_outdated");
        }
        let missing = tempfile::tempdir().unwrap();
        assert_eq!(
            authority(Endpoint::new(missing.path()))
                .google_authority()
                .unwrap_err()
                .code(),
            "helper_required"
        );
    }

    #[test]
    fn remote_authority_uses_exact_account_read_only_and_refuses_substitution() {
        use broscribe_core::calendar::{
            AccountAuthority, AccountId, AccountRef, CalendarTarget, RemoteIntegration,
        };
        let account = AccountRef {
            integration: RemoteIntegration::MicrosoftGraph,
            account_id: AccountId::parse("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap(),
        };
        let live = AccountAuthority {
            account: account.clone(),
            generation: 7,
            selection_generation: 11,
            selected_calendars: vec![CalendarTarget::Own {
                calendar_id: "same-id".into(),
            }],
        };
        for answer in [json!({"authority": live}), json!({"authority": null})] {
            let expected = answer["authority"].clone();
            let daemon = agent(Ok(answer));
            let result = authority(daemon.endpoint.clone())
                .remote_authority(&account)
                .unwrap();
            assert_eq!(serde_json::to_value(result).unwrap(), expected);
            let calls = calls(&daemon);
            assert_eq!(
                calls,
                vec![Call::MicrosoftCalendarAuthority {
                    account: account.clone()
                }]
            );
            assert!(!calls[0].is_mutating());
        }
        let mut other = live.clone();
        other.account.account_id = AccountId::parse("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
        for answer in [
            json!({"authority": other}),
            json!({}),
            json!({"generation": 7}),
            json!({"authority": null, "extra": true}),
        ] {
            let daemon = agent(Ok(answer));
            assert!(
                authority(daemon.endpoint.clone())
                    .remote_authority(&account)
                    .is_err()
            );
        }
    }

    #[test]
    fn refuses_anything_but_a_bare_generation() {
        for value in [
            json!({"generation": 1, "calendars": [{"id": "x", "title": "Work"}]}),
            json!({}),
            json!({"generation": -1}),
            json!({"generation": 1.5}),
            json!({"generation": "1"}),
            json!(null),
            json!([1]),
        ] {
            let error = ask(Ok(value.clone())).unwrap_err();
            assert_eq!(error.code(), "internal", "{value}");
        }
    }

    #[test]
    fn an_agent_without_the_method_is_outdated() {
        let error = ask(Err(IpcError::new(
            IpcError::INVALID_REQUEST,
            "unknown variant `integrations.system_calendar_authority`",
        )))
        .unwrap_err();
        assert_eq!(error.code(), "helper_outdated");
    }

    #[test]
    fn known_service_codes_survive_and_others_keep_core_codes() {
        let error = ask(Err(IpcError::new("calendar_denied", "denied"))).unwrap_err();
        assert_eq!(error.code(), "calendar_denied");
        let error = ask(Err(IpcError::new("not_configured", "off"))).unwrap_err();
        assert_eq!(error.code(), "not_configured");
        let error = ask(Err(IpcError::new("something_new", "?"))).unwrap_err();
        assert_eq!(error.code(), "internal");
    }

    #[test]
    fn a_missing_or_silent_agent_requires_the_helper() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = authority(Endpoint::new(&tmp.path().join("absent")));
        assert_eq!(
            missing
                .active_generation(CalendarProvider::EventKit)
                .unwrap_err()
                .code(),
            "helper_required"
        );
        // An agent that closes the connection without answering.
        let daemon = FakeDaemon::start(|_| Reply::Close);
        assert_eq!(
            authority(daemon.endpoint.clone())
                .active_generation(CalendarProvider::EventKit)
                .unwrap_err()
                .code(),
            "helper_required"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_shared_socket_folder_is_never_contacted() {
        use std::fs::{self, Permissions};
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixListener;

        let (_tmp, endpoint) = crate::ipc::tests::private_endpoint();
        let socket = endpoint.socket_location().unwrap().clone();
        let listener = UnixListener::bind(&socket.path).unwrap();
        listener.set_nonblocking(true).unwrap();
        fs::set_permissions(&socket.dir, Permissions::from_mode(0o755)).unwrap();
        let error = authority(endpoint)
            .active_generation(CalendarProvider::EventKit)
            .unwrap_err();
        assert_eq!(error.code(), "io");
        assert!(listener.accept().is_err(), "connected to a shared folder");
        fs::set_permissions(&socket.dir, Permissions::from_mode(0o700)).unwrap();
    }
}
