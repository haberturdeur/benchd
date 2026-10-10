//! Compatibility is checked before any application message or USB/IP bytes.
//! Increment PROTOCOL_VERSION for incompatible wire or behavioral changes.
//! Package versions and build identifiers are diagnostic, not admission rules.

use std::io;

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;
pub const BUILD_ID: &str = env!("BENCHD_BUILD_ID");

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    // A tagged struct only emits its tag when serializing; this field also
    // validates the message kind when deserializing.
    #[serde(rename = "msg")]
    kind: HelloKind,
    pub protocol: u32,
    pub version: String,
    pub build: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HelloKind {
    Hello,
}

impl Hello {
    pub fn current() -> Self {
        Self {
            kind: HelloKind::Hello,
            protocol: PROTOCOL_VERSION,
            version: env!("CARGO_PKG_VERSION").into(),
            build: BUILD_ID.into(),
        }
    }

    pub fn check(&self, peer: &Self) -> io::Result<()> {
        if self.protocol != peer.protocol {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!(
                "benchd protocol mismatch: local {self}; peer {peer}. Upgrade the incompatible deployment"
            )));
        }
        Ok(())
    }
}

impl std::fmt::Display for Hello {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} (protocol {}, build {})",
            self.version, self.protocol, self.build
        )
    }
}

pub fn version_string() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| Hello::current().to_string())
}

#[cfg(feature = "transport")]
pub use transport::{accept, connect};

#[cfg(feature = "transport")]
mod transport {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    const TIMEOUT: Duration = Duration::from_secs(5);
    const MAX_HELLO: usize = 4096;

    /// The caller retains the unbuffered stream. Reading exactly one line is
    /// essential: the next byte may be an application message or USB/IP data.
    pub async fn connect<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S) -> io::Result<Hello> {
        deadline(async {
            let local = Hello::current();
            send(stream, &local).await?;
            let line = read_line(stream).await?;
            let peer = parse_hello(&line)?;
            local.check(&peer)?;
            Ok(peer)
        })
        .await
    }

    /// Reject peers before they can register hardware or acquire any lease.
    pub async fn accept<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S) -> io::Result<Hello> {
        deadline(async {
            let line = read_line(stream).await?;
            let peer = match parse_hello(&line) {
                Ok(peer) => peer,
                Err(error) => {
                    // Older hosts understand `rejected`, clients/operators
                    // understand `error`. No application request is executed.
                    let legacy: serde_json::Value = serde_json::from_slice(&line).unwrap_or_default();
                    let error = format!("benchd protocol handshake required; local {}. Please upgrade the peer. {error}", Hello::current());
                    let reply = if legacy["msg"] == "register" {
                        serde_json::json!({"msg": "rejected", "reason": error})
                    } else {
                        serde_json::json!({"msg": "error", "request": legacy["request"].as_u64().unwrap_or(0), "error": error, "retryable": false})
                    };
                    send(stream, &reply).await?;
                    return Err(io::Error::new(io::ErrorKind::InvalidData, error));
                }
            };
            let local = Hello::current();
            // Send our metadata even on mismatch so both ends diagnose it.
            send(stream, &local).await?;
            local.check(&peer)?;
            Ok(peer)
        }).await
    }

    async fn deadline<F: std::future::Future<Output = io::Result<Hello>>>(
        operation: F,
    ) -> io::Result<Hello> {
        tokio::time::timeout(TIMEOUT, operation).await.map_err(|_| io::Error::new(
            io::ErrorKind::TimedOut,
            format!("benchd protocol handshake timed out after {}s; local {}. Check that the peer is running a compatible benchd", TIMEOUT.as_secs(), Hello::current()),
        ))?
    }

    async fn send<S: AsyncWrite + Unpin, T: Serialize>(
        stream: &mut S,
        value: &T,
    ) -> io::Result<()> {
        let mut line = serde_json::to_vec(value)?;
        line.push(b'\n');
        stream.write_all(&line).await?;
        stream.flush().await
    }

    async fn read_line<S: AsyncRead + Unpin>(stream: &mut S) -> io::Result<Vec<u8>> {
        let mut line = Vec::new();
        for _ in 0..MAX_HELLO {
            let byte = stream.read_u8().await.map_err(|err| {
                io::Error::new(
                    err.kind(),
                    format!(
                        "benchd protocol handshake could not be read: {err}; local {}",
                        Hello::current()
                    ),
                )
            })?;
            if byte == b'\n' {
                return Ok(line);
            }
            line.push(byte);
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "benchd protocol hello is too long",
        ))
    }

    fn parse_hello(line: &[u8]) -> io::Result<Hello> {
        serde_json::from_slice(line).map_err(|err| {
            let value: serde_json::Value = serde_json::from_slice(line).unwrap_or_default();
            let detail = value["error"].as_str().or_else(|| value["reason"].as_str()).unwrap_or("missing or malformed hello");
            io::Error::new(io::ErrorKind::InvalidData, format!(
                "peer did not send a valid benchd protocol handshake ({detail}: {err}); local {}. Check or upgrade the peer", Hello::current()
            ))
        })
    }
}
