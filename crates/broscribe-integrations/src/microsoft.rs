//! Corporate Graph discovery and route-qualified reads. No token identity decoding.
use super::*;
use crate::{
    AccountPermission, CalendarDescriptor, CalendarTarget, RemoteCalendarStamp,
    VerifiedMicrosoftIdentity,
};

/// The Graph event properties BroScribe reads; bodies, locations and attachments are never
/// requested.
const GRAPH_EVENT_SELECT: &str = "id,subject,start,end,isCancelled,isAllDay,sensitivity,responseStatus,attendees,organizer,onlineMeeting,onlineMeetingUrl,iCalUId,originalStart";

impl Backend<'_> {
    pub(crate) fn microsoft_identity(
        &self,
        token: &str,
        client: &str,
        authority: &str,
    ) -> Result<VerifiedMicrosoftIdentity> {
        let base = self.endpoints.api.trim_end_matches('/');
        let me = self
            .get_json(
                &format!(
                    "{base}/me?{}",
                    build_query(&[("$select", "id,displayName,mail,userPrincipalName")])
                ),
                token,
            )
            .map_err(|_| {
                crate::service::service_error(
                    "account_identity_unavailable",
                    "verified Microsoft profile is unavailable",
                )
            })?;
        let organization = self
            .get_json(
                &format!(
                    "{base}/organization?{}",
                    build_query(&[("$select", "id,displayName")])
                ),
                token,
            )
            .map_err(|_| {
                crate::service::service_error(
                    "account_identity_unavailable",
                    "verified resource tenant is unavailable",
                )
            })?;
        let organizations = organization.get("value").and_then(Value::as_array);
        let fail = || {
            crate::service::service_error(
                "account_identity_unavailable",
                "a unique resource tenant and object identity are required",
            )
        };
        let organizations = organizations.filter(|v| v.len() == 1).ok_or_else(fail)?;
        if organization.get("@odata.nextLink").is_some() {
            return Err(fail());
        }
        let object_id = text(&me, "id")
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(fail)?;
        let resource_tenant_id = text(&organizations[0], "id")
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(fail)?;
        let mut addresses = Vec::new();
        for field in ["mail", "userPrincipalName"] {
            if let Some(address) = text(&me, field).filter(|s| !s.trim().is_empty()) {
                let address = address.trim().to_lowercase();
                if !addresses.contains(&address) {
                    addresses.push(address);
                }
            }
        }
        Ok(VerifiedMicrosoftIdentity {
            authority_host: authority.to_owned(),
            resource_tenant_id: resource_tenant_id.to_ascii_lowercase(),
            object_id: object_id.to_ascii_lowercase(),
            client_id: client.to_owned(),
            display_name: text(&me, "displayName"),
            tenant_display_name: text(&organizations[0], "displayName"),
            verified_addresses: addresses,
        })
    }

    pub(crate) fn verify_incremental(
        &self,
        token: &str,
        permission: AccountPermission,
    ) -> Result<()> {
        match permission {
            AccountPermission::SharedCalendars => self.microsoft_catalog(token, &[]).map(|_| ()),
            AccountPermission::Microsoft365Groups => self.microsoft_groups(token).map(|_| ()),
            AccountPermission::Directory => {
                if self.settings.directory == GraphDirectory::None {
                    return Err(IntegrationError::NotConfigured(self.id.to_string(), "configure a Microsoft directory permission before granting directory access".into()));
                }
                self.directory(token, "BroScribe permission check")
                    .map(|_| ())
            }
        }
    }

    pub(crate) fn microsoft_catalog(
        &self,
        token: &str,
        reader_addresses: &[String],
    ) -> Result<Vec<CalendarDescriptor>> {
        let base = self.endpoints.api.trim_end_matches('/');
        self.pages(
            format!(
                "{base}/me/calendars?{}",
                build_query(&[
                    ("$select", "id,name,owner,isDefaultCalendar,canEdit"),
                    ("$top", "100")
                ])
            ),
            base,
            token,
            "value",
        )?
        .into_iter()
        .map(|v| {
            let calendar_id = text(&v, "id").filter(|s| !s.is_empty()).ok_or_else(|| {
                crate::service::service_error("invalid_calendar_target", "calendar has no identity")
            })?;
            let owner_address = v.get("owner").and_then(|o| text(o, "address"));
            let own = owner_address
                .as_ref()
                .is_some_and(|a| reader_addresses.iter().any(|r| r.eq_ignore_ascii_case(a)));
            Ok(CalendarDescriptor {
                target: if own {
                    CalendarTarget::Own { calendar_id }
                } else {
                    CalendarTarget::RecipientShared { calendar_id }
                },
                name: text(&v, "name"),
                owner_name: v.get("owner").and_then(|o| text(o, "name")),
                owner_address,
                is_default: v
                    .get("isDefaultCalendar")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                can_edit: v.get("canEdit").and_then(Value::as_bool),
                access: crate::Capability::Unknown,
            })
        })
        .collect()
    }

    pub(crate) fn microsoft_groups(&self, token: &str) -> Result<Vec<CalendarDescriptor>> {
        let base = self.endpoints.api.trim_end_matches('/');
        self.pages(
            format!(
                "{base}/me/memberOf/microsoft.graph.group?{}",
                build_query(&[
                    ("$select", "id,displayName,groupTypes,mail"),
                    ("$top", "100")
                ])
            ),
            base,
            token,
            "value",
        )?
        .into_iter()
        .filter(|v| {
            v.get("groupTypes")
                .and_then(Value::as_array)
                .is_some_and(|types| types.iter().any(|t| t.as_str() == Some("Unified")))
        })
        .map(|v| {
            Ok(CalendarDescriptor {
                target: CalendarTarget::Group {
                    group_id: text(&v, "id").filter(|s| !s.is_empty()).ok_or_else(|| {
                        crate::service::service_error(
                            "invalid_calendar_target",
                            "group has no identity",
                        )
                    })?,
                },
                name: text(&v, "displayName"),
                owner_name: None,
                owner_address: text(&v, "mail"),
                is_default: false,
                can_edit: None,
                access: crate::Capability::Unknown,
            })
        })
        .collect()
    }

    fn target_path(&self, target: &CalendarTarget) -> Result<String> {
        target.validate().map_err(IntegrationError::Core)?;
        Ok(match target {
            CalendarTarget::Own { calendar_id }
            | CalendarTarget::RecipientShared { calendar_id } => {
                format!("/me/calendars/{}", pct_encode(calendar_id))
            }
            CalendarTarget::Delegated {
                mailbox,
                calendar_id,
            } => match calendar_id {
                Some(id) => format!(
                    "/users/{}/calendars/{}",
                    pct_encode(mailbox),
                    pct_encode(id)
                ),
                None => format!("/users/{}/calendar", pct_encode(mailbox)),
            },
            CalendarTarget::Group { group_id } => {
                format!("/groups/{}/calendar", pct_encode(group_id))
            }
        })
    }

    pub(crate) fn verify_target(&self, token: &str, target: &CalendarTarget) -> Result<()> {
        self.get_json(
            &format!(
                "{}{}?{}",
                self.endpoints.api.trim_end_matches('/'),
                self.target_path(target)?,
                build_query(&[("$select", "id")])
            ),
            token,
        )
        .map(|_| ())
    }

    pub(crate) fn microsoft_calendar(
        &self,
        token: &str,
        identity: &VerifiedMicrosoftIdentity,
        stamp: RemoteCalendarStamp,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<CalendarEvent>> {
        stamp.target.validate().map_err(IntegrationError::Core)?;
        if end <= start || end.signed_duration_since(start) > chrono::Duration::days(366) {
            return Err(crate::service::service_error(
                "invalid_calendar_interval",
                "calendar interval must be positive and at most 366 days",
            ));
        }
        let base = self.endpoints.api.trim_end_matches('/');
        let route = match &stamp.target {
            CalendarTarget::Group { group_id } => {
                format!("/groups/{}/calendarView", pct_encode(group_id))
            }
            CalendarTarget::Delegated {
                mailbox,
                calendar_id: None,
            } => format!("/users/{}/calendarView", pct_encode(mailbox)),
            target => format!("{}/calendarView", self.target_path(target)?),
        };
        let items = self.pages(
            format!(
                "{base}{route}?{}",
                build_query(&[
                    ("startDateTime", &start.to_rfc3339()),
                    ("endDateTime", &end.to_rfc3339()),
                    ("$select", GRAPH_EVENT_SELECT),
                    ("$top", "100")
                ])
            ),
            base,
            token,
            "value",
        )?;
        let reader = |email: &str| {
            identity
                .verified_addresses
                .iter()
                .any(|a| a.eq_ignore_ascii_case(email))
        };
        let own = matches!(stamp.target, CalendarTarget::Own { .. });
        let mut events = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for v in items {
            if v.get("isCancelled").and_then(Value::as_bool) == Some(true)
                || v.get("isAllDay").and_then(Value::as_bool) == Some(true)
                || matches!(
                    v.get("sensitivity").and_then(Value::as_str),
                    Some("private" | "confidential")
                )
            {
                continue;
            }
            let attendee_values = v.get("attendees").and_then(Value::as_array);
            let declined_reader = attendee_values.into_iter().flatten().any(|a| {
                a.pointer("/emailAddress/address")
                    .and_then(Value::as_str)
                    .is_some_and(reader)
                    && a.pointer("/status/response").and_then(Value::as_str) == Some("declined")
            });
            // responseStatus belongs to the mailbox owner, not necessarily the authenticated reader.
            if declined_reader
                || (own
                    && v.pointer("/responseStatus/response")
                        .and_then(Value::as_str)
                        == Some("declined"))
            {
                continue;
            }
            let (Some(id), Some(start), Some(end)) = (
                text(&v, "id"),
                v.get("start").and_then(date),
                v.get("end").and_then(date),
            ) else {
                continue;
            };
            if end <= start {
                continue;
            }
            let occurrence_id = match v.get("originalStart") {
                Some(value) => value
                    .as_str()
                    .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                    .map(|original| original.with_timezone(&chrono::Utc).to_rfc3339()),
                None => Some(start.to_rfc3339()),
            };
            if !seen.insert((id.clone(), occurrence_id.clone())) {
                continue;
            }
            let mut attendees: Vec<Attendee> = attendee_values
                .into_iter()
                .flatten()
                .filter(|a| {
                    a.get("type").and_then(Value::as_str) != Some("resource")
                        && a.pointer("/status/response").and_then(Value::as_str) != Some("declined")
                })
                .map(|a| {
                    let email = a.get("emailAddress").and_then(|a| text(a, "address"));
                    Attendee {
                        name: a.get("emailAddress").and_then(|a| text(a, "name")),
                        is_self: email.as_deref().is_some_and(reader),
                        email,
                    }
                })
                .collect();
            if let Some(organizer) = v.pointer("/organizer/emailAddress") {
                let email = text(organizer, "address");
                if email.as_ref().is_some_and(|email| {
                    !attendees.iter().any(|a| {
                        a.email
                            .as_ref()
                            .is_some_and(|a| a.eq_ignore_ascii_case(email))
                    })
                }) {
                    attendees.push(Attendee {
                        name: text(organizer, "name"),
                        is_self: email.as_deref().is_some_and(reader),
                        email,
                    });
                }
            }
            let links: Vec<String> = v
                .get("onlineMeeting")
                .and_then(|o| text(o, "joinUrl"))
                .or_else(|| text(&v, "onlineMeetingUrl"))
                .into_iter()
                .collect();
            events.push(CalendarEvent {
                id,
                integration: self.id,
                title: text(&v, "subject"),
                start,
                end,
                app: meeting_app(&links),
                join_links: links,
                attendees,
                account_generation: None,
                google: None,
                remote: Some(stamp.clone()),
                ical_uid: text(&v, "iCalUId"),
                occurrence_id,
            });
        }
        Ok(events)
    }
}
