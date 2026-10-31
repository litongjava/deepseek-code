//! Transport for direct Remote Control between dcl instances, without a
//! server. A random token is the pre-shared key of a Noise NNpsk0 handshake,
//! so traffic is encrypted and both sides prove they know the token without
//! sending it. The channel carries JSON frames of any size.
use super::FRAME_LIMIT;
use anyhow::{bail, ensure, Context, Result};
use serde_json::Value;
use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{tcp::OwnedWriteHalf, TcpStream},
    sync::mpsc,
};

pub const DEFAULT_PORT: u16 = 30086;
const NOISE: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
const PROLOGUE: &[u8] = b"dcl-remote-control/1";
const NOISE_MAX: usize = 65535;
const TAG: usize = 16;
/// Plaintext per Noise message: one flag byte, then data.
const CHUNK: usize = NOISE_MAX - TAG - 1;
const TOKEN_BYTES: usize = 20;
const BASE32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
pub const KEEP_ALIVE: Duration = Duration::from_secs(30);
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// The shared secret a client needs to connect. Printed once by the host.
#[derive(Clone, PartialEq, Eq)]
pub struct Token([u8; TOKEN_BYTES]);

impl Token {
    pub fn generate() -> Result<Self> {
        use ring::rand::SecureRandom;
        let mut bytes = [0; TOKEN_BYTES];
        ring::rand::SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| anyhow::anyhow!("Could not generate a Remote Control token"))?;
        Ok(Self(bytes))
    }

    /// Accepts the printed form; dashes, spaces and case are ignored.
    pub fn parse(text: &str) -> Result<Self> {
        let mut bits = 0u32;
        let mut count = 0;
        let mut bytes = Vec::with_capacity(TOKEN_BYTES);
        for c in text.chars().filter(|c| *c != '-' && !c.is_whitespace()) {
            let value = BASE32
                .iter()
                .position(|b| *b as char == c.to_ascii_lowercase())
                .context("Invalid Remote Control token")?;
            bits = (bits << 5) | value as u32;
            count += 5;
            if count >= 8 {
                count -= 8;
                bytes.push((bits >> count) as u8);
                bits &= (1 << count) - 1;
            }
        }
        ensure!(
            bytes.len() == TOKEN_BYTES && bits == 0,
            "Invalid Remote Control token"
        );
        Ok(Self(bytes.try_into().unwrap()))
    }

    fn psk(&self) -> [u8; 32] {
        let mut context = ring::digest::Context::new(&ring::digest::SHA256);
        context.update(b"dcl-remote-control-psk-v1");
        context.update(&self.0);
        context.finish().as_ref().try_into().unwrap()
    }
}

impl std::fmt::Display for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut text = String::new();
        for (index, chunk) in self.0.chunks(5).enumerate() {
            let bits = chunk.iter().fold(0u64, |acc, b| (acc << 8) | *b as u64);
            for shift in (0..8).rev() {
                text.push(BASE32[((bits >> (shift * 5)) & 31) as usize] as char);
                if shift == 4 {
                    text.push('-');
                }
            }
            if index < TOKEN_BYTES / 5 - 1 {
                text.push('-');
            }
        }
        f.write_str(&text)
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

/// An authenticated, encrypted connection carrying JSON frames.
pub struct Channel {
    transport: snow::TransportState,
    writer: OwnedWriteHalf,
    frames: mpsc::Receiver<std::io::Result<Vec<u8>>>,
    reader: tokio::task::JoinHandle<()>,
    partial: Vec<u8>,
    pub peer: SocketAddr,
}

impl Drop for Channel {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

async fn read_frame(reader: &mut (impl AsyncReadExt + Unpin)) -> std::io::Result<Vec<u8>> {
    let length = reader.read_u16().await? as usize;
    let mut frame = vec![0; length];
    reader.read_exact(&mut frame).await?;
    Ok(frame)
}

async fn write_frame(writer: &mut (impl AsyncWriteExt + Unpin), frame: &[u8]) -> Result<()> {
    writer.write_u16(frame.len() as u16).await?;
    writer.write_all(frame).await?;
    Ok(())
}

/// Runs the Noise handshake; a wrong token fails here on both sides.
pub async fn handshake(mut stream: TcpStream, token: &Token, initiator: bool) -> Result<Channel> {
    let peer = stream.peer_addr()?;
    stream.set_nodelay(true)?;
    let psk = token.psk();
    let builder = snow::Builder::new(NOISE.parse()?)
        .psk(0, &psk)?
        .prologue(PROLOGUE)?;
    let mut buffer = vec![0; NOISE_MAX];
    let state = if initiator {
        let mut state = builder.build_initiator()?;
        let length = state.write_message(&[], &mut buffer)?;
        write_frame(&mut stream, &buffer[..length]).await?;
        let reply = read_frame(&mut stream).await.context(
            "The host closed the connection: wrong token, or not a dcl Remote Control listener",
        )?;
        state
            .read_message(&reply, &mut buffer)
            .context("Could not authenticate the host")?;
        state
    } else {
        let mut state = builder.build_responder()?;
        let hello = read_frame(&mut stream).await?;
        state
            .read_message(&hello, &mut buffer)
            .context("Client used a wrong token")?;
        let length = state.write_message(&[], &mut buffer)?;
        write_frame(&mut stream, &buffer[..length]).await?;
        state
    };
    let transport = state.into_transport_mode()?;
    let (mut read, writer) = stream.into_split();
    let (sender, frames) = mpsc::channel(64);
    // A dedicated reader keeps `recv` cancel-safe.
    let reader = tokio::spawn(async move {
        loop {
            match read_frame(&mut read).await {
                Ok(frame) => {
                    if sender.send(Ok(frame)).await.is_err() {
                        break;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(error) => {
                    let _ = sender.send(Err(error)).await;
                    break;
                }
            }
        }
    });
    Ok(Channel {
        transport,
        writer,
        frames,
        reader,
        partial: Vec::new(),
        peer,
    })
}

impl Channel {
    pub async fn send(&mut self, value: &Value) -> Result<()> {
        let data = serde_json::to_vec(value)?;
        ensure!(
            data.len() <= FRAME_LIMIT,
            "Remote Control frame is too large"
        );
        let mut chunks = data.chunks(CHUNK).peekable();
        let mut plain = Vec::with_capacity(CHUNK + 1);
        let mut cipher = vec![0; NOISE_MAX];
        loop {
            let chunk = chunks.next().unwrap_or_default();
            let last = chunks.peek().is_none();
            plain.clear();
            plain.push(last as u8);
            plain.extend_from_slice(chunk);
            let length = self.transport.write_message(&plain, &mut cipher)?;
            write_frame(&mut self.writer, &cipher[..length]).await?;
            if last {
                return Ok(());
            }
        }
    }

    /// Next frame, or `None` when the peer closed the connection. Cancel-safe.
    pub async fn recv(&mut self) -> Result<Option<Value>> {
        let mut plain = vec![0; NOISE_MAX];
        loop {
            let Some(frame) = self.frames.recv().await else {
                return Ok(None);
            };
            let length = self
                .transport
                .read_message(&frame?, &mut plain)
                .context("Remote Control message failed authentication")?;
            ensure!(length > 0, "Empty Remote Control message");
            self.partial.extend_from_slice(&plain[1..length]);
            ensure!(
                self.partial.len() <= FRAME_LIMIT,
                "Remote Control frame is too large"
            );
            if plain[0] == 1 {
                let data = std::mem::take(&mut self.partial);
                return Ok(Some(
                    serde_json::from_slice(&data).context("Invalid Remote Control frame")?,
                ));
            }
        }
    }
}

/// Resolves `HOST`, `HOST:PORT`, `IP` or `[IPv6]:PORT`.
pub async fn resolve(target: &str) -> Result<Vec<SocketAddr>> {
    let target = target.trim();
    ensure!(!target.is_empty(), "Remote Control needs a host address");
    if let Ok(address) = target.parse::<SocketAddr>() {
        return Ok(vec![address]);
    }
    let bare = target.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, DEFAULT_PORT)]);
    }
    let addresses: Vec<_> = if target.contains(':') {
        tokio::net::lookup_host(target).await.map(Iterator::collect)
    } else {
        tokio::net::lookup_host((target, DEFAULT_PORT))
            .await
            .map(Iterator::collect)
    }
    .with_context(|| format!("Cannot resolve {target}"))?;
    ensure!(!addresses.is_empty(), "Cannot resolve {target}");
    Ok(addresses)
}

/// Connects to a host as a client.
pub async fn connect(target: &str, token: &Token) -> Result<Channel> {
    let mut failure = None;
    for address in resolve(target).await? {
        let stream = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(address)).await
        {
            Ok(Ok(stream)) => stream,
            Ok(Err(error)) => {
                failure = Some(anyhow::anyhow!("Cannot connect to {address}: {error}"));
                continue;
            }
            Err(_) => {
                failure = Some(anyhow::anyhow!("Connecting to {address} timed out"));
                continue;
            }
        };
        return match tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake(stream, token, true)).await {
            Ok(result) => result,
            Err(_) => bail!("Authenticating with {address} timed out"),
        };
    }
    Err(failure.unwrap_or_else(|| anyhow::anyhow!("Cannot connect to {target}")))
}

/// The address other machines most likely use to reach this one.
pub fn reachable_ip(bound: IpAddr) -> IpAddr {
    if !bound.is_unspecified() {
        return bound;
    }
    // Connecting a UDP socket only selects a route; nothing is sent.
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|socket| {
            socket.connect("192.0.2.1:9")?;
            socket.local_addr()
        })
        .map(|address| address.ip())
        .unwrap_or(bound)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_round_trip_and_ignore_formatting() {
        let token = Token::generate().unwrap();
        let text = token.to_string();
        assert_eq!(text.len(), 32 + 7);
        assert_eq!(Token::parse(&text).unwrap(), token);
        assert_eq!(
            Token::parse(&text.to_uppercase().replace('-', " ")).unwrap(),
            token
        );
        assert!(Token::parse(&text[..text.len() - 1]).is_err());
        assert!(Token::parse("not a token").is_err());
        assert_eq!(format!("{token:?}"), "Token(<redacted>)");
    }

    #[tokio::test]
    async fn targets_default_to_the_peer_port() {
        assert_eq!(
            resolve("10.0.0.2").await.unwrap(),
            vec!["10.0.0.2:30086".parse().unwrap()]
        );
        assert_eq!(
            resolve("10.0.0.2:4000").await.unwrap(),
            vec!["10.0.0.2:4000".parse().unwrap()]
        );
        assert_eq!(
            resolve("[::1]").await.unwrap(),
            vec!["[::1]:30086".parse().unwrap()]
        );
        assert!(resolve(" ").await.is_err());
    }
}
