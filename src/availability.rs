//! Seller availability gates (M15.23.g).
//!
//! **F29 sweep:** marketing-specific by design. Takes
//! `nexo_tool_meta::marketing::Seller` (CRM shape) directly.
//! The window logic + IANA tz resolve is generic but
//! introducing a `HasWorkingHours` trait abstraction for one
//! consumer is overengineering — lift when 2nd consumer
//! appears.
//!
//! Two layers of availability protection live on top of the
//! routing dispatcher's seller assignment:
//!
//! 1. **Vacation mode** — `seller.on_vacation = true` short-
//!    circuits to `unavailable` regardless of the time. The
//!    optional `vacation_until` (UTC date) lets the operator
//!    schedule a return date without manually flipping the
//!    flag back; on the first inbound past `vacation_until`
//!    the gate yields `available` (the operator still owns
//!    flipping `on_vacation` back to `false` to clear the
//!    label, but no leads get misassigned in the gap).
//! 2. **Working hours window** — when the seller has a
//!    `WorkingHoursWindow`, the current `now_utc` is converted
//!    to the seller's `timezone` (IANA), the matching weekday
//!    block is selected (`mon_fri`, `saturday`, `sunday`), and
//!    the local `HH:MM` is checked against `start..end`. No
//!    block defined for the weekday ⇒ off that day.
//!
//! When the seller has neither block, the gate defaults to
//! `available` — sellers without a working_hours block are
//! always-on (matches the existing UX where the editor leaves
//! the toggle off by default).
//!
//! Pure logic — every function takes `now_utc` as a parameter
//! so tests can pin a deterministic instant.

use chrono::{DateTime, Datelike, NaiveTime, TimeZone, Utc, Weekday};
use chrono_tz::Tz;
use nexo_tool_meta::marketing::{DayWindow, Seller, WorkingHoursWindow};

/// What the gate decided for one seller at one point in time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AvailabilityOutcome {
    /// Seller is open for assignment right now.
    Available,
    /// Seller is on vacation. `until` mirrors
    /// `seller.vacation_until` so the caller can surface the
    /// expected return date in the audit log.
    OnVacation {
        /// Inclusive end of the vacation window. `None` when
        /// the operator flagged vacation without an end date —
        /// gate stays closed indefinitely.
        until: Option<DateTime<Utc>>,
    },
    /// Outside the seller's configured working hours.
    OffHours {
        /// Seller-local timezone the gate evaluated in.
        timezone: String,
        /// 3-char weekday label (`Mon`, `Sat`, `Sun`) for the
        /// audit log. The dispatcher's `why` chain shows
        /// "off_hours: Sat 14:32 (America/Bogota)".
        weekday_label: &'static str,
        /// Local `HH:MM` the gate computed for the audit row.
        local_hhmm: String,
    },
    /// Working hours config carried an unknown timezone or
    /// malformed `HH:MM`. Surface as a typed reason so the
    /// operator's UI flags the misconfig instead of silently
    /// dropping leads.
    Misconfigured {
        /// Short label the audit log records ("invalid_timezone",
        /// "invalid_hhmm").
        reason: &'static str,
    },
}

impl AvailabilityOutcome {
    /// `true` when the gate yielded `Available`.
    pub fn is_available(&self) -> bool {
        matches!(self, AvailabilityOutcome::Available)
    }

    /// One-line audit reason for the routing `why` chain.
    /// Empty when available — caller should not append
    /// anything in that case.
    pub fn audit_reason(&self) -> Option<String> {
        match self {
            AvailabilityOutcome::Available => None,
            AvailabilityOutcome::OnVacation { until: Some(t) } => Some(format!(
                "on_vacation_until_{}",
                t.format("%Y-%m-%d")
            )),
            AvailabilityOutcome::OnVacation { until: None } => {
                Some("on_vacation".into())
            }
            AvailabilityOutcome::OffHours {
                weekday_label,
                local_hhmm,
                timezone,
            } => Some(format!(
                "off_hours_{weekday_label}_{local_hhmm}_{timezone}"
            )),
            AvailabilityOutcome::Misconfigured { reason } => {
                Some(format!("availability_misconfigured_{reason}"))
            }
        }
    }
}

/// Evaluate one seller's availability at `now_utc`. Vacation
/// takes precedence over working hours: an on-vacation seller
/// is closed even during their normal weekday window.
pub fn check(seller: &Seller, now_utc: DateTime<Utc>) -> AvailabilityOutcome {
    if seller.on_vacation {
        // Past `vacation_until` ⇒ open the gate even though
        // the flag still says vacation. The operator clears
        // the flag manually next time they edit the row.
        if let Some(until) = seller.vacation_until {
            if now_utc > until {
                // Continue to working-hours check below.
            } else {
                return AvailabilityOutcome::OnVacation {
                    until: Some(until),
                };
            }
        } else {
            return AvailabilityOutcome::OnVacation { until: None };
        }
    }

    let Some(window) = seller.working_hours.as_ref() else {
        // No working hours configured ⇒ always-on (matches
        // the SellerForm default where the toggle starts off).
        return AvailabilityOutcome::Available;
    };

    check_window(window, now_utc)
}

fn check_window(
    window: &WorkingHoursWindow,
    now_utc: DateTime<Utc>,
) -> AvailabilityOutcome {
    let tz: Tz = match window.timezone.parse() {
        Ok(t) => t,
        Err(_) => {
            return AvailabilityOutcome::Misconfigured {
                reason: "invalid_timezone",
            };
        }
    };
    let local = tz.from_utc_datetime(&now_utc.naive_utc());
    let weekday = local.weekday();
    let weekday_label = weekday_short_label(weekday);
    let local_hhmm = format!("{:02}:{:02}", local.hour(), local.minute());

    let day_window = match weekday {
        Weekday::Mon
        | Weekday::Tue
        | Weekday::Wed
        | Weekday::Thu
        | Weekday::Fri => window.mon_fri.as_ref(),
        Weekday::Sat => window.saturday.as_ref(),
        Weekday::Sun => window.sunday.as_ref(),
    };

    let Some(day_window) = day_window else {
        // Weekday is off (no block configured for it).
        return AvailabilityOutcome::OffHours {
            timezone: window.timezone.clone(),
            weekday_label,
            local_hhmm,
        };
    };

    let Some((start, end)) = parse_hhmm_pair(day_window) else {
        return AvailabilityOutcome::Misconfigured {
            reason: "invalid_hhmm",
        };
    };
    let now_local_time = NaiveTime::from_hms_opt(
        local.hour(),
        local.minute(),
        local.second(),
    )
    .unwrap_or_else(|| NaiveTime::from_hms_opt(0, 0, 0).unwrap());
    if now_local_time >= start && now_local_time < end {
        AvailabilityOutcome::Available
    } else {
        AvailabilityOutcome::OffHours {
            timezone: window.timezone.clone(),
            weekday_label,
            local_hhmm,
        }
    }
}

use chrono::Timelike as _;

fn parse_hhmm_pair(window: &DayWindow) -> Option<(NaiveTime, NaiveTime)> {
    let start = parse_hhmm(&window.start)?;
    let end = parse_hhmm(&window.end)?;
    if start >= end {
        return None;
    }
    Some((start, end))
}

fn parse_hhmm(s: &str) -> Option<NaiveTime> {
    let mut parts = s.split(':');
    let h: u32 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    NaiveTime::from_hms_opt(h, m, 0)
}

fn weekday_short_label(w: Weekday) -> &'static str {
    match w {
        Weekday::Mon => "Mon",
        Weekday::Tue => "Tue",
        Weekday::Wed => "Wed",
        Weekday::Thu => "Thu",
        Weekday::Fri => "Fri",
        Weekday::Sat => "Sat",
        Weekday::Sun => "Sun",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexo_tool_meta::marketing::{SellerId, SellerNotificationSettings, TenantIdRef};

    fn seller(id: &str) -> Seller {
        Seller {
            id: SellerId(id.into()),
            tenant_id: TenantIdRef("acme".into()),
            name: id.into(),
            primary_email: format!("{id}@acme.com"),
            alt_emails: vec![],
            signature_text: String::new(),
            working_hours: None,
            on_vacation: false,
            vacation_until: None,
            preferred_language: None,
            agent_id: None,
            notification_settings: Some(SellerNotificationSettings::default()),
            smtp_credential: None,
            model_override: None,
            draft_template: None,
        }
    }

    fn t(date: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(date).unwrap().with_timezone(&Utc)
    }

    fn bogota_window() -> WorkingHoursWindow {
        WorkingHoursWindow {
            timezone: "America/Bogota".into(),
            mon_fri: Some(DayWindow {
                start: "09:00".into(),
                end: "17:00".into(),
            }),
            saturday: None,
            sunday: None,
        }
    }

    #[test]
    fn no_working_hours_means_always_available() {
        let s = seller("pedro");
        assert!(check(&s, t("2026-05-12T03:00:00Z")).is_available());
        assert!(check(&s, t("2026-05-12T23:30:00Z")).is_available());
    }

    #[test]
    fn on_vacation_blocks_assignment() {
        let mut s = seller("pedro");
        s.on_vacation = true;
        let out = check(&s, t("2026-05-12T15:00:00Z"));
        assert!(matches!(out, AvailabilityOutcome::OnVacation { .. }));
        assert!(!out.is_available());
    }

    #[test]
    fn vacation_until_past_re_opens_gate() {
        let mut s = seller("pedro");
        s.on_vacation = true;
        // Vacation ended yesterday — current call after that.
        s.vacation_until = Some(t("2026-05-10T23:59:59Z"));
        let out = check(&s, t("2026-05-12T15:00:00Z"));
        // No working_hours configured ⇒ Available once
        // vacation expired.
        assert!(out.is_available());
    }

    #[test]
    fn vacation_with_future_until_still_blocks() {
        let mut s = seller("pedro");
        s.on_vacation = true;
        s.vacation_until = Some(t("2026-05-20T00:00:00Z"));
        let out = check(&s, t("2026-05-12T15:00:00Z"));
        match out {
            AvailabilityOutcome::OnVacation { until } => {
                assert!(until.is_some());
            }
            other => panic!("expected OnVacation, got {other:?}"),
        }
    }

    #[test]
    fn working_hours_inside_window_is_available() {
        let mut s = seller("pedro");
        s.working_hours = Some(bogota_window());
        // 2026-05-12 is a Tuesday. 14:00 UTC = 09:00 Bogota
        // (UTC-5). Window is 09:00-17:00 ⇒ available.
        let out = check(&s, t("2026-05-12T14:00:00Z"));
        assert!(out.is_available(), "got {out:?}");
    }

    #[test]
    fn working_hours_before_open_is_off_hours() {
        let mut s = seller("pedro");
        s.working_hours = Some(bogota_window());
        // 13:00 UTC = 08:00 Bogota — 1h before open.
        let out = check(&s, t("2026-05-12T13:00:00Z"));
        match out {
            AvailabilityOutcome::OffHours {
                weekday_label,
                local_hhmm,
                timezone,
            } => {
                assert_eq!(weekday_label, "Tue");
                assert_eq!(local_hhmm, "08:00");
                assert_eq!(timezone, "America/Bogota");
            }
            other => panic!("expected OffHours, got {other:?}"),
        }
    }

    #[test]
    fn working_hours_after_close_is_off_hours() {
        let mut s = seller("pedro");
        s.working_hours = Some(bogota_window());
        // 23:00 UTC = 18:00 Bogota — 1h after close.
        let out = check(&s, t("2026-05-12T23:00:00Z"));
        assert!(matches!(out, AvailabilityOutcome::OffHours { .. }));
    }

    #[test]
    fn weekend_with_no_block_is_off_hours() {
        let mut s = seller("pedro");
        s.working_hours = Some(bogota_window());
        // 2026-05-16 is a Saturday — no `saturday` block.
        let out = check(&s, t("2026-05-16T15:00:00Z"));
        match out {
            AvailabilityOutcome::OffHours { weekday_label, .. } => {
                assert_eq!(weekday_label, "Sat");
            }
            other => panic!("expected OffHours, got {other:?}"),
        }
    }

    #[test]
    fn weekend_with_block_honoured() {
        let mut s = seller("pedro");
        let mut w = bogota_window();
        w.saturday = Some(DayWindow {
            start: "10:00".into(),
            end: "14:00".into(),
        });
        s.working_hours = Some(w);
        // 2026-05-16 Saturday, 16:00 UTC = 11:00 Bogota.
        let out = check(&s, t("2026-05-16T16:00:00Z"));
        assert!(out.is_available(), "got {out:?}");
    }

    #[test]
    fn invalid_timezone_returns_misconfigured() {
        let mut s = seller("pedro");
        s.working_hours = Some(WorkingHoursWindow {
            timezone: "Mars/Olympus".into(),
            mon_fri: Some(DayWindow {
                start: "09:00".into(),
                end: "17:00".into(),
            }),
            saturday: None,
            sunday: None,
        });
        let out = check(&s, t("2026-05-12T14:00:00Z"));
        match out {
            AvailabilityOutcome::Misconfigured { reason } => {
                assert_eq!(reason, "invalid_timezone");
            }
            other => panic!("expected Misconfigured, got {other:?}"),
        }
    }

    #[test]
    fn invalid_hhmm_returns_misconfigured() {
        let mut s = seller("pedro");
        s.working_hours = Some(WorkingHoursWindow {
            timezone: "America/Bogota".into(),
            mon_fri: Some(DayWindow {
                start: "9 AM".into(),
                end: "5 PM".into(),
            }),
            saturday: None,
            sunday: None,
        });
        let out = check(&s, t("2026-05-12T14:00:00Z"));
        assert!(matches!(
            out,
            AvailabilityOutcome::Misconfigured { reason: "invalid_hhmm" }
        ));
    }

    #[test]
    fn audit_reason_carries_context() {
        let mut s = seller("pedro");
        s.on_vacation = true;
        s.vacation_until = Some(t("2026-05-20T00:00:00Z"));
        let out = check(&s, t("2026-05-12T15:00:00Z"));
        assert_eq!(
            out.audit_reason().as_deref(),
            Some("on_vacation_until_2026-05-20"),
        );
        let avail = check(&seller("pedro"), t("2026-05-12T15:00:00Z"));
        assert_eq!(avail.audit_reason(), None);
    }
}
