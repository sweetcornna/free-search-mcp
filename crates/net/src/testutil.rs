//! Test-only local servers (raw HTTP/1.1 over tokio): they record every
//! request head byte for byte (header order and case), count connections and
//! report whether a streamed body was aborted by the client.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// One received request.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    pub method: String,
    /// Request target as sent (`/path?q` or, through a proxy, absolute-form).
    pub target: String,
    /// Headers in wire order, names as sent.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// What the handler answers.
#[derive(Debug, Clone)]
pub enum Reply {
    /// Status, headers, body; `Content-Length` is added unless present.
    Full(u16, Vec<(String, String)>, Vec<u8>),
    /// Hold the head back `delay`, then stream `chunks` × `chunk` bytes of
    /// `x` every `every`; `length` says whether `Content-Length` is sent.
    Slow {
        delay: Duration,
        chunk: usize,
        chunks: usize,
        every: Duration,
        length: bool,
    },
    /// Stream raw custom headers, an initial `prefix`, then `chunks` × `chunk`
    /// bytes of `fill` every `every`.
    SlowCustom {
        headers: Vec<(String, String)>,
        prefix: Vec<u8>,
        fill: u8,
        chunk: usize,
        chunks: usize,
        every: Duration,
    },
}

pub fn ok(body: &str) -> Reply {
    Reply::Full(
        200,
        vec![("Content-Type".into(), "text/plain".into())],
        body.as_bytes().to_vec(),
    )
}

pub fn redirect(status: u16, location: &str) -> Reply {
    Reply::Full(
        status,
        vec![("Location".into(), location.into())],
        Vec::new(),
    )
}

type Handler = Arc<dyn Fn(&Seen) -> Reply + Send + Sync>;

/// A running server.
#[derive(Clone)]
pub struct Server {
    pub addr: SocketAddr,
    pub conns: Arc<AtomicUsize>,
    pub seen: Arc<Mutex<Vec<Seen>>>,
    /// `"aborted"` / `"complete"` per streamed body.
    pub ended: Arc<Mutex<Vec<&'static str>>>,
}

impl Server {
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    pub fn connections(&self) -> usize {
        self.conns.load(Ordering::SeqCst)
    }

    pub fn requests(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    pub async fn how_it_ended(&self) -> Option<&'static str> {
        for _ in 0..100 {
            if let Some(e) = self.ended.lock().unwrap().last().copied() {
                return Some(e);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        None
    }
}

pub async fn serve(handler: impl Fn(&Seen) -> Reply + Send + Sync + 'static) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = Server {
        addr: listener.local_addr().unwrap(),
        conns: Arc::new(AtomicUsize::new(0)),
        seen: Arc::new(Mutex::new(Vec::new())),
        ended: Arc::new(Mutex::new(Vec::new())),
    };
    let handler: Handler = Arc::new(handler);
    let s = server.clone();
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                return;
            };
            s.conns.fetch_add(1, Ordering::SeqCst);
            let (s, h) = (s.clone(), handler.clone());
            tokio::spawn(async move {
                let _ = handle(sock, s, h).await;
            });
        }
    });
    server
}

async fn read_head(sock: &mut TcpStream) -> std::io::Result<Option<(Seen, Vec<u8>)>> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..i]).into_owned();
            let rest = buf[i + 4..].to_vec();
            let mut lines = head.split("\r\n");
            let mut first = lines.next().unwrap_or("").split(' ');
            let method = first.next().unwrap_or("").to_owned();
            let target = first.next().unwrap_or("").to_owned();
            let headers = lines
                .filter_map(|l| {
                    l.split_once(':')
                        .map(|(k, v)| (k.to_owned(), v.trim().to_owned()))
                })
                .collect();
            return Ok(Some((
                Seen {
                    method,
                    target,
                    headers,
                    body: Vec::new(),
                },
                rest,
            )));
        }
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

async fn handle(mut sock: TcpStream, s: Server, h: Handler) -> std::io::Result<()> {
    let Some((mut seen, mut rest)) = read_head(&mut sock).await? else {
        return Ok(());
    };
    let len: usize = seen
        .header("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    while rest.len() < len {
        let mut tmp = [0u8; 4096];
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        rest.extend_from_slice(&tmp[..n]);
    }
    rest.truncate(len);
    seen.body = rest;
    s.seen.lock().unwrap().push(seen.clone());
    match h(&seen) {
        Reply::Full(status, headers, body) => {
            let mut out = format!("HTTP/1.1 {status} X\r\nConnection: close\r\n");
            if !headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            {
                out.push_str(&format!("Content-Length: {}\r\n", body.len()));
            }
            for (k, v) in &headers {
                out.push_str(&format!("{k}: {v}\r\n"));
            }
            out.push_str("\r\n");
            sock.write_all(out.as_bytes()).await?;
            sock.write_all(&body).await?;
            sock.shutdown().await?;
        }
        Reply::Slow {
            delay,
            chunk,
            chunks,
            every,
            length,
        } => {
            tokio::time::sleep(delay).await;
            let mut out = String::from("HTTP/1.1 200 OK\r\nConnection: close\r\n");
            if length {
                out.push_str(&format!("Content-Length: {}\r\n", chunk * chunks));
            }
            out.push_str("\r\n");
            let r: std::io::Result<()> = async {
                sock.write_all(out.as_bytes()).await?;
                let data = vec![b'x'; chunk];
                for _ in 0..chunks {
                    sock.write_all(&data).await?;
                    sock.flush().await?;
                    tokio::time::sleep(every).await;
                }
                Ok(())
            }
            .await;
            s.ended
                .lock()
                .unwrap()
                .push(if r.is_ok() { "complete" } else { "aborted" });
        }
        Reply::SlowCustom {
            headers,
            prefix,
            fill,
            chunk,
            chunks,
            every,
        } => {
            let mut out = String::from("HTTP/1.1 200 OK\r\nConnection: close\r\n");
            for (k, v) in &headers {
                out.push_str(&format!("{k}: {v}\r\n"));
            }
            out.push_str("\r\n");
            let r: std::io::Result<()> = async {
                sock.write_all(out.as_bytes()).await?;
                if !prefix.is_empty() {
                    sock.write_all(&prefix).await?;
                }
                sock.flush().await?;
                let data = vec![fill; chunk];
                for _ in 0..chunks {
                    sock.write_all(&data).await?;
                    sock.flush().await?;
                    tokio::time::sleep(every).await;
                }
                Ok(())
            }
            .await;
            s.ended
                .lock()
                .unwrap()
                .push(if r.is_ok() { "complete" } else { "aborted" });
        }
    }
    Ok(())
}

/// A minimal SOCKS5 TCP CONNECT proxy for offline proxy tests (RFC 1928, no-auth).
#[derive(Clone)]
pub struct Socks5Server {
    pub addr: SocketAddr,
    pub conns: Arc<AtomicUsize>,
    pub targets: Arc<Mutex<Vec<(u8, String, u16)>>>,
}

impl Socks5Server {
    pub fn connections(&self) -> usize {
        self.conns.load(Ordering::SeqCst)
    }

    pub fn targets(&self) -> Vec<(u8, String, u16)> {
        self.targets.lock().unwrap().clone()
    }
}

pub async fn serve_socks5() -> Socks5Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let srv = Socks5Server {
        addr: listener.local_addr().unwrap(),
        conns: Arc::new(AtomicUsize::new(0)),
        targets: Arc::new(Mutex::new(Vec::new())),
    };
    let s = srv.clone();
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                return;
            };
            s.conns.fetch_add(1, Ordering::SeqCst);
            let s = s.clone();
            tokio::spawn(async move {
                let _ = handle_socks5(sock, s).await;
            });
        }
    });
    srv
}

async fn handle_socks5(mut client: TcpStream, srv: Socks5Server) -> std::io::Result<()> {
    let mut hdr = [0u8; 2];
    client.read_exact(&mut hdr).await?;
    if hdr[0] != 0x05 {
        return Ok(());
    }
    let mut methods = vec![0u8; usize::from(hdr[1])];
    client.read_exact(&mut methods).await?;
    // Reply: version 5, no auth (0x00)
    client.write_all(&[0x05, 0x00]).await?;

    let mut req = [0u8; 4];
    client.read_exact(&mut req).await?;
    if req[0] != 0x05 || req[1] != 0x01 {
        return Ok(());
    }
    let atyp = req[3];
    let host = match atyp {
        0x01 => {
            let mut ip = [0u8; 4];
            client.read_exact(&mut ip).await?;
            std::net::Ipv4Addr::from(ip).to_string()
        }
        0x03 => {
            let mut len = [0u8; 1];
            client.read_exact(&mut len).await?;
            let mut domain = vec![0u8; usize::from(len[0])];
            client.read_exact(&mut domain).await?;
            String::from_utf8_lossy(&domain).into_owned()
        }
        0x04 => {
            let mut ip = [0u8; 16];
            client.read_exact(&mut ip).await?;
            std::net::Ipv6Addr::from(ip).to_string()
        }
        _ => return Ok(()),
    };
    let mut port_b = [0u8; 2];
    client.read_exact(&mut port_b).await?;
    let port = u16::from_be_bytes(port_b);
    srv.targets.lock().unwrap().push((atyp, host.clone(), port));

    let Ok(mut upstream) = TcpStream::connect((host.as_str(), port)).await else {
        let _ = client
            .write_all(&[0x05, 0x05, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await;
        return Ok(());
    };
    client
        .write_all(&[0x05, 0x00, 0x00, 0x01, 127, 0, 0, 1, 0, 0])
        .await?;
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    Ok(())
}
