//! GitHub App authentication: JWT generation and installation tokens.
//!
//! Used to clone private repos as the GitHub App (not as a user).
//! Uses `openssl` on the host to sign the JWT (RS256), avoiding a
//! heavy JWT/crypto dependency in the binary.

use anyhow::{Context, Result};

/// Generate an installation access token for a GitHub App.
///
/// 1. Build a JWT signed with the App's private key (RS256).
/// 2. Exchange it for an installation token via the GitHub API.
pub async fn get_installation_token(
    app_id: &str,
    private_key_path: &str,
    api_base: &str,
) -> Result<String> {
    let jwt = build_app_jwt(app_id, private_key_path).await?;

    // Find the installation ID for this app
    let client = reqwest::Client::new();
    let installs: serde_json::Value = client
        .get(format!("{}/app/installations", api_base))
        .header("Authorization", format!("Bearer {}", jwt))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "groose-agent")
        .send()
        .await?
        .json()
        .await?;

    let installation_id = installs
        .as_array()
        .and_then(|a| a.first())
        .and_then(|i| i["id"].as_u64())
        .context("no installations found for GitHub App")?;

    // Exchange JWT for installation access token
    let token_resp: serde_json::Value = client
        .post(format!(
            "{}/app/installations/{}/access_tokens",
            api_base, installation_id
        ))
        .header("Authorization", format!("Bearer {}", jwt))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "groose-agent")
        .json(&serde_json::json!({}))
        .send()
        .await?
        .json()
        .await?;

    let token = token_resp["token"]
        .as_str()
        .context("no token in installation token response")?
        .to_string();

    Ok(token)
}

/// Build a GitHub App JWT (RS256) using openssl on the host.
async fn build_app_jwt(app_id: &str, private_key_path: &str) -> Result<String> {
    let now = chrono::Utc::now().timestamp();
    let header = base64url(r#"{"alg":"RS256","typ":"JWT"}"#.as_bytes());
    let payload = base64url(
        serde_json::json!({
            "iat": now - 60,
            "exp": now + 600,
            "iss": app_id,
        })
        .to_string()
        .as_bytes(),
    );

    let signing_input = format!("{}.{}", header, payload);

    // Sign with openssl (RS256 = RSASSA-PKCS1-v1_5 with SHA-256)
    let mut child = tokio::process::Command::new("openssl")
        .args(["dgst", "-sha256", "-sign", private_key_path])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn openssl")?;

    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().context("openssl stdin unavailable")?;
        stdin.write_all(signing_input.as_bytes()).await?;
    }

    let output = child
        .wait_with_output()
        .await
        .context("openssl signing failed")?;

    if !output.status.success() {
        anyhow::bail!(
            "openssl signing failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let signature = base64url(&output.stdout);
    Ok(format!("{}.{}", signing_input, signature))
}

fn base64url(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}
