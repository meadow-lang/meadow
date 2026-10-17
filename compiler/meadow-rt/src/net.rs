//! Sockets and TLS, for `Std.Net` and `Std.Tls`: what each runtime's `Net`
//! operations do, written once.
//!
//! A program holds a socket by a number, a handle into one table the whole
//! process shares -- a green thread may move between OS threads, and its
//! sockets with it. A listener answers `accept`, a stream `read` and `write`;
//! a TLS session is a stream that encrypts, made from a plain one, whose
//! handle it then takes over. Every operation answers `Err` with what went
//! wrong rather than failing the program: a peer that went away is ordinary.
//!
//! Each call blocks the OS thread that makes it, as `Fs`'s do; a read waits
//! for at least a byte, or for the timeout `set_timeout` gave the stream.
//!
//! TLS is rustls, with ring's cryptography. A client checks the server's
//! certificate against Mozilla's roots (`webpki-roots`), and against one more
//! authority where it is given one -- a service's own, inside a company or a
//! test. A server is given its certificate chain and key as PEM files.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{
    ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection, StreamOwned,
};

/// What a handle names.
enum Socket {
    Listener(TcpListener),
    Tcp(TcpStream),
    /// Behind a lock of its own: a TLS session's state changes as it is read
    /// and written, where a plain stream's lives in the kernel.
    Tls(Mutex<Session>),
}

enum Session {
    Client(StreamOwned<ClientConnection, TcpStream>),
    Server(StreamOwned<ServerConnection, TcpStream>),
}

impl Session {
    fn sock(&self) -> &TcpStream {
        match self {
            Session::Client(s) => &s.sock,
            Session::Server(s) => &s.sock,
        }
    }
}

fn table() -> MutexGuard<'static, HashMap<i64, Arc<Socket>>> {
    static TABLE: OnceLock<Mutex<HashMap<i64, Arc<Socket>>>> = OnceLock::new();
    TABLE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

fn insert(s: Socket) -> i64 {
    static NEXT: AtomicI64 = AtomicI64::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    table().insert(id, Arc::new(s));
    id
}

/// The socket `id` names, shared: the table's lock is not held while it is
/// used, so one thread's read does not hold up another's accept.
fn get(id: i64) -> Result<Arc<Socket>, String> {
    table()
        .get(&id)
        .cloned()
        .ok_or_else(|| format!("socket {id} is closed, or was never opened"))
}

/// What went wrong with an I/O call, said as a program would want it.
fn said(e: std::io::Error) -> String {
    match e.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => "timed out".to_string(),
        _ => e.to_string(),
    }
}

fn port_of(port: i64) -> Result<u16, String> {
    u16::try_from(port).map_err(|_| format!("{port} is not a port: one is from 0 to 65535"))
}

/// A listener on `host`'s `port`: `"0.0.0.0"` for every address, and port 0
/// for one the system picks (`local_port` says which).
pub fn listen(host: &str, port: i64) -> Result<i64, String> {
    let l = TcpListener::bind((host, port_of(port)?)).map_err(said)?;
    Ok(insert(Socket::Listener(l)))
}

/// The next connection to listener `id`, and where it came from.
pub fn accept(id: i64) -> Result<(i64, String), String> {
    match &*get(id)? {
        Socket::Listener(l) => {
            let (s, peer) = l.accept().map_err(said)?;
            let _ = s.set_nodelay(true);
            Ok((insert(Socket::Tcp(s)), peer.to_string()))
        }
        _ => Err(format!("socket {id} is a stream, not a listener")),
    }
}

/// A stream to `host`'s `port`, trying each address the name has.
pub fn connect(host: &str, port: i64) -> Result<i64, String> {
    let s = TcpStream::connect((host, port_of(port)?)).map_err(said)?;
    let _ = s.set_nodelay(true);
    Ok(insert(Socket::Tcp(s)))
}

/// Up to `max` bytes from stream `id`, waiting for at least one; none at
/// all is the end of the stream.
pub fn read(id: i64, max: i64) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; max.clamp(1, 1 << 20) as usize];
    let n = match &*get(id)? {
        Socket::Tcp(s) => {
            let mut r: &TcpStream = s;
            r.read(&mut buf).map_err(said)?
        }
        Socket::Tls(m) => {
            let mut g = m.lock().unwrap_or_else(|p| p.into_inner());
            let got = match &mut *g {
                Session::Client(s) => s.read(&mut buf),
                Session::Server(s) => s.read(&mut buf),
            };
            match got {
                Ok(n) => n,
                // A peer that closes without saying so, as many servers do
                // once a response is sent: the end of the stream all the same.
                Err(e) if e.kind() == ErrorKind::UnexpectedEof => 0,
                Err(e) => return Err(said(e)),
            }
        }
        Socket::Listener(_) => return Err(format!("socket {id} is a listener, not a stream")),
    };
    buf.truncate(n);
    Ok(buf)
}

/// All of `bytes` to stream `id`: how many there were.
pub fn write(id: i64, bytes: &[u8]) -> Result<i64, String> {
    match &*get(id)? {
        Socket::Tcp(s) => {
            let mut w: &TcpStream = s;
            w.write_all(bytes).map_err(said)?
        }
        Socket::Tls(m) => {
            let mut g = m.lock().unwrap_or_else(|p| p.into_inner());
            match &mut *g {
                Session::Client(s) => s.write_all(bytes).and_then(|_| s.flush()),
                Session::Server(s) => s.write_all(bytes).and_then(|_| s.flush()),
            }
            .map_err(said)?
        }
        Socket::Listener(_) => return Err(format!("socket {id} is a listener, not a stream")),
    }
    Ok(bytes.len() as i64)
}

/// Close socket `id`: a TLS session says so to its peer first. Closing one
/// already closed does nothing.
pub fn close(id: i64) {
    let Some(s) = table().remove(&id) else {
        return;
    };
    match &*s {
        Socket::Tcp(t) => {
            let _ = t.shutdown(Shutdown::Both);
        }
        Socket::Tls(m) => {
            let mut g = m.lock().unwrap_or_else(|p| p.into_inner());
            match &mut *g {
                Session::Client(s) => {
                    s.conn.send_close_notify();
                    let _ = s.flush();
                }
                Session::Server(s) => {
                    s.conn.send_close_notify();
                    let _ = s.flush();
                }
            }
            let _ = g.sock().shutdown(Shutdown::Both);
        }
        Socket::Listener(_) => {}
    }
}

/// How long a read or write of stream `id` waits before it fails as timed
/// out, in milliseconds: 0 or less for as long as it takes.
pub fn set_timeout(id: i64, millis: i64) -> Result<(), String> {
    let t = (millis > 0).then(|| Duration::from_millis(millis as u64));
    let set = |s: &TcpStream| {
        s.set_read_timeout(t)
            .and_then(|_| s.set_write_timeout(t))
            .map_err(said)
    };
    match &*get(id)? {
        Socket::Tcp(s) => set(s),
        Socket::Tls(m) => set(m.lock().unwrap_or_else(|p| p.into_inner()).sock()),
        Socket::Listener(_) => Err(format!("socket {id} is a listener, not a stream")),
    }
}

/// The port socket `id` is on, here: what port 0 was given.
pub fn local_port(id: i64) -> Result<i64, String> {
    let addr = match &*get(id)? {
        Socket::Listener(l) => l.local_addr(),
        Socket::Tcp(s) => s.local_addr(),
        Socket::Tls(m) => m
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .sock()
            .local_addr(),
    };
    Ok(i64::from(addr.map_err(said)?.port()))
}

/// The addresses `host` names, each once, in the order the system gives.
pub fn resolve(host: &str) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for a in (host, 0).to_socket_addrs().map_err(said)? {
        let ip = a.ip().to_string();
        if !out.contains(&ip) {
            out.push(ip);
        }
    }
    Ok(out)
}

// --- TLS ----------------------------------------------------------------------

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// The plain stream `id` takes, to wrap: it must be no one else's, mid-call.
fn take_plain(id: i64) -> Result<TcpStream, String> {
    let mut t = table();
    let s = t
        .remove(&id)
        .ok_or_else(|| format!("socket {id} is closed, or was never opened"))?;
    match Arc::try_unwrap(s) {
        Ok(Socket::Tcp(s)) => Ok(s),
        Ok(other) => {
            t.insert(id, Arc::new(other));
            Err(format!("socket {id} is not a plain stream"))
        }
        Err(shared) => {
            t.insert(id, shared);
            Err(format!("socket {id} is being read or written"))
        }
    }
}

/// A client's configuration: Mozilla's roots, and `extra`'s authorities
/// where it names a PEM file. Made once for each `extra`.
fn client_config(extra: &str) -> Result<Arc<ClientConfig>, String> {
    static MADE: OnceLock<Mutex<HashMap<String, Arc<ClientConfig>>>> = OnceLock::new();
    let made = MADE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(c) = made.lock().unwrap_or_else(|p| p.into_inner()).get(extra) {
        return Ok(c.clone());
    }
    let mut roots = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    if !extra.is_empty() {
        for cert in CertificateDer::pem_file_iter(extra).map_err(|e| format!("{extra}: {e}"))? {
            let cert = cert.map_err(|e| format!("{extra}: {e}"))?;
            roots.add(cert).map_err(|e| format!("{extra}: {e}"))?;
        }
    }
    let config = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let config = Arc::new(config);
    made.lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(extra.to_string(), config.clone());
    Ok(config)
}

/// A server's configuration, from its certificate chain and key in PEM
/// files. Made once for each pair.
fn server_config(cert: &str, key: &str) -> Result<Arc<ServerConfig>, String> {
    type Made = Mutex<HashMap<(String, String), Arc<ServerConfig>>>;
    static MADE: OnceLock<Made> = OnceLock::new();
    let made = MADE.get_or_init(|| Mutex::new(HashMap::new()));
    let k = (cert.to_string(), key.to_string());
    if let Some(c) = made.lock().unwrap_or_else(|p| p.into_inner()).get(&k) {
        return Ok(c.clone());
    }
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .map_err(|e| format!("{cert}: {e}"))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{cert}: {e}"))?;
    let secret = PrivateKeyDer::from_pem_file(key).map_err(|e| format!("{key}: {e}"))?;
    let config = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(chain, secret)
        .map_err(|e| e.to_string())?;
    let config = Arc::new(config);
    made.lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(k, config.clone());
    Ok(config)
}

/// Stream `id` as the client of a TLS session with `server`, checked against
/// Mozilla's roots and `extra`'s, where it names a PEM file. The handshake is
/// done before this answers, so a certificate that does not check is said
/// here. The session takes `id`'s place; the answer is its handle.
pub fn tls_connect(id: i64, server: &str, extra: &str) -> Result<i64, String> {
    let config = client_config(extra)?;
    let name = ServerName::try_from(server.to_string())
        .map_err(|_| format!("`{server}` is not a name a certificate can be for"))?;
    let mut sock = take_plain(id)?;
    let mut conn = ClientConnection::new(config, name).map_err(|e| e.to_string())?;
    while conn.is_handshaking() {
        conn.complete_io(&mut sock).map_err(said)?;
    }
    Ok(insert(Socket::Tls(Mutex::new(Session::Client(
        StreamOwned::new(conn, sock),
    )))))
}

/// Stream `id` as the server of a TLS session, with the certificate chain
/// and key in PEM files `cert` and `key`. As `tls_connect`, it shakes hands
/// first and takes `id`'s place.
pub fn tls_accept(id: i64, cert: &str, key: &str) -> Result<i64, String> {
    let config = server_config(cert, key)?;
    let mut sock = take_plain(id)?;
    let mut conn = ServerConnection::new(config).map_err(|e| e.to_string())?;
    while conn.is_handshaking() {
        conn.complete_io(&mut sock).map_err(said)?;
    }
    Ok(insert(Socket::Tls(Mutex::new(Session::Server(
        StreamOwned::new(conn, sock),
    )))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stream_reads_what_its_peer_wrote_and_then_its_end() {
        let l = listen("127.0.0.1", 0).unwrap();
        let port = local_port(l).unwrap();
        let client = std::thread::spawn(move || {
            let c = connect("127.0.0.1", port).unwrap();
            write(c, b"hello").unwrap();
            close(c);
        });
        let (s, peer) = accept(l).unwrap();
        assert!(peer.starts_with("127.0.0.1:"), "{peer}");
        let mut got = Vec::new();
        loop {
            let b = read(s, 3).unwrap();
            if b.is_empty() {
                break;
            }
            got.extend(b);
        }
        client.join().unwrap();
        assert_eq!(got, b"hello");
        close(s);
        close(l);
        assert!(read(s, 1).unwrap_err().contains("closed"));
    }

    #[test]
    fn a_read_with_nothing_coming_times_out() {
        let l = listen("127.0.0.1", 0).unwrap();
        let c = connect("127.0.0.1", local_port(l).unwrap()).unwrap();
        let (s, _) = accept(l).unwrap();
        set_timeout(s, 50).unwrap();
        assert_eq!(read(s, 8).unwrap_err(), "timed out");
        for id in [c, s, l] {
            close(id);
        }
    }

    #[test]
    fn a_name_resolves_and_a_bad_port_is_said() {
        assert!(
            resolve("localhost")
                .unwrap()
                .iter()
                .any(|a| a == "127.0.0.1" || a == "::1")
        );
        assert!(
            listen("127.0.0.1", 70000)
                .unwrap_err()
                .contains("not a port")
        );
    }

    #[test]
    fn tls_carries_a_conversation_both_ways() {
        let dir = std::env::temp_dir().join(format!("meadow-tls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let (cp, kp) = (dir.join("cert.pem"), dir.join("key.pem"));
        std::fs::write(&cp, cert.cert.pem()).unwrap();
        std::fs::write(&kp, cert.key_pair.serialize_pem()).unwrap();
        let (cp, kp) = (cp.display().to_string(), kp.display().to_string());
        let l = listen("127.0.0.1", 0).unwrap();
        let port = local_port(l).unwrap();
        let ca = cp.clone();
        let client = std::thread::spawn(move || {
            let c = connect("127.0.0.1", port).unwrap();
            // Not one of Mozilla's: refused, unless it is trusted as well.
            assert!(tls_connect(c, "localhost", "").is_err());
            let c = connect("127.0.0.1", port).unwrap();
            let t = tls_connect(c, "localhost", &ca).unwrap();
            write(t, b"ping").unwrap();
            let back = read(t, 16).unwrap();
            close(t);
            back
        });
        // The refused one.
        let (first, _) = accept(l).unwrap();
        assert!(tls_accept(first, &cp, &kp).is_err());
        let (s, _) = accept(l).unwrap();
        let t = tls_accept(s, &cp, &kp).unwrap();
        assert_eq!(read(t, 16).unwrap(), b"ping");
        write(t, b"pong").unwrap();
        assert_eq!(client.join().unwrap(), b"pong");
        assert!(read(t, 16).unwrap().is_empty());
        close(t);
        close(l);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
