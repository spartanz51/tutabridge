//! Standalone live test of the new Rust SDK 2FA login flow.
//!
//! Does NOT touch the keyring / saved session. Exercises:
//!   initiate_session -> authenticate_with_second_factor_totp
//!   -> is_second_factor_pending -> login
//!
//! Run with:
//!   TUTA_EMAIL=you@tuta.io TUTA_PASSWORD='...' cargo run -p tutabridge-core --example test_2fa
//! It will prompt for the TOTP code on stdin.

use std::io::{BufRead, Write};
use std::sync::Arc;
use std::time::Duration;

use tutasdk::bindings::rest_client::RestClient;
use tutasdk::bindings::test_file_client::TestFileClient;
use tutasdk::folder_system::MailSetKind;
use tutasdk::net::native_rest_client::NativeRestClient;
use tutasdk::tutanota_constants::SecondFactorType;
use tutasdk::Sdk;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let email = std::env::var("TUTA_EMAIL").unwrap_or_else(|_| "mck1@tuta.io".to_string());
    let api_url =
        std::env::var("TUTA_API_URL").unwrap_or_else(|_| "https://app.tuta.com".to_string());
    let password = match std::env::var("TUTA_PASSWORD") {
        Ok(p) => p,
        Err(_) => rpassword::prompt_password(format!("Password for {email}: "))?,
    };

    let rest_client: Arc<dyn RestClient> = Arc::new(NativeRestClient::try_new()?);
    let file_client = Arc::new(TestFileClient::default());
    let sdk = Sdk::new(api_url, rest_client, file_client);

    println!("==> initiate_session for {email}");
    let session = sdk.initiate_session(&email, &password).await?;
    let access_token = session.credentials.access_token.clone();
    println!(
        "    got credentials, {} pending challenge(s)",
        session.challenges.len()
    );

    if !session.challenges.is_empty() {
        for c in &session.challenges {
            println!("    challenge: type={} id={:?}", c.r#type, c._id);
        }
        let totp_type = i64::from(SecondFactorType::Totp);
        if !session.challenges.iter().any(|c| c.r#type == totp_type) {
            return Err("account has no TOTP factor (only TOTP supported by this test)".into());
        }

        print!("TOTP code: ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        let code: u32 = line.trim().parse().map_err(|_| "invalid TOTP code")?;

        println!("==> authenticate_with_second_factor_totp");
        sdk.authenticate_with_second_factor_totp(&access_token, code)
            .await?;

        println!("==> polling is_second_factor_pending");
        let mut cleared = false;
        for i in 0..30 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let pending = sdk.is_second_factor_pending(&access_token).await?;
            println!("    poll {i}: pending={pending}");
            if !pending {
                cleared = true;
                break;
            }
        }
        if !cleared {
            return Err("2FA still pending after 30s".into());
        }
    }

    println!("==> login");
    let logged_in = sdk.login(session.credentials).await?;

    println!("==> verifying: load folders");
    let mailbox = logged_in.mail_facade().load_user_mailbox().await?;
    let folders = logged_in
        .mail_facade()
        .load_folders_for_mailbox(&mailbox)
        .await?;
    let inbox = folders
        .system_folder_by_type(MailSetKind::Inbox)
        .ok_or("no inbox folder found after login")?;
    println!("    OK — logged in, inbox folder id={:?}", inbox._id);

    println!("\nSUCCESS: full 2FA login flow worked end-to-end.");
    Ok(())
}
