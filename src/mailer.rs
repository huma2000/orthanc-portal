//! # mailer.rs
//!
//! Sends **share invitations by email** through the practice's own mail server (SMTP), set up by an
//! administrator in Settings > Network & sharing > Email. Optional: with no mail server set, the
//! portal never sends anything and the Share windows say so.
//!
//! * Plain-text emails only: nothing in them can run, and the link is shown exactly as it is.
//! * The password is included only when the staff member ticks it for that email (the safer way
//!   is to give it by another channel, a phone call or a text message).
//! * Addresses are checked here before anything is created or changed, so a typo never leaves a
//!   half-done invitation.

use crate::settings::{Settings, SmtpSecurity};
use lettre::message::header::ContentType;
use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// At most this many recipients per invitation.
pub const MAX_RECIPIENTS: usize = 10;

/// At most this many invitation recipients per staff member and per hour. Plenty for sharing studies,
/// but a stolen account cannot turn the practice's mail server into a source of spam or phishing.
pub const MAX_RECIPIENTS_PER_HOUR: usize = 30;

/// When each staff member sent invitation emails (unix seconds, one entry per recipient), for the
/// last hour. In memory: a restart forgets it, which is harmless.
fn sent_log() -> &'static Mutex<HashMap<String, Vec<i64>>> {
    static LOG: OnceLock<Mutex<HashMap<String, Vec<i64>>>> = OnceLock::new();
    LOG.get_or_init(Default::default)
}

/// May `user` email `recipients` more people now? If yes, they are counted at once (so two
/// simultaneous requests cannot both slip under the limit).
pub fn reserve_quota(user: &str, recipients: usize, now: i64) -> Result<(), String> {
    let mut log = sent_log().lock().unwrap_or_else(|e| e.into_inner());
    log.retain(|_, times| {
        times.retain(|t| now - t < 3600);
        !times.is_empty()
    });
    let times = log.entry(user.to_lowercase()).or_default();
    if times.len() + recipients > MAX_RECIPIENTS_PER_HOUR {
        return Err(format!(
            "You can email invitations to at most {MAX_RECIPIENTS_PER_HOUR} people per hour. Try again later, or \
             copy the link and send it yourself."
        ));
    }
    times.extend(std::iter::repeat_n(now, recipients));
    Ok(())
}

/// "a@b.org, Dr Smith <c@d.org>; e@f.org" -> the mailboxes, or a message saying what is wrong.
pub fn parse_recipients(raw: &str) -> Result<Vec<Mailbox>, String> {
    let parts: Vec<&str> = raw.split([',', ';', '\n']).map(str::trim).filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return Err("Enter the email address of the person to invite".into());
    }
    if parts.len() > MAX_RECIPIENTS {
        return Err(format!("At most {MAX_RECIPIENTS} email addresses"));
    }
    parts
        .iter()
        .map(|p| {
            if p.len() > 254 || p.chars().any(char::is_control) {
                return Err(format!("'{p}' is not a valid email address"));
            }
            p.parse::<Mailbox>().map_err(|_| format!("'{p}' is not a valid email address"))
        })
        .collect()
}

/// The mailboxes as text, for answers and the audit log: "a@b.org, c@d.org".
pub fn addresses(to: &[Mailbox]) -> String {
    to.iter().map(|m| m.email.to_string()).collect::<Vec<_>>().join(", ")
}

/// Sends one plain-text email with the saved settings. Runs the (blocking) SMTP conversation on a
/// worker thread. The error is a sentence for the person who pressed the button.
pub async fn send(settings: &Settings, to: Vec<Mailbox>, subject: &str, body: &str) -> Result<(), String> {
    if !settings.email_configured() {
        return Err("Email is not set up: an administrator must enter the mail server in Settings > Email".into());
    }
    let from: Mailbox =
        settings.smtp_from.parse().map_err(|_| "The sender address in Settings > Email is not valid".to_string())?;
    let mut builder = Message::builder().from(from).subject(subject).header(ContentType::TEXT_PLAIN);
    for mailbox in to {
        builder = builder.to(mailbox);
    }
    let message = builder.body(body.to_string()).map_err(|e| format!("The email could not be prepared: {e}"))?;

    let host = settings.smtp_host.as_str();
    let transport = match settings.smtp_security {
        SmtpSecurity::None => Ok(SmtpTransport::builder_dangerous(host)),
        SmtpSecurity::Starttls => SmtpTransport::starttls_relay(host),
        SmtpSecurity::Tls => SmtpTransport::relay(host),
    }
    .map_err(|e| format!("The mail server address is not valid: {e}"))?
    .port(settings.smtp_port as u16)
    .timeout(Some(Duration::from_secs(20)));
    let transport = if settings.smtp_username.is_empty() {
        transport
    } else {
        transport.credentials(Credentials::new(settings.smtp_username.clone(), settings.smtp_password.0.clone()))
    }
    .build();

    let server = format!("{host}:{}", settings.smtp_port);
    tokio::task::spawn_blocking(move || transport.send(&message))
        .await
        .map_err(|e| format!("The email could not be sent: {e}"))?
        .map(|_| ())
        .map_err(|e| {
            tracing::warn!("email through {server} failed: {e}");
            format!("The mail server ({server}) did not accept the email: {e}")
        })
}

/// "2 days 5 hours 20 minutes" (the parts that are not zero).
pub fn duration_words(minutes: u64) -> String {
    let (d, h, m) = (minutes / 1440, (minutes % 1440) / 60, minutes % 60);
    let part = |n: u64, one: &str, many: &str| match n {
        0 => None,
        1 => Some(format!("1 {one}")),
        n => Some(format!("{n} {many}")),
    };
    let parts: Vec<String> = [part(d, "day", "days"), part(h, "hour", "hours"), part(m, "minute", "minutes")]
        .into_iter()
        .flatten()
        .collect();
    if parts.is_empty() {
        "less than a minute".into()
    } else {
        parts.join(" ")
    }
}

/// What an invitation email says.
pub struct Invitation<'a> {
    pub portal_name: &'a str,
    pub sender: &'a str,
    pub url: &'a str,
    pub minutes_left: u64,
    pub downloads_left: i64,
    /// `Some` = included in the email (ticked by the staff member).
    pub password: Option<&'a str>,
    /// Optional personal message of the staff member.
    pub message: &'a str,
    /// The link was changed (more time, downloads, a new password), not just created.
    pub updated: bool,
    /// Settings > Email > Text at the end of every email (address, phone...).
    pub footer: &'a str,
}

impl Invitation<'_> {
    pub fn subject(&self) -> String {
        if self.updated {
            format!("{}: your link to a shared study was updated", self.portal_name)
        } else {
            format!("{}: an imaging study has been shared with you", self.portal_name)
        }
    }

    pub fn body(&self) -> String {
        let mut text = String::from("Hello,\n\n");
        if self.updated {
            text +=
                &format!("{} at {} has updated your link to a shared imaging study.\n", self.sender, self.portal_name);
        } else {
            text += &format!("{} at {} has shared an imaging study with you.\n", self.sender, self.portal_name);
        }
        if !self.message.is_empty() {
            text += &format!("\nMessage from {}:\n", self.sender);
            for line in self.message.lines() {
                text += &format!("  {line}\n");
            }
        }
        text += &format!("\nOpen the study here:\n{}\n\n", self.url);
        text += &format!("The link works for {} from the time of this email.\n", duration_words(self.minutes_left));
        text += &match self.downloads_left {
            0 => "You can look at the study online (downloading it is not allowed).\n".to_string(),
            1 => "You can look at the study online and download it once.\n".to_string(),
            n => format!("You can look at the study online and download it {n} times.\n"),
        };
        text += &match self.password {
            Some(p) => format!("\nPassword: {p}\n(capital letters, spaces and dashes do not matter)\n"),
            None => "\nYou need a password to open it. It will be given to you separately (for example by phone \
                     or text message).\n"
                .to_string(),
        };
        text += &format!(
            "\nThis email was sent by {}. If you were not expecting it, you can ignore it.\n",
            self.portal_name
        );
        if !self.footer.is_empty() {
            text += &format!("\n--\n{}\n", self.footer);
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recipients_are_checked() {
        assert_eq!(parse_recipients("a@b.org, Dr Smith <c@d.org>;e@f.org").unwrap().len(), 3);
        assert!(parse_recipients("").is_err());
        assert!(parse_recipients("not an address").is_err());
        assert!(parse_recipients("a@b.org\r\nBcc: x@y.org").is_err(), "no header injection");
        assert!(parse_recipients(&["a@b.org"; 11].join(",")).is_err());
    }

    #[test]
    fn emails_per_hour_are_limited() {
        let now = 1_000_000;
        assert!(reserve_quota("quota-test", MAX_RECIPIENTS_PER_HOUR - 1, now).is_ok());
        assert!(reserve_quota("QUOTA-TEST", 2, now).is_err(), "the same person, whatever the case");
        assert!(reserve_quota("quota-test", 1, now + 10).is_ok(), "exactly at the limit");
        assert!(reserve_quota("quota-test", 1, now + 20).is_err());
        assert!(reserve_quota("someone-else", 5, now).is_ok(), "other people are not affected");
        assert!(reserve_quota("quota-test", 1, now + 3600).is_ok(), "an hour later");
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration_words(2 * 1440 + 5 * 60 + 20), "2 days 5 hours 20 minutes");
        assert_eq!(duration_words(60), "1 hour");
        assert_eq!(duration_words(1441), "1 day 1 minute");
    }

    #[test]
    fn the_password_is_only_there_when_asked() {
        let mut inv = Invitation {
            portal_name: "Demo",
            sender: "doctor",
            url: "https://x/s/abc",
            minutes_left: 2880,
            downloads_left: 1,
            password: None,
            message: "Second opinion please",
            updated: false,
            footer: "1 High Street",
        };
        let body = inv.body();
        assert!(body.contains("https://x/s/abc") && body.contains("2 days") && body.contains("download it once"));
        assert!(body.contains("given to you separately") && !body.contains("Password:"));
        assert!(body.contains("  Second opinion please") && body.ends_with("1 High Street\n"));
        inv.password = Some("ABCD-EFGH-JKLM");
        assert!(inv.body().contains("Password: ABCD-EFGH-JKLM"));
    }
}
