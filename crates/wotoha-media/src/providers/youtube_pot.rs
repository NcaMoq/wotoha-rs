use std::{
    env,
    path::PathBuf,
    process::Stdio,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    time::timeout,
};

const DEFAULT_PROVIDER_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_TOKEN_BYTES: usize = 16 * 1024;
const MAX_STDOUT_BYTES: usize = 64 * 1024;
const MAX_STDERR_BYTES: usize = 8 * 1024;

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PoTokenContext {
    Player,
    Gvs,
}

#[derive(Debug, Serialize)]
struct PoTokenRequest<'a> {
    protocol_version: u8,
    client: PoTokenClient<'a>,
    context: PoTokenContext,
    video_id: &'a str,
    visitor_data: Option<&'a str>,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct PoTokenClient<'a> {
    pub profile_id: &'a str,
    pub client_name: &'a str,
    pub client_version: &'a str,
    pub client_number: &'a str,
    pub user_agent: &'a str,
    pub os_name: &'a str,
    pub os_version: &'a str,
    pub device_make: Option<&'a str>,
    pub device_model: Option<&'a str>,
    pub android_sdk_version: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct PoTokenResponse {
    protocol_version: u8,
    token: Option<String>,
    expires_in_seconds: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct PoToken {
    pub value: String,
    pub expires_at: Instant,
    pub expires_at_unix: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum PoTokenError {
    #[error("failed to encode PO Token request: {0}")]
    Encode(serde_json::Error),
    #[error("failed to start PO Token provider {path}: {source}")]
    Start {
        path: String,
        source: std::io::Error,
    },
    #[error("failed to communicate with PO Token provider: {0}")]
    Io(std::io::Error),
    #[error("PO Token provider timed out after {0:?}")]
    Timeout(Duration),
    #[error("PO Token provider failed with status {status}: {message}")]
    Failed { status: String, message: String },
    #[error("PO Token provider output exceeded {MAX_STDOUT_BYTES} bytes")]
    OutputTooLarge,
    #[error("PO Token provider returned invalid JSON: {0}")]
    InvalidJson(serde_json::Error),
    #[error("PO Token provider protocol version {0} is not supported")]
    UnsupportedProtocol(u8),
    #[error("PO Token provider returned an invalid token")]
    InvalidToken,
}

pub fn is_configured() -> bool {
    provider_path().is_some()
}

pub async fn request_token(
    client: PoTokenClient<'_>,
    context: PoTokenContext,
    video_id: &str,
    visitor_data: Option<&str>,
) -> Result<Option<PoToken>, PoTokenError> {
    let Some(executable) = provider_path() else {
        return Ok(None);
    };
    let request = serde_json::to_vec(&PoTokenRequest {
        protocol_version: 1,
        client,
        context,
        video_id,
        visitor_data,
    })
    .map_err(PoTokenError::Encode)?;
    let timeout_duration = provider_timeout();

    let mut command = Command::new(&executable);
    command
        .env_clear()
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    copy_safe_environment(&mut command);
    let mut child = command.spawn().map_err(|source| PoTokenError::Start {
        path: executable.display().to_string(),
        source,
    })?;
    let mut stdin = child.stdin.take().ok_or_else(|| {
        PoTokenError::Io(std::io::Error::other(
            "PO Token provider stdin was unavailable",
        ))
    })?;
    let mut stdout = child.stdout.take().ok_or_else(|| {
        PoTokenError::Io(std::io::Error::other(
            "PO Token provider stdout was unavailable",
        ))
    })?;
    let mut stderr = child.stderr.take().ok_or_else(|| {
        PoTokenError::Io(std::io::Error::other(
            "PO Token provider stderr was unavailable",
        ))
    })?;
    let operation = async {
        stdin.write_all(&request).await?;
        stdin.shutdown().await?;
        drop(stdin);
        let (stdout, stderr, status) = tokio::try_join!(
            read_limited(&mut stdout, MAX_STDOUT_BYTES),
            read_limited(&mut stderr, MAX_STDERR_BYTES),
            child.wait(),
        )?;
        Ok::<_, std::io::Error>((status, stdout, stderr))
    };
    let (status, stdout, stderr) = match timeout(timeout_duration, operation).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => return Err(PoTokenError::Io(error)),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(PoTokenError::Timeout(timeout_duration));
        }
    };
    if stdout.truncated {
        return Err(PoTokenError::OutputTooLarge);
    }
    if !status.success() {
        return Err(PoTokenError::Failed {
            status: status
                .code()
                .map_or_else(|| "signal".to_owned(), |code| code.to_string()),
            message: concise_stderr(&stderr.bytes),
        });
    }

    parse_response(&stdout.bytes)
}

fn parse_response(output: &[u8]) -> Result<Option<PoToken>, PoTokenError> {
    let response: PoTokenResponse =
        serde_json::from_slice(output).map_err(PoTokenError::InvalidJson)?;
    if response.protocol_version != 1 {
        return Err(PoTokenError::UnsupportedProtocol(response.protocol_version));
    }
    let Some(token) = response.token else {
        return Ok(None);
    };
    if token.is_empty() || token.len() > MAX_TOKEN_BYTES || token.chars().any(char::is_whitespace) {
        return Err(PoTokenError::InvalidToken);
    }
    let ttl = response
        .expires_in_seconds
        .unwrap_or(10 * 60)
        .min(12 * 60 * 60);
    let expires_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .saturating_add(ttl);
    Ok(Some(PoToken {
        value: token,
        expires_at: Instant::now() + Duration::from_secs(ttl),
        expires_at_unix,
    }))
}

fn provider_path() -> Option<PathBuf> {
    env::var_os("WOTOHA_YOUTUBE_PO_TOKEN_PROVIDER")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

fn provider_timeout() -> Duration {
    env::var("WOTOHA_YOUTUBE_PO_TOKEN_TIMEOUT_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| (1..=60).contains(seconds))
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_PROVIDER_TIMEOUT)
}

#[derive(Debug)]
struct LimitedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

async fn read_limited(
    reader: &mut (impl AsyncRead + Unpin),
    limit: usize,
) -> std::io::Result<LimitedOutput> {
    let mut bytes = Vec::with_capacity(limit.min(8 * 1024));
    let mut buffer = [0_u8; 8 * 1024];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(bytes.len());
        let stored = remaining.min(read);
        bytes.extend_from_slice(&buffer[..stored]);
        truncated |= stored < read;
    }
    Ok(LimitedOutput { bytes, truncated })
}

fn copy_safe_environment(command: &mut Command) {
    for key in [
        "PATH",
        "HOME",
        "TMPDIR",
        "TEMP",
        "TMP",
        "SYSTEMROOT",
        "WINDIR",
    ] {
        if let Some(value) = env::var_os(key) {
            command.env(key, value);
        }
    }
}

fn concise_stderr(stderr: &[u8]) -> String {
    const MAX_CHARS: usize = 500;
    let text = String::from_utf8_lossy(stderr);
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= MAX_CHARS {
        compact
    } else {
        format!("{}…", compact.chars().take(MAX_CHARS).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_valid_provider_response() {
        let token = parse_response(
            br#"{"protocol_version":1,"token":"test-token","expires_in_seconds":120}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(token.value, "test-token");
        assert!(token.expires_at > Instant::now());
    }

    #[test]
    fn accepts_provider_declining_a_token() {
        assert!(
            parse_response(br#"{"protocol_version":1,"token":null}"#)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn rejects_whitespace_in_tokens() {
        let error = parse_response(br#"{"protocol_version":1,"token":"not valid"}"#).unwrap_err();
        assert!(matches!(error, PoTokenError::InvalidToken));
    }

    #[test]
    fn rejects_unknown_provider_protocol() {
        let error = parse_response(br#"{"protocol_version":2,"token":null}"#).unwrap_err();
        assert!(matches!(error, PoTokenError::UnsupportedProtocol(2)));
    }

    #[test]
    fn serializes_versioned_provider_request() {
        let request = PoTokenRequest {
            protocol_version: 1,
            client: PoTokenClient {
                profile_id: "android_vr",
                client_name: "ANDROID_VR",
                client_version: "1.65.10",
                client_number: "28",
                user_agent: "test-agent",
                os_name: "Android",
                os_version: "12L",
                device_make: Some("Meta"),
                device_model: Some("Quest"),
                android_sdk_version: Some(32),
            },
            context: PoTokenContext::Gvs,
            video_id: "video-id",
            visitor_data: Some("visitor"),
        };
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            json!({
                "protocol_version": 1,
                "client": {
                    "profile_id": "android_vr",
                    "client_name": "ANDROID_VR",
                    "client_version": "1.65.10",
                    "client_number": "28",
                    "user_agent": "test-agent",
                    "os_name": "Android",
                    "os_version": "12L",
                    "device_make": "Meta",
                    "device_model": "Quest",
                    "android_sdk_version": 32
                },
                "context": "gvs",
                "video_id": "video-id",
                "visitor_data": "visitor"
            })
        );
    }

    #[test]
    fn does_not_extend_zero_ttl() {
        let before = Instant::now();
        let token =
            parse_response(br#"{"protocol_version":1,"token":"short","expires_in_seconds":0}"#)
                .unwrap()
                .unwrap();
        assert!(token.expires_at <= before + Duration::from_secs(1));
    }

    #[tokio::test]
    async fn caps_provider_output_while_draining_it() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        let write = tokio::spawn(async move {
            writer.write_all(b"0123456789").await.unwrap();
            writer.shutdown().await.unwrap();
        });
        let output = read_limited(&mut reader, 4).await.unwrap();
        write.await.unwrap();

        assert_eq!(output.bytes, b"0123");
        assert!(output.truncated);
    }
}
