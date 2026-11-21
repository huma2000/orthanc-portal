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

use crate::settings::{Language, Settings, SmtpSecurity};
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

/// One email address of an account ("" = none): the plain address, checked.
pub fn single_address(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(String::new());
    }
    match parse_recipients(raw)?.as_slice() {
        [one] => Ok(one.email.to_string()),
        _ => Err("Enter one email address".into()),
    }
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
    duration_words_in(minutes, Language::En)
}

/// [`duration_words`] in a language: "2 días 5 horas 20 minutos".
pub fn duration_words_in(minutes: u64, lang: Language) -> String {
    let (d, h, m) = (minutes / 1440, (minutes % 1440) / 60, minutes % 60);
    let words: [(&str, &str); 3] = match lang {
        Language::En => [("day", "days"), ("hour", "hours"), ("minute", "minutes")],
        Language::Es => [("día", "días"), ("hora", "horas"), ("minuto", "minutos")],
    };
    let part = |n: u64, (one, many): (&str, &str)| match n {
        0 => None,
        1 => Some(format!("1 {one}")),
        n => Some(format!("{n} {many}")),
    };
    let parts: Vec<String> = [part(d, words[0]), part(h, words[1]), part(m, words[2])].into_iter().flatten().collect();
    if !parts.is_empty() {
        parts.join(" ")
    } else if lang == Language::Es {
        "menos de un minuto".into()
    } else {
        "less than a minute".into()
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
    /// The language of the email (Settings > Appearance > Language).
    pub language: Language,
}

/// The sentences of an invitation email, in one language. {portal}, {sender}, {duration}, {n} and
/// {password} are filled in.
struct Words {
    subject_updated: &'static str,
    subject_new: &'static str,
    hello: &'static str,
    updated: &'static str,
    shared: &'static str,
    message_from: &'static str,
    open_here: &'static str,
    works_for: &'static str,
    view_only: &'static str,
    download_once: &'static str,
    download_times: &'static str,
    password: &'static str,
    password_separately: &'static str,
    sent_by: &'static str,
}

const ENGLISH: Words = Words {
    subject_updated: "{portal}: your link to a shared study was updated",
    subject_new: "{portal}: an imaging study has been shared with you",
    hello: "Hello,",
    updated: "{sender} at {portal} has updated your link to a shared imaging study.",
    shared: "{sender} at {portal} has shared an imaging study with you.",
    message_from: "Message from {sender}:",
    open_here: "Open the study here:",
    works_for: "The link works for {duration} from the time of this email.",
    view_only: "You can look at the study online (downloading it is not allowed).",
    download_once: "You can look at the study online and download it once.",
    download_times: "You can look at the study online and download it {n} times.",
    password: "Password: {password}\n(capital letters, spaces and dashes do not matter)",
    password_separately:
        "You need a password to open it. It will be given to you separately (for example by phone or text message).",
    sent_by: "This email was sent by {portal}. If you were not expecting it, you can ignore it.",
};

const SPANISH: Words = Words {
    subject_updated: "{portal}: se ha actualizado su enlace a un estudio compartido",
    subject_new: "{portal}: se ha compartido con usted un estudio de imagen",
    hello: "Hola:",
    updated: "{sender}, de {portal}, ha actualizado su enlace a un estudio de imagen compartido.",
    shared: "{sender}, de {portal}, ha compartido con usted un estudio de imagen.",
    message_from: "Mensaje de {sender}:",
    open_here: "Abra el estudio aquí:",
    works_for: "El enlace funciona durante {duration} a partir del envío de este correo.",
    view_only: "Puede ver el estudio en línea (no se permite descargarlo).",
    download_once: "Puede ver el estudio en línea y descargarlo una vez.",
    download_times: "Puede ver el estudio en línea y descargarlo {n} veces.",
    password: "Contraseña: {password}\n(no importan las mayúsculas, los espacios ni los guiones)",
    password_separately:
        "Necesita una contraseña para abrirlo. Se le facilitará por separado (por ejemplo, por teléfono o por SMS).",
    sent_by: "Este correo lo ha enviado {portal}. Si no lo esperaba, puede ignorarlo.",
};

impl Invitation<'_> {
    fn words(&self) -> &'static Words {
        match self.language {
            Language::En => &ENGLISH,
            Language::Es => &SPANISH,
        }
    }

    /// A sentence with its {portal} and {sender} filled in.
    fn fill(&self, text: &str) -> String {
        text.replace("{portal}", self.portal_name).replace("{sender}", self.sender)
    }

    pub fn subject(&self) -> String {
        let w = self.words();
        self.fill(if self.updated { w.subject_updated } else { w.subject_new })
    }

    pub fn body(&self) -> String {
        let w = self.words();
        let mut text = format!("{}\n\n", w.hello);
        text += &self.fill(if self.updated { w.updated } else { w.shared });
        text += "\n";
        if !self.message.is_empty() {
            text += &format!("\n{}\n", self.fill(w.message_from));
            for line in self.message.lines() {
                text += &format!("  {line}\n");
            }
        }
        text += &format!("\n{}\n{}\n\n", w.open_here, self.url);
        text += &w.works_for.replace("{duration}", &duration_words_in(self.minutes_left, self.language));
        text += "\n";
        text += &match self.downloads_left {
            0 => w.view_only.to_string(),
            1 => w.download_once.to_string(),
            n => w.download_times.replace("{n}", &n.to_string()),
        };
        text += "\n\n";
        text += &match self.password {
            Some(p) => w.password.replace("{password}", p),
            None => w.password_separately.to_string(),
        };
        text += &format!("\n\n{}\n", self.fill(w.sent_by));
        if !self.footer.is_empty() {
            text += &format!("\n--\n{}\n", self.footer);
        }
        text
    }
}

/// What happened to a link, for the email to its creator ([`link_notice`]).
#[derive(Clone, Copy)]
pub enum LinkEvent {
    /// The guest entered the password for the first time.
    Opened,
    /// The guest downloaded a study; `left` downloads remain.
    Downloaded { left: i64 },
    /// Files arrived through an upload link (`files` so far, `megabytes` so far).
    Received { files: i64, megabytes: i64 },
}

/// The email telling staff what happened to one of their links: (subject, body). On purpose it
/// names the link by its number and the creator's own note, never the patient: emails travel
/// through mail servers the practice does not control.
pub fn link_notice(portal: &str, link: &str, note: &str, event: LinkEvent, lang: Language) -> (String, String) {
    let note_line = |label: &str| if note.is_empty() { String::new() } else { format!("{label}: {note}\n") };
    match lang {
        Language::En => {
            let (subject, what) = match event {
                LinkEvent::Opened => (
                    format!("{portal}: your shared link {link} was opened"),
                    "The person you invited entered the password and opened the study.".to_string(),
                ),
                LinkEvent::Downloaded { left } => (
                    format!("{portal}: your shared link {link} was downloaded"),
                    format!("The person you invited downloaded the study. Downloads left: {left}."),
                ),
                LinkEvent::Received { files, megabytes } => (
                    format!("{portal}: files received through your upload link {link}"),
                    format!("Files are arriving through your upload link: {files} so far ({megabytes} MB). They are in the archive (Shares > Upload links shows the studies)."),
                ),
            };
            (
                subject,
                format!(
                    "{what}\n\nLink: {link}\n{}\nYou asked {portal} to tell you (Shares page). This email names no patient on purpose.\n",
                    note_line("Your note")
                ),
            )
        }
        Language::Es => {
            let (subject, what) = match event {
                LinkEvent::Opened => (
                    format!("{portal}: se ha abierto su enlace compartido {link}"),
                    "La persona invitada ha introducido la contraseña y ha abierto el estudio.".to_string(),
                ),
                LinkEvent::Downloaded { left } => (
                    format!("{portal}: se ha descargado su enlace compartido {link}"),
                    format!("La persona invitada ha descargado el estudio. Descargas restantes: {left}."),
                ),
                LinkEvent::Received { files, megabytes } => (
                    format!("{portal}: archivos recibidos por su enlace de envío {link}"),
                    format!("Están llegando archivos por su enlace de envío: {files} hasta ahora ({megabytes} MB). Ya están en el archivo (Compartidos > Enlaces de envío muestra los estudios)."),
                ),
            };
            (
                subject,
                format!(
                    "{what}\n\nEnlace: {link}\n{}\nPidió a {portal} que le avisara (página Compartidos). Este correo no nombra a ningún paciente a propósito.\n",
                    note_line("Su nota")
                ),
            )
        }
    }
}

/// An email asking somebody outside to SEND studies through an upload link: (subject, body).
pub struct UploadInvitation<'a> {
    pub portal_name: &'a str,
    pub sender: &'a str,
    pub url: &'a str,
    pub minutes_left: u64,
    pub max_mb: i64,
    pub password: Option<&'a str>,
    pub message: &'a str,
    pub footer: &'a str,
    pub language: Language,
}

impl UploadInvitation<'_> {
    pub fn subject(&self) -> String {
        match self.language {
            Language::En => format!("{}: please send us your images", self.portal_name),
            Language::Es => format!("{}: envíenos sus imágenes, por favor", self.portal_name),
        }
    }

    pub fn body(&self) -> String {
        let (p, s) = (self.portal_name, self.sender);
        let duration = duration_words_in(self.minutes_left, self.language);
        let message = if self.message.is_empty() {
            String::new()
        } else {
            let lines: String = self.message.lines().map(|l| format!("  {l}\n")).collect();
            match self.language {
                Language::En => format!("\nMessage from {s}:\n{lines}"),
                Language::Es => format!("\nMensaje de {s}:\n{lines}"),
            }
        };
        let mut text = match self.language {
            Language::En => format!(
                "Hello,\n\n{s} at {p} asks you to send imaging studies (for example the files of a CD or USB stick with earlier images).\n{message}\nOpen this page and drag the files or the whole folder onto it:\n{}\n\nThe link works for {duration} from the time of this email, for up to {} MB. You will not see anything else of the archive.\n\n",
                self.url, self.max_mb
            ),
            Language::Es => format!(
                "Hola:\n\n{s}, de {p}, le pide que envíe estudios de imagen (por ejemplo, los archivos de un CD o de una memoria USB con imágenes anteriores).\n{message}\nAbra esta página y arrastre a ella los archivos o la carpeta entera:\n{}\n\nEl enlace funciona durante {duration} a partir del envío de este correo, hasta {} MB. No verá nada más del archivo.\n\n",
                self.url, self.max_mb
            ),
        };
        text += &match (self.password, self.language) {
            (Some(pw), Language::En) => format!("Password: {pw}\n(capital letters, spaces and dashes do not matter)"),
            (Some(pw), Language::Es) => {
                format!("Contraseña: {pw}\n(no importan las mayúsculas, los espacios ni los guiones)")
            }
            (None, Language::En) => "You need a password to open it. It will be given to you separately (for example by phone or text message).".into(),
            (None, Language::Es) => "Necesita una contraseña para abrirlo. Se le facilitará por separado (por ejemplo, por teléfono o por SMS).".into(),
        };
        text += &match self.language {
            Language::En => {
                format!("\n\nThis email was sent by {p}. If you were not expecting it, you can ignore it.\n")
            }
            Language::Es => format!("\n\nEste correo lo ha enviado {p}. Si no lo esperaba, puede ignorarlo.\n"),
        };
        if !self.footer.is_empty() {
            text += &format!("\n--\n{}\n", self.footer);
        }
        text
    }
}

/// Sends a notice in the background: the person who triggered it (a guest) never waits for the
/// mail server, and a failure only goes to the log.
pub fn send_in_background(settings: std::sync::Arc<Settings>, to: String, subject: String, body: String) {
    tokio::spawn(async move {
        let Ok(mailbox) = to.parse::<Mailbox>() else { return };
        if let Err(e) = send(&settings, vec![mailbox], &subject, &body).await {
            tracing::warn!("a link notification could not be emailed: {e}");
        }
    });
}

/// What a rule's email says about the study. Never the patient: emails travel through mail
/// servers the practice does not control.
pub struct RuleNotice<'a> {
    pub portal: &'a str,
    pub rule: &'a str,
    pub modalities: &'a [String],
    pub institution: &'a str,
    pub description: &'a str,
    /// `YYYYMMDD` as stored in DICOM.
    pub date: &'a str,
    pub accession: &'a str,
    pub calling_aet: &'a str,
    pub labels: &'a [String],
    /// The study in the staff portal, when its address is known (Settings > `public_base_url`).
    pub link: Option<String>,
    pub lang: Language,
}

impl RuleNotice<'_> {
    /// (subject, body).
    pub fn email(&self) -> (String, String) {
        // DICOM texts go in the subject too: no line breaks or other control characters.
        let clean = |s: &str| s.chars().filter(|c| !c.is_control()).collect::<String>().trim().to_string();
        let date = match self.date.len() {
            8 if self.date.is_ascii() => format!("{}-{}-{}", &self.date[..4], &self.date[4..6], &self.date[6..]),
            _ => clean(self.date),
        };
        let modalities = self.modalities.join(", ");
        let es = matches!(self.lang, Language::Es);
        let rows = [
            (if es { "Modalidades" } else { "Modalities" }, modalities.clone()),
            (if es { "Centro" } else { "Institution" }, clean(self.institution)),
            (if es { "Descripción" } else { "Description" }, clean(self.description)),
            (if es { "Fecha del estudio" } else { "Study date" }, date),
            (if es { "Número de acceso" } else { "Accession number" }, clean(self.accession)),
            (if es { "Enviado por" } else { "Sent by" }, clean(self.calling_aet)),
            (if es { "Etiquetas" } else { "Labels" }, self.labels.join(", ")),
        ];
        let facts: String = rows.iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| format!("{k}: {v}\n")).collect();
        let what: Vec<String> = [modalities, clean(self.institution)].into_iter().filter(|s| !s.is_empty()).collect();
        let what = if what.is_empty() { String::new() } else { format!(" ({})", what.join(", ")) };
        let (rule, portal) = (clean(self.rule), self.portal);
        if es {
            let open = match &self.link {
                Some(link) => format!("Ábralo en el portal: {link}"),
                None => "Búsquelo en el portal (por ejemplo por su número de acceso).".to_string(),
            };
            (
                format!("{portal}: nuevo estudio para la regla «{rule}»{what}"),
                format!(
                    "Ha llegado un estudio que cumple la regla «{rule}».\n\n{facts}\n{open}\n\nRecibe este correo porque un administrador añadió su dirección a la regla «{rule}» (Ajustes > Reenvío automático). Este correo no nombra al paciente a propósito.\n"
                ),
            )
        } else {
            let open = match &self.link {
                Some(link) => format!("Open it in the portal: {link}"),
                None => "Find it in the portal (by its accession number, for example).".to_string(),
            };
            (
                format!("{portal}: new study for the rule \"{rule}\"{what}"),
                format!(
                    "A study that matches the rule \"{rule}\" has arrived.\n\n{facts}\n{open}\n\nYou receive this email because an administrator added your address to the rule \"{rule}\" (Settings > Auto-forward). This email names no patient on purpose.\n"
                ),
            )
        }
    }
}

/// The test email of Settings > Email: (subject, body).
pub fn test_email(portal: &str, admin: &str, lang: Language) -> (String, String) {
    match lang {
        Language::En => (
            format!("{portal}: test email"),
            format!(
                "This is a test email from {portal}.\n\nIt was sent by {admin} from Settings > Email. If you received it, the portal can send share invitations by email.\n"
            ),
        ),
        Language::Es => (
            format!("{portal}: correo de prueba"),
            format!(
                "Este es un correo de prueba de {portal}.\n\nLo ha enviado {admin} desde Ajustes > Correo electrónico. Si lo ha recibido, el portal puede enviar invitaciones por correo.\n"
            ),
        ),
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
            language: Language::En,
        };
        let body = inv.body();
        assert!(body.contains("https://x/s/abc") && body.contains("2 days") && body.contains("download it once"));
        assert!(body.contains("given to you separately") && !body.contains("Password:"));
        assert!(body.contains("  Second opinion please") && body.ends_with("1 High Street\n"));
        inv.password = Some("ABCD-EFGH-JKLM");
        assert!(inv.body().contains("Password: ABCD-EFGH-JKLM"));
    }

    #[test]
    fn invitations_can_be_in_spanish() {
        let inv = Invitation {
            portal_name: "Demo",
            sender: "doctor",
            url: "https://x/s/abc",
            minutes_left: 1500,
            downloads_left: 3,
            password: None,
            message: "",
            updated: false,
            footer: "",
            language: Language::Es,
        };
        assert_eq!(inv.subject(), "Demo: se ha compartido con usted un estudio de imagen");
        let body = inv.body();
        assert!(body.starts_with("Hola:\n\ndoctor, de Demo, ha compartido"), "{body}");
        assert!(body.contains("durante 1 día 1 hora a partir") && body.contains("descargarlo 3 veces"), "{body}");
        assert!(body.contains("Se le facilitará por separado") && !body.contains("Contraseña:"), "{body}");
        assert_eq!(duration_words_in(2, Language::Es), "2 minutos");
        assert_eq!(duration_words_in(0, Language::Es), "menos de un minuto");
    }
}
