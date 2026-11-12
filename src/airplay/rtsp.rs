//! A small blocking RTSP client for the AirPlay control connection, with the
//! optional encrypted framing that kicks in once pairing is done.

use super::cipher::FramedCipher;
use anyhow::{anyhow, bail, Context, Result};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

pub struct Response {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn plist(&self) -> Result<plist::Value> {
        plist::from_bytes(&self.body).context("response body is not a plist")
    }
}

pub struct RtspClient {
    stream: TcpStream,
    cseq: u32,
    cipher: Option<FramedCipher>,
    raw: Vec<u8>,
    plain: Vec<u8>,
    common_headers: Vec<(String, String)>,
}

impl RtspClient {
    pub fn connect(addr: SocketAddr, common_headers: Vec<(String, String)>) -> Result<Self> {
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
            .with_context(|| format!("could not connect to {addr}"))?;
        stream.set_nodelay(true).ok();
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        Ok(Self {
            stream,
            cseq: 0,
            cipher: None,
            raw: Vec::new(),
            plain: Vec::new(),
            common_headers,
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.stream.local_addr()?)
    }

    pub fn enable_encryption(&mut self, cipher: FramedCipher) {
        self.cipher = Some(cipher);
    }

    pub fn request(
        &mut self,
        method: &str,
        uri: &str,
        content_type: Option<&str>,
        body: &[u8],
        extra_headers: &[(&str, &str)],
    ) -> Result<Response> {
        self.cseq += 1;
        let mut head = format!("{method} {uri} RTSP/1.0\r\nCSeq: {}\r\n", self.cseq);
        for (k, v) in &self.common_headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        for (k, v) in extra_headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        if let Some(ct) = content_type {
            head.push_str(&format!("Content-Type: {ct}\r\n"));
        }
        head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));

        let mut msg = head.into_bytes();
        msg.extend_from_slice(body);

        let wire = match self.cipher.as_mut() {
            Some(c) => c.encrypt(&msg)?,
            None => msg,
        };
        self.stream
            .write_all(&wire)
            .with_context(|| format!("sending {method} {uri}"))?;

        self.read_response()
            .with_context(|| format!("reading response to {method} {uri}"))
    }

    fn fill(&mut self) -> Result<()> {
        let mut buf = [0u8; 8192];
        let n = self.stream.read(&mut buf)?;
        if n == 0 {
            bail!("connection closed by device");
        }
        match self.cipher.as_mut() {
            Some(c) => {
                self.raw.extend_from_slice(&buf[..n]);
                c.decrypt_available(&mut self.raw, &mut self.plain)?;
            }
            None => self.plain.extend_from_slice(&buf[..n]),
        }
        Ok(())
    }

    fn read_response(&mut self) -> Result<Response> {
        let header_end = loop {
            if let Some(pos) = find(&self.plain, b"\r\n\r\n") {
                break pos;
            }
            self.fill()?;
        };
        let head = String::from_utf8_lossy(&self.plain[..header_end]).to_string();
        let mut lines = head.split("\r\n");
        let status_line = lines.next().ok_or_else(|| anyhow!("empty response"))?;
        let mut parts = status_line.splitn(3, ' ');
        let _proto = parts.next();
        let status: u16 = parts
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| anyhow!("bad status line: {status_line}"))?;
        let reason = parts.next().unwrap_or("").to_string();
        let headers: Vec<(String, String)> = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .collect();
        let content_length: usize = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("Content-Length"))
            .and_then(|(_, v)| v.parse().ok())
            .unwrap_or(0);

        let body_start = header_end + 4;
        while self.plain.len() < body_start + content_length {
            self.fill()?;
        }
        let body = self.plain[body_start..body_start + content_length].to_vec();
        self.plain.drain(..body_start + content_length);

        Ok(Response {
            status,
            reason,
            headers,
            body,
        })
    }
}

pub fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
