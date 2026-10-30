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
//! for at least a byte, or for the timeout `set_timeout` gave the stream. A
//! runtime with green threads makes the calls that may wait on OS threads of
//! their own, so that a green thread waiting on a socket holds up no worker.
//! Closing a socket wakes a call waiting on it, from whichever thread closes
//! it, and a run that ends stops the calls its threads are still waiting in
//! (`Waits`): a server's accepting thread must not keep a finished program
//! from exiting.
//!
//! TLS is rustls, with ring's cryptography. A client checks the server's
//! certificate against Mozilla's roots (`webpki-roots`), and against one more
//! authority where it is given one -- a service's own, inside a company or a
//! test. A server is given its certificate chain and key as PEM files.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{
    ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection, StreamOwned,
};

/// What a handle names, with what every call on it shares.
struct Entry {
    sock: Socket,
    /// Set by `close`, for a call still waiting on the socket: an `accept`
    /// woken by the connection closing makes, or a read woken by its slice.
    closed: AtomicBool,
    /// The stream's timeout, as `set_timeout` gave it.
    timeout: Mutex<Option<Duration>>,
    /// The thread it belongs to (see [`owning`]), or 0 for none: closed when
    /// that thread ends.
    owner: AtomicU64,
}

enum Socket {
    Listener(TcpListener),
    Tcp(TcpStream),
    Tls(Tls),
}

struct Tls {
    /// Behind a lock of its own: a TLS session's state changes as it is read
    /// and written, where a plain stream's lives in the kernel.
    session: Mutex<Session>,
    /// The session's socket again, for what must not wait for the lock: a
    /// read holds it for as long as it waits, and closing wakes that read.
    raw: TcpStream,
}

impl Tls {
    fn new(session: Session) -> Result<Tls, String> {
        let raw = session.sock().try_clone().map_err(said)?;
        Ok(Tls {
            session: Mutex::new(session),
            raw,
        })
    }

    fn session(&self) -> MutexGuard<'_, Session> {
        self.session.lock().unwrap_or_else(|p| p.into_inner())
    }
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

impl Entry {
    fn timeout(&self) -> Option<Duration> {
        *self.timeout.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// How long a read waits at a time, where shutting a socket down does not
/// wake a read waiting on it -- Windows. It then waits in slices, looking
/// between them for a `close` and for its own timeout.
const SLICE: Option<Duration> = if cfg!(windows) {
    Some(Duration::from_millis(250))
} else {
    None
};

/// A stream's read timeout, as the system is to keep it: its own, or a
/// slice where reads wait in slices.
fn read_timeout(own: Option<Duration>) -> Option<Duration> {
    match (SLICE, own) {
        (Some(slice), Some(t)) => Some(slice.min(t)),
        (Some(slice), None) => Some(slice),
        (None, own) => own,
    }
}

fn table() -> MutexGuard<'static, HashMap<i64, Arc<Entry>>> {
    static TABLE: OnceLock<Mutex<HashMap<i64, Arc<Entry>>>> = OnceLock::new();
    TABLE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

fn insert(sock: Socket, timeout: Option<Duration>) -> i64 {
    static NEXT: AtomicI64 = AtomicI64::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let entry = Entry {
        sock,
        closed: AtomicBool::new(false),
        timeout: Mutex::new(timeout),
        owner: AtomicU64::new(OWNER.with(Cell::get)),
    };
    table().insert(id, Arc::new(entry));
    id
}

/// A new stream's handle.
fn stream(s: TcpStream) -> Result<i64, String> {
    let _ = s.set_nodelay(true);
    s.set_read_timeout(read_timeout(None)).map_err(said)?;
    Ok(insert(Socket::Tcp(s), None))
}

/// The socket `id` names, shared: the table's lock is not held while it is
/// used, so one thread's read does not hold up another's accept.
fn get(id: i64) -> Result<Arc<Entry>, String> {
    table()
        .get(&id)
        .cloned()
        .ok_or_else(|| format!("socket {id} is closed, or was never opened"))
}

fn timed_out(e: &std::io::Error) -> bool {
    matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

/// What went wrong with an I/O call, said as a program would want it.
fn said(e: std::io::Error) -> String {
    if timed_out(&e) {
        "timed out".to_string()
    } else {
        e.to_string()
    }
}

/// `once` -- a read -- until it reads, or fails for some other reason than
/// a slice's end; or until `closed` is set, or the stream's own timeout
/// passes. Where reads do not wait in slices, it runs once.
fn patiently(
    closed: &AtomicBool,
    timeout: Option<Duration>,
    mut once: impl FnMut() -> std::io::Result<usize>,
) -> Result<usize, String> {
    let deadline = timeout.map(|t| Instant::now() + t);
    loop {
        match once() {
            Ok(n) => return Ok(n),
            Err(e) if SLICE.is_some() && timed_out(&e) => {
                if closed.load(Ordering::SeqCst) {
                    return Ok(0);
                }
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    return Err(said(e));
                }
            }
            Err(e) => return Err(said(e)),
        }
    }
}

// --- who a socket belongs to -------------------------------------------------
//
// A socket belongs to the green thread that opened it -- by `listen`,
// `accept`, `connect`, or a TLS handshake, whose stream takes the plain one's
// place -- and is closed when that thread ends, whichever way, after its
// releases have run: a thread shares nothing mutable, so what it holds and
// did not hand on is out of everyone's reach once it is over, and keeping the
// socket open would only keep its port and its peer waiting for the run to
// end. A socket handed on -- in a spawned function, a message or an answer,
// which the runtimes see as they copy it (`roles::Role::Listener`,
// `roles::Role::Stream`) -- belongs to the thread it arrives in; one put where
// every thread may read it, a `TVar` or a `Compact`, to nobody. See
// `docs/CANCELLATION.md`.

/// Meadow's constructors for a socket: `Std.Net`'s, each around its handle.
/// What the front end declares for [`crate::roles::Role::Listener`] and
/// [`crate::roles::Role::Stream`].
pub const LISTENER: &str = "Std.Net.Listener.Listener";
pub const STREAM: &str = "Std.Net.Stream.Stream";

thread_local! {
    /// Whose sockets this OS thread opens now: see [`owning`].
    static OWNER: Cell<u64> = const { Cell::new(0) };
}

/// A new owner, for a green thread: never 0, and never given twice in a
/// process, where many runs may be going at once.
pub fn new_owner() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// The calls each owner has under way, and the owners that ended while one
/// was: a thread woken from a socket wait by a cancel ends while the call goes
/// on, on its I/O thread, and what that call opens when it comes back -- an
/// accepted connection -- belongs to nobody alive. It is closed as the call
/// ends. Kept only while a call is under way, so it stays small.
#[derive(Default)]
struct Calls {
    under_way: HashMap<u64, u32>,
    ended: std::collections::HashSet<u64>,
}

fn calls() -> MutexGuard<'static, Calls> {
    static CALLS: OnceLock<Mutex<Calls>> = OnceLock::new();
    CALLS
        .get_or_init(|| Mutex::new(Calls::default()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// `f`, with what it opens belonging to `owner`: the call a green thread
/// makes, wherever the runtime makes it.
pub fn owning<T>(owner: u64, f: impl FnOnce() -> T) -> T {
    if owner != 0 {
        *calls().under_way.entry(owner).or_default() += 1;
    }
    let was = OWNER.with(|o| o.replace(owner));
    let out = f();
    OWNER.with(|o| o.set(was));
    if owner != 0 {
        let orphaned = {
            let mut c = calls();
            let n = c.under_way.get_mut(&owner).map(|n| {
                *n -= 1;
                *n
            });
            if n == Some(0) {
                c.under_way.remove(&owner);
            }
            n == Some(0) && c.ended.remove(&owner)
        };
        if orphaned {
            close_held(owner);
        }
    }
    out
}

/// Socket `id` belongs to `owner` now (0: to nobody), if it is open.
pub fn hand_on(id: i64, owner: u64) {
    if let Some(e) = table().get(&id) {
        e.owner.store(owner, Ordering::SeqCst);
    }
}

/// Close every socket `owner` holds: its thread has ended. A call of its
/// still under way closes what it opens as it ends.
pub fn close_owned(owner: u64) {
    if owner == 0 {
        return;
    }
    {
        let mut c = calls();
        if c.under_way.contains_key(&owner) {
            c.ended.insert(owner);
        }
    }
    close_held(owner);
}

fn close_held(owner: u64) {
    let ids: Vec<i64> = table()
        .iter()
        .filter(|(_, e)| e.owner.load(Ordering::SeqCst) == owner)
        .map(|(id, _)| *id)
        .collect();
    for id in ids {
        close(id);
    }
}

// --- waiting -----------------------------------------------------------------

/// The calls one run's threads are waiting in, so that the run can stop them
/// when it ends.
///
/// A runtime whose program ends with a thread still waiting -- a server's,
/// in `accept` -- has a worker it cannot join. Each worker joins the run's
/// `Waits` as it starts; a call it waits in is recorded with how to wake it,
/// and `stop` wakes them all and refuses any later one. A process that
/// simply exits at the end of `main` needs none of this.
#[derive(Default)]
pub struct Waits {
    inner: Mutex<WaitsInner>,
}

#[derive(Default)]
struct WaitsInner {
    stopped: bool,
    waiting: HashMap<ThreadId, Waker>,
}

/// How to wake a waiting call: close its handle; or, for a handshake, whose
/// stream is out of the table, shut its socket and say why.
enum Waker {
    Handle(i64),
    Shake(TcpStream, Arc<AtomicBool>),
}

thread_local! {
    static RUN: RefCell<Option<Arc<Waits>>> = const { RefCell::new(None) };
}

impl Waits {
    fn lock(&self) -> MutexGuard<'_, WaitsInner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Wake every call the run's threads wait in, and refuse any they make
    /// from now on.
    pub fn stop(&self) {
        let wakers: Vec<Waker> = {
            let mut g = self.lock();
            g.stopped = true;
            g.waiting.drain().map(|(_, w)| w).collect()
        };
        for w in wakers {
            match w {
                Waker::Handle(id) => close(id),
                Waker::Shake(s, stopped) => {
                    stopped.store(true, Ordering::SeqCst);
                    let _ = s.shutdown(Shutdown::Both);
                }
            }
        }
    }
}

/// This OS thread's calls are `run`'s, until `leave`.
pub fn join(run: &Arc<Waits>) {
    RUN.with(|r| *r.borrow_mut() = Some(run.clone()));
}

pub fn leave() {
    RUN.with(|r| *r.borrow_mut() = None);
}

/// A call being waited in, on a thread that joined a run: recorded until
/// it is dropped.
struct Waiting(Option<Arc<Waits>>);

impl Waiting {
    fn on(waker: impl FnOnce() -> Option<Waker>) -> Result<Waiting, String> {
        let Some(run) = RUN.with(|r| r.borrow().clone()) else {
            return Ok(Waiting(None));
        };
        let mut g = run.lock();
        if g.stopped {
            return Err("the program has ended".to_string());
        }
        if let Some(w) = waker() {
            g.waiting.insert(std::thread::current().id(), w);
        }
        drop(g);
        Ok(Waiting(Some(run)))
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        if let Some(run) = &self.0 {
            run.lock().waiting.remove(&std::thread::current().id());
        }
    }
}

// --- sockets -------------------------------------------------------------------

fn port_of(port: i64) -> Result<u16, String> {
    u16::try_from(port).map_err(|_| format!("{port} is not a port: one is from 0 to 65535"))
}

/// A listener on `host`'s `port`: `"0.0.0.0"` for every address, and port 0
/// for one the system picks (`local_port` says which).
pub fn listen(host: &str, port: i64) -> Result<i64, String> {
    let l = TcpListener::bind((host, port_of(port)?)).map_err(said)?;
    Ok(insert(Socket::Listener(l), None))
}

/// The next connection to listener `id`, and where it came from.
pub fn accept(id: i64) -> Result<(i64, String), String> {
    let e = get(id)?;
    let Socket::Listener(l) = &e.sock else {
        return Err(format!("socket {id} is a stream, not a listener"));
    };
    let _waiting = Waiting::on(|| Some(Waker::Handle(id)))?;
    let (s, peer) = l.accept().map_err(said)?;
    if e.closed.load(Ordering::SeqCst) {
        return Err(format!("socket {id} is closed"));
    }
    Ok((stream(s)?, peer.to_string()))
}

/// A stream to `host`'s `port`, trying each address the name has.
pub fn connect(host: &str, port: i64) -> Result<i64, String> {
    let _waiting = Waiting::on(|| None)?;
    let s = TcpStream::connect((host, port_of(port)?)).map_err(said)?;
    stream(s)
}

/// Up to `max` bytes from stream `id`, waiting for at least one; none at
/// all is the end of the stream.
pub fn read(id: i64, max: i64) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; max.clamp(1, 1 << 20) as usize];
    let e = get(id)?;
    let _waiting = Waiting::on(|| Some(Waker::Handle(id)))?;
    let n = match &e.sock {
        Socket::Tcp(s) => patiently(&e.closed, e.timeout(), || {
            let mut r: &TcpStream = s;
            r.read(&mut buf)
        })?,
        Socket::Tls(t) => {
            let mut g = t.session();
            patiently(&e.closed, e.timeout(), || {
                let got = match &mut *g {
                    Session::Client(s) => s.read(&mut buf),
                    Session::Server(s) => s.read(&mut buf),
                };
                match got {
                    // A peer that closes without saying so, as many servers
                    // do once a response is sent: the end all the same.
                    Err(e) if e.kind() == ErrorKind::UnexpectedEof => Ok(0),
                    got => got,
                }
            })?
        }
        Socket::Listener(_) => return Err(format!("socket {id} is a listener, not a stream")),
    };
    buf.truncate(n);
    Ok(buf)
}

/// All of `bytes` to stream `id`: how many there were.
pub fn write(id: i64, bytes: &[u8]) -> Result<i64, String> {
    let e = get(id)?;
    let _waiting = Waiting::on(|| Some(Waker::Handle(id)))?;
    match &e.sock {
        Socket::Tcp(s) => {
            let mut w: &TcpStream = s;
            w.write_all(bytes).map_err(said)?
        }
        Socket::Tls(t) => {
            let mut g = t.session();
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
/// already closed does nothing. A call another thread waits in on it ends:
/// a read as the stream's end, an `accept` as closed.
pub fn close(id: i64) {
    let Some(e) = table().remove(&id) else {
        return;
    };
    e.closed.store(true, Ordering::SeqCst);
    match &e.sock {
        Socket::Tcp(t) => {
            let _ = t.shutdown(Shutdown::Both);
        }
        Socket::Tls(t) => {
            // A session some thread is reading has nobody to say goodbye
            // for: shutting the socket under it is what wakes that thread.
            if let Ok(mut g) = t.session.try_lock() {
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
            }
            let _ = t.raw.shutdown(Shutdown::Both);
        }
        Socket::Listener(l) => {
            // Nothing interrupts an `accept` but a connection: make one.
            if let Ok(mut at) = l.local_addr() {
                if at.ip().is_unspecified() {
                    at.set_ip(match at.ip() {
                        IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
                        IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
                    });
                }
                let _ = TcpStream::connect_timeout(&at, Duration::from_millis(500));
            }
        }
    }
}

/// How long a read or write of stream `id` waits before it fails as timed
/// out, in milliseconds: 0 or less for as long as it takes.
pub fn set_timeout(id: i64, millis: i64) -> Result<(), String> {
    let t = (millis > 0).then(|| Duration::from_millis(millis as u64));
    let e = get(id)?;
    let s = match &e.sock {
        Socket::Tcp(s) => s,
        Socket::Tls(tls) => &tls.raw,
        Socket::Listener(_) => return Err(format!("socket {id} is a listener, not a stream")),
    };
    s.set_read_timeout(read_timeout(t))
        .and_then(|_| s.set_write_timeout(t))
        .map_err(said)?;
    *e.timeout.lock().unwrap_or_else(|p| p.into_inner()) = t;
    Ok(())
}

/// The port socket `id` is on, here: what port 0 was given.
pub fn local_port(id: i64) -> Result<i64, String> {
    let addr = match &get(id)?.sock {
        Socket::Listener(l) => l.local_addr(),
        Socket::Tcp(s) => s.local_addr(),
        Socket::Tls(t) => t.raw.local_addr(),
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

/// The plain stream `id` takes, to wrap, with its timeout: it must be no one
/// else's, mid-call.
fn take_plain(id: i64) -> Result<(TcpStream, Option<Duration>), String> {
    let mut t = table();
    let e = t
        .remove(&id)
        .ok_or_else(|| format!("socket {id} is closed, or was never opened"))?;
    match Arc::try_unwrap(e) {
        Ok(Entry {
            sock: Socket::Tcp(s),
            timeout,
            ..
        }) => Ok((s, timeout.into_inner().unwrap_or_else(|p| p.into_inner()))),
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
    let (mut sock, timeout) = take_plain(id)?;
    let mut conn = ClientConnection::new(config, name).map_err(|e| e.to_string())?;
    shake(&mut sock, &mut conn, timeout)?;
    let tls = Tls::new(Session::Client(StreamOwned::new(conn, sock)))?;
    Ok(insert(Socket::Tls(tls), timeout))
}

/// Stream `id` as the server of a TLS session, with the certificate chain
/// and key in PEM files `cert` and `key`. As `tls_connect`, it shakes hands
/// first and takes `id`'s place.
pub fn tls_accept(id: i64, cert: &str, key: &str) -> Result<i64, String> {
    let config = server_config(cert, key)?;
    let (mut sock, timeout) = take_plain(id)?;
    let mut conn = ServerConnection::new(config).map_err(|e| e.to_string())?;
    shake(&mut sock, &mut conn, timeout)?;
    let tls = Tls::new(Session::Server(StreamOwned::new(conn, sock)))?;
    Ok(insert(Socket::Tls(tls), timeout))
}

/// A handshake, to its end. The stream is out of the table while it goes
/// on, so a run that ends wakes it by its socket.
fn shake<D>(
    sock: &mut TcpStream,
    conn: &mut rustls::ConnectionCommon<D>,
    timeout: Option<Duration>,
) -> Result<(), String> {
    let stopped = Arc::new(AtomicBool::new(false));
    let _waiting = Waiting::on(|| {
        sock.try_clone()
            .ok()
            .map(|s| Waker::Shake(s, stopped.clone()))
    })?;
    while conn.is_handshaking() {
        patiently(&stopped, timeout, || {
            conn.complete_io(&mut *sock).map(|(r, w)| r + w + 1)
        })?;
        if stopped.load(Ordering::SeqCst) {
            return Err("the program has ended".to_string());
        }
    }
    Ok(())
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
        let t0 = Instant::now();
        assert_eq!(read(s, 8).unwrap_err(), "timed out");
        assert!(t0.elapsed() < Duration::from_secs(2));
        // Longer than a slice, where reads wait in slices.
        set_timeout(s, 600).unwrap();
        let t0 = Instant::now();
        assert_eq!(read(s, 8).unwrap_err(), "timed out");
        assert!(
            t0.elapsed() >= Duration::from_millis(550),
            "{:?}",
            t0.elapsed()
        );
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
        let dir = meadow_scratch::scratch!("meadow-tls-{}", std::process::id());
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

        // A session another thread is reading closes at once, all the same.
        let trusted = cp.clone();
        let client = std::thread::spawn(move || {
            let c = connect("127.0.0.1", port).unwrap();
            let t = tls_connect(c, "localhost", &trusted).unwrap();
            let reading = std::thread::spawn(move || read(t, 16));
            std::thread::sleep(Duration::from_millis(50));
            let t0 = Instant::now();
            close(t);
            let got = reading.join().unwrap();
            (got, t0.elapsed())
        });
        let (s, _) = accept(l).unwrap();
        let t2 = tls_accept(s, &cp, &kp).unwrap();
        let (got, took) = client.join().unwrap();
        assert!(got.map_or(true, |b| b.is_empty()));
        assert!(took < Duration::from_secs(2), "{took:?}");
        close(t2);
        close(t);
        close(l);
    }

    #[test]
    fn closing_wakes_a_call_another_thread_waits_in() {
        let l = listen("0.0.0.0", 0).unwrap();
        let accepting = std::thread::spawn(move || accept(l));
        std::thread::sleep(Duration::from_millis(50));
        close(l);
        assert!(accepting.join().unwrap().unwrap_err().contains("closed"));

        let l = listen("127.0.0.1", 0).unwrap();
        let c = connect("127.0.0.1", local_port(l).unwrap()).unwrap();
        let (s, _) = accept(l).unwrap();
        let reading = std::thread::spawn(move || read(s, 8));
        std::thread::sleep(Duration::from_millis(50));
        let t0 = Instant::now();
        close(s);
        assert!(reading.join().unwrap().map_or(true, |b| b.is_empty()));
        assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
        close(c);
        close(l);
    }

    #[test]
    fn a_run_that_stops_wakes_its_threads_and_refuses_more() {
        let run = Arc::new(Waits::default());
        let l = listen("127.0.0.1", 0).unwrap();
        let c = connect("127.0.0.1", local_port(l).unwrap()).unwrap();
        let (s, _) = accept(l).unwrap();
        let r = run.clone();
        let waiting = std::thread::spawn(move || {
            join(&r);
            let got = read(s, 8);
            let again = accept(l);
            leave();
            (got, again)
        });
        std::thread::sleep(Duration::from_millis(50));
        let t0 = Instant::now();
        run.stop();
        let (got, again) = waiting.join().unwrap();
        assert!(got.map_or(true, |b| b.is_empty()));
        assert!(again.is_err());
        assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
        // Not this thread's run: its calls go on as ever.
        assert!(local_port(l).is_ok());
        close(c);
        close(l);
    }
}
