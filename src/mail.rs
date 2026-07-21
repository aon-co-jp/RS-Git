//! SMTP経由でのOTPメール送信。[`open-easy-web`]の`server/src/mail.rs`と
//! 同じ設計(`lettre`の同期SMTPクライアントを`spawn_blocking`でオフロード)。
//!
//! [`open-easy-web`]: https://github.com/aon-co-jp/open-easy-web

use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};

#[derive(Debug, Clone)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub from: String,
}

impl SmtpConfig {
    pub fn from_env() -> Option<Self> {
        Some(Self {
            host: std::env::var("RGIT_SMTP_HOST").ok()?,
            port: std::env::var("RGIT_SMTP_PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(587),
            username: std::env::var("RGIT_SMTP_USERNAME").ok()?,
            password: std::env::var("RGIT_SMTP_PASSWORD").ok()?,
            from: std::env::var("RGIT_SMTP_FROM").ok()?,
        })
    }
}

#[derive(Debug)]
pub enum MailError {
    Build(String),
    Send(String),
}

impl std::fmt::Display for MailError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MailError::Build(e) => write!(f, "メール作成失敗: {e}"),
            MailError::Send(e) => write!(f, "メール送信失敗: {e}"),
        }
    }
}

fn build_and_send(config: &SmtpConfig, to: &str, code: &str) -> Result<(), MailError> {
    let body = format!(
        "RGit ログイン用ワンタイムパスワード\n\n\
         コード: {code}\n\
         このコードは5分間有効です。\n\n\
         心当たりがない場合はこのメールを無視してください。"
    );

    let email = Message::builder()
        .from(config.from.parse().map_err(|e| MailError::Build(format!("{e}")))?)
        .to(to.parse().map_err(|e| MailError::Build(format!("{e}")))?)
        .subject("RGit ログインコード")
        .header(ContentType::TEXT_PLAIN)
        .body(body)
        .map_err(|e| MailError::Build(format!("{e}")))?;

    let creds = Credentials::new(config.username.clone(), config.password.clone());
    let mailer =
        SmtpTransport::starttls_relay(&config.host).map_err(|e| MailError::Send(format!("{e}")))?.port(config.port).credentials(creds).build();

    mailer.send(&email).map_err(|e| MailError::Send(format!("{e}")))?;
    Ok(())
}

pub async fn send_otp(config: SmtpConfig, to: String, code: String) -> Result<(), MailError> {
    tokio::task::spawn_blocking(move || build_and_send(&config, &to, &code)).await.map_err(|e| MailError::Send(format!("task panicked: {e}")))?
}
