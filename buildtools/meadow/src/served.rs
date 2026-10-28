//! **A VM kept for many runs**: `meadow-silo-vm --serve`, which loads an
//! image, then each one it is sent as a reload, and runs an entry when it is
//! told -- what a REPL's session runs on, one VM for all of it, and what a
//! build's macros run on, one VM for every call. `silo/vm/src/main.rs` says
//! what is said on the connection.
//!
//! A program that fails ends the VM, with its message on standard error, as
//! it would end any run of it; the next run starts another.

use serde_json::{Value as Json, json};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub struct Served {
    child: Child,
    out: TcpStream,
    input: BufReader<TcpStream>,
    /// What it has said on standard error, where that is kept rather than
    /// shown: and whether it has stopped saying anything.
    said: Option<(
        std::sync::Arc<std::sync::Mutex<String>>,
        std::sync::mpsc::Receiver<()>,
    )>,
}

/// How a run ended other than with an answer.
#[derive(Debug)]
pub enum Ended {
    /// The VM ended, as its status says: the program failed, and said why.
    Failed(Option<i32>),
    /// It took longer than it was given, and was stopped.
    TimedOut,
    /// Something went wrong between this and the VM.
    Broken(String),
}

impl Ended {
    /// As a person is told it.
    pub fn said(&self) -> String {
        match self {
            Ended::Failed(code) => format!(
                "the program failed ({})",
                code.map_or("killed".to_string(), |c| format!("exit status {c}"))
            ),
            Ended::TimedOut => "it did not finish in the time it was given".into(),
            Ended::Broken(why) => why.clone(),
        }
    }
}

/// Said when `MEADOW_SERVE_LOG` is set: when a VM is started, and when it
/// takes a build, for whoever is checking how many there were.
fn log(what: &str) {
    if std::env::var_os("MEADOW_SERVE_LOG").is_some() {
        eprintln!("served: {what}");
    }
}

impl Served {
    /// A VM for `image`, its standard input and output the program's if
    /// `console`, and nothing if not -- a run that is given its input keeps
    /// what it writes either way -- and what it says on standard error shown
    /// if `console`, and kept for [`Served::said`] if not. `env` is set for
    /// it: what tells the VM to run with its JIT.
    pub fn start(image: &Path, console: bool, env: &[(&str, &str)]) -> Result<Served, String> {
        let listener =
            TcpListener::bind(("127.0.0.1", 0)).map_err(|e| format!("cannot listen: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let (stdin, stdout) = if console {
            (Stdio::inherit(), Stdio::inherit())
        } else {
            (Stdio::null(), Stdio::null())
        };
        let mut child = Command::new(crate::aot::interpreter()?)
            .envs(env.iter().copied())
            .arg("--serve")
            .arg(port.to_string())
            .arg(image)
            .stdin(stdin)
            .stdout(stdout)
            .stderr(if console {
                Stdio::inherit()
            } else {
                Stdio::piped()
            })
            .spawn()
            .map_err(|e| format!("cannot start Silo's interpreter: {e}"))?;
        let said = child.stderr.take().map(|mut err| {
            let kept = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
            let (done, ended) = std::sync::mpsc::channel();
            let into = kept.clone();
            std::thread::spawn(move || {
                use std::io::Read;
                let mut buf = [0u8; 4096];
                while let Ok(n) = err.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    into.lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .push_str(&String::from_utf8_lossy(&buf[..n]));
                }
                let _ = done.send(());
            });
            (kept, ended)
        });
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(60);
        let conn = loop {
            match listener.accept() {
                Ok((conn, _)) => break conn,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if let Ok(Some(status)) = child.try_wait() {
                        return Err(format!(
                            "Silo's interpreter ended before it began: {status}"
                        ));
                    }
                    if Instant::now() > deadline {
                        let _ = child.kill();
                        return Err("Silo's interpreter did not connect".into());
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(e) => return Err(format!("the interpreter's connection failed: {e}")),
            }
        };
        conn.set_nonblocking(false).map_err(|e| e.to_string())?;
        let input = BufReader::new(conn.try_clone().map_err(|e| e.to_string())?);
        let mut s = Served {
            child,
            out: conn,
            input,
            said,
        };
        match s.hear(None) {
            Ok(m) if m.get("ready").is_some() => {
                log("started");
                Ok(s)
            }
            Ok(m) => Err(format!("Silo's interpreter said {m} first")),
            Err(e) => Err(e.said()),
        }
    }

    fn send(&mut self, msg: Json) -> Result<(), Ended> {
        let mut text = msg.to_string();
        text.push('\n');
        self.out
            .write_all(text.as_bytes())
            .and_then(|()| self.out.flush())
            .map_err(|_| self.ended())
    }

    /// What the VM says next, waiting at most `wait`.
    fn hear(&mut self, wait: Option<Duration>) -> Result<Json, Ended> {
        let _ = self.input.get_ref().set_read_timeout(wait);
        let mut line = String::new();
        match self.input.read_line(&mut line) {
            Ok(0) => Err(self.ended()),
            Ok(_) => serde_json::from_str(&line).map_err(|e| Ended::Broken(e.to_string())),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                let _ = self.child.kill();
                let _ = self.child.wait();
                Err(Ended::TimedOut)
            }
            Err(_) => Err(self.ended()),
        }
    }

    /// How the VM ended, once its connection has.
    fn ended(&mut self) -> Ended {
        Ended::Failed(self.child.wait().ok().and_then(|s| s.code()))
    }

    /// What it has said on standard error since this was last asked, where
    /// that is kept: all of it, once it has ended.
    pub fn said(&mut self) -> String {
        let Some((kept, ended)) = &self.said else {
            return String::new();
        };
        if self.child.try_wait().ok().flatten().is_some() {
            let _ = ended.recv_timeout(Duration::from_secs(5));
        }
        std::mem::take(&mut *kept.lock().unwrap_or_else(|p| p.into_inner()))
    }

    /// The image at `image`, loaded into the program as a reload: what the
    /// VM said of it, or why it would not.
    pub fn load(&mut self, image: &Path) -> Result<String, Result<String, Ended>> {
        self.send(json!({ "load": image.to_string_lossy() }))
            .map_err(Err)?;
        let m = self.hear(None).map_err(Err)?;
        if let Some(what) = m["reloaded"].as_str() {
            log("reloaded");
            return Ok(what.to_string());
        }
        let why = m["refused"].as_str().unwrap_or("refused").to_string();
        log(&format!("refused: {why}"));
        Err(Ok(why))
    }

    /// Run entry `entry` of a program of entries, or the entry of the build
    /// last loaded, its console reading the lines of `input` if there is one,
    /// each whole, for at most `limit`: the value as it is shown, and what it
    /// wrote, if its console was given. Each run of a program of entries is
    /// a program's first, with no top-level value computed: a macro's is.
    pub fn run(
        &mut self,
        entry: Option<usize>,
        input: Option<&[&str]>,
        limit: Option<Duration>,
    ) -> Result<(String, Option<String>), Ended> {
        let fresh = entry.is_some();
        self.send(json!({ "run": { "entry": entry, "input": input, "fresh": fresh } }))?;
        let m = self.hear(limit)?;
        log(&format!("ran: {m}"));
        Ok((
            m["done"].as_str().unwrap_or("").to_string(),
            m["output"].as_str().map(str::to_string),
        ))
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.send(json!({ "exit": true }));
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
