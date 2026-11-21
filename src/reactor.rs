//! The network thread's event loop (plan step 7): the "reactor pattern".
//!
//! One loop, one thread, never blocking on any single thing:
//!
//!   loop {
//!       ask the OS: "which sockets are ready?" (wait until one is, or until
//!                    the next timer tick, or until the state machine wakes us)
//!       for each ready socket: make as much progress as possible without
//!                              waiting (read everything there, write what we can)
//!       send any actions the state machine queued
//!       fire the 1-second timer if it's due
//!       reconnect anything that dropped (with growing delays)
//!   }
//!
//! "Ask the OS" is kqueue on macOS and epoll on Linux; the `mio` library
//! gives both one interface. Sockets are NON-BLOCKING: a read with nothing
//! to read returns the error `WouldBlock` immediately instead of waiting.
//! That error is normal and just means "come back later".
//!
//! mio is EDGE-TRIGGERED: it tells us when a socket *becomes* ready, once.
//! So every time we touch a socket we must keep reading until WouldBlock,
//! or leftover data may sit there with no further notification.
//!
//! Each connection is its own little state machine:
//!   TcpConnecting -> TlsHandshaking -> (websocket: Upgrading) -> Open

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::ToSocketAddrs;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mio::net::TcpStream;
use mio::{Events, Interest, Poll, Token, Waker};
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use serde_json::{json, Value};
use tungstenite::handshake::client::generate_key;
use tungstenite::handshake::derive_accept_key;
use tungstenite::protocol::Role as WsRole;
use tungstenite::{Message, WebSocket};

use crate::action::Action;
use crate::eventlog;
use crate::queue::EventWriter;
use crate::http::{find_from, header_value, parse_response};
use crate::kalshi::{self, Credentials};
use crate::net::{self, Header};
use crate::sm::{REST_CONN, WS_CONN};
use crate::Error;

type Tls = StreamOwned<ClientConnection, TcpStream>;

/// mio identifies sockets by "tokens". Connections use their conn number.
const WAKER: Token = Token(0);
const TICK: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Kalshi closes a REST connection after 10 s without a request (we saw it
/// in the logs). Sending an order at that exact moment would fail with no
/// way to know whether it went through. So we replace idle connections a
/// little before that, on our own schedule.
const REST_RECYCLE: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Ws,
    Rest,
}

#[derive(Debug)]
enum Phase {
    TcpConnecting,
    TlsHandshaking,
    /// Websocket upgrade request sent; waiting for "101 Switching Protocols".
    Upgrading { key: String },
    Open,
}

/// Before the upgrade we speak raw TLS; after it, tungstenite owns the stream.
enum Transport {
    Tls(Tls),
    Ws(WebSocket<Tls>),
}

struct Conn {
    role: Role,
    id: u64,
    host: &'static str,
    /// Websocket only: signed auth headers for the upgrade request, made
    /// when the connection attempt starts (signatures expire after 30 s).
    upgrade_headers: Vec<Header>,
    phase: Phase,
    /// Option so we can move the stream from Tls into Ws.
    transport: Option<Transport>,
    /// Bytes read but not yet parsed (HTTP responses).
    inbuf: Vec<u8>,
}

impl Conn {
    fn tls(&mut self) -> &mut Tls {
        match self.transport.as_mut().expect("transport present") {
            Transport::Tls(t) => t,
            Transport::Ws(ws) => ws.get_mut(),
        }
    }
}

/// What a connection did during one `drive`, for the reactor to react to.
#[derive(Default)]
struct Progress {
    /// The websocket just finished its upgrade: (re)subscribe.
    ws_opened: bool,
    /// The REST connection just became ready to send.
    rest_ready: bool,
    /// One REST response completed.
    rest_responses: usize,
}

/// A REST request waiting to be sent.
struct RestRequest {
    method: &'static str,
    path: String,
    headers: Vec<Header>,
    body: Option<String>,
}

pub struct Reactor {
    poll: Poll,
    log: Box<dyn EventWriter>,
    creds: Credentials,
    tls_config: Arc<ClientConfig>,
    subscriptions: Vec<Value>,
    actions: Receiver<Action>,
    ws: Option<Conn>,
    rest: Option<Conn>,
    rest_queue: VecDeque<RestRequest>,
    rest_in_flight: bool,
    /// When the REST connection last became idle (None = busy or down).
    rest_idle_since: Option<Instant>,
    next_rest_attempt: Instant,
    rest_backoff: Duration,
    /// Kalshi wants a unique id on each command sent on a websocket.
    next_cmd_id: u64,
    next_ws_attempt: Instant,
    ws_backoff: Duration,
    next_tick: Instant,
    /// Testing aid: force one websocket disconnect this long after it opens.
    test_drop_after: Option<Duration>,
    test_drop_at: Option<Instant>,
}

impl Reactor {
    /// Build the reactor. Returns a `Waker` the state machine thread uses
    /// to interrupt the reactor's wait when it has queued an action.
    pub fn new(
        log: Box<dyn EventWriter>,
        creds: Credentials,
        tickers: &[String],
        actions: Receiver<Action>,
        test_drop_after: Option<Duration>,
    ) -> Result<(Reactor, Arc<Waker>), Error> {
        let poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), WAKER)?);
        let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let tls_config = Arc::new(ClientConfig::builder().with_root_certificates(roots).with_no_client_auth());
        // Public order books for our markets, plus two private channels
        // about our own orders: status changes and trades (fills).
        let subscriptions = vec![
            json!({ "channels": ["orderbook_delta"], "market_tickers": tickers }),
            json!({ "channels": ["user_orders"] }),
            json!({ "channels": ["fill"] }),
        ];
        let now = Instant::now();
        let reactor = Reactor {
            poll,
            log,
            creds,
            tls_config,
            subscriptions,
            actions,
            ws: None,
            rest: None,
            rest_queue: VecDeque::new(),
            rest_in_flight: false,
            rest_idle_since: None,
            next_rest_attempt: now,
            rest_backoff: Duration::from_secs(1),
            next_cmd_id: 1,
            next_ws_attempt: now,
            ws_backoff: Duration::from_secs(1),
            next_tick: now + TICK,
            test_drop_after,
            test_drop_at: None,
        };
        Ok((reactor, waker))
    }

    pub fn run(mut self) -> Result<(), Error> {
        let mut events = Events::with_capacity(64);
        loop {
            // Keep both connections up: the websocket for data, and a warm
            // REST connection so an order never waits for TCP + TLS setup
            // (~130 ms).
            let now = Instant::now();
            if self.ws.is_none() && now >= self.next_ws_attempt {
                self.connect(Role::Ws)?;
            }
            if self.rest.is_none() && now >= self.next_rest_attempt {
                self.connect(Role::Rest)?;
            }
            if self.rest_idle_since.is_some_and(|t| now >= t + REST_RECYCLE) {
                self.rest_idle_since = None;
                self.close(Role::Rest, "recycling idle connection")?;
                self.next_rest_attempt = Instant::now(); // replace it right away
            }

            // Sleep until something is ready, or the next deadline.
            let mut deadline = self.next_tick;
            if self.ws.is_none() {
                deadline = deadline.min(self.next_ws_attempt);
            }
            if self.rest.is_none() {
                deadline = deadline.min(self.next_rest_attempt);
            }
            if let Some(t) = self.rest_idle_since {
                deadline = deadline.min(t + REST_RECYCLE);
            }
            if let Some(t) = self.test_drop_at {
                deadline = deadline.min(t);
            }
            let timeout = deadline.saturating_duration_since(Instant::now());
            match self.poll.poll(&mut events, Some(timeout)) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }

            for event in events.iter() {
                match event.token() {
                    WAKER => {} // actions are drained below
                    Token(id) if id as u64 == WS_CONN => self.drive_conn(Role::Ws)?,
                    Token(id) if id as u64 == REST_CONN => self.drive_conn(Role::Rest)?,
                    _ => {}
                }
            }

            // Actions from the state machine.
            loop {
                match self.actions.try_recv() {
                    Ok(action) => self.execute(action)?,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return Ok(()), // state machine finished
                }
            }

            let now = Instant::now();
            if now >= self.next_tick {
                // A heartbeat for the state machine, so it can act (or
                // notice that nothing has happened) even in a quiet market.
                self.log.write(0, "timer", json!({}))?;
                self.next_tick += TICK;
                if self.next_tick < now {
                    self.next_tick = now + TICK; // we fell behind: don't burst
                }
            }
            if self.test_drop_at.is_some_and(|t| now >= t) {
                self.test_drop_at = None;
                self.test_drop_after = None; // only once
                self.close(Role::Ws, "test: forced disconnect")?;
            }
        }
    }

    // ---- connections --------------------------------------------------------

    fn slot(&mut self, role: Role) -> &mut Option<Conn> {
        match role {
            Role::Ws => &mut self.ws,
            Role::Rest => &mut self.rest,
        }
    }

    /// Start a non-blocking connect. It completes later, when the socket
    /// becomes writable.
    fn connect(&mut self, role: Role) -> Result<(), Error> {
        let (id, host) = match role {
            Role::Ws => (WS_CONN, self.creds.env.ws_host()),
            Role::Rest => (REST_CONN, self.creds.env.rest_host()),
        };
        self.log.write(id, "tcp_connect", json!({ "host": host, "port": 443 }))?;
        let result = (|| {
            // DNS lookup. This one call does block (briefly); acceptable
            // because it only happens when (re)connecting.
            let addr = (host, 443).to_socket_addrs()?.next().ok_or("no address for host")?;
            let mut sock = TcpStream::connect(addr)?;
            sock.set_nodelay(true)?;
            self.poll.registry().register(&mut sock, Token(id as usize), Interest::READABLE | Interest::WRITABLE)?;
            let tls = ClientConnection::new(self.tls_config.clone(), ServerName::try_from(host.to_string())?)?;
            Ok::<_, Error>(StreamOwned::new(tls, sock))
        })();
        match result {
            Ok(tls) => {
                let upgrade_headers = match role {
                    Role::Ws => self.creds.auth_headers(now_ms(), "GET", kalshi::WS_PATH),
                    Role::Rest => Vec::new(),
                };
                let conn = Conn {
                    role,
                    id,
                    host,
                    upgrade_headers,
                    phase: Phase::TcpConnecting,
                    transport: Some(Transport::Tls(tls)),
                    inbuf: Vec::new(),
                };
                *self.slot(role) = Some(conn);
                Ok(())
            }
            Err(e) => self.on_conn_failed(role, &e.to_string()),
        }
    }

    /// Make all the progress we can on one connection. Any error closes it.
    fn drive_conn(&mut self, role: Role) -> Result<(), Error> {
        let Some(mut conn) = self.slot(role).take() else { return Ok(()) };
        match drive(&mut conn, self.log.as_mut()) {
            Ok(progress) => {
                *self.slot(role) = Some(conn);
                if progress.ws_opened {
                    self.on_ws_open()?;
                }
                if progress.rest_responses > 0 {
                    self.rest_in_flight = false;
                }
                if progress.rest_ready {
                    self.rest_backoff = Duration::from_secs(1); // healthy
                }
                if progress.rest_ready || progress.rest_responses > 0 {
                    self.rest_idle_since = Some(Instant::now());
                    self.send_next_rest()?;
                }
                Ok(())
            }
            Err(e) => {
                *self.slot(role) = Some(conn);
                self.close(role, &e.to_string())
            }
        }
    }

    fn close(&mut self, role: Role, why: &str) -> Result<(), Error> {
        if let Some(mut conn) = self.slot(role).take() {
            let _ = self.poll.registry().deregister(&mut conn.tls().sock);
            self.log.write(conn.id, "tcp_close", json!(why))?;
        }
        self.on_conn_failed(role, why)
    }

    /// Clean up after a connection is gone (or never came up).
    fn on_conn_failed(&mut self, role: Role, why: &str) -> Result<(), Error> {
        match role {
            Role::Ws => {
                // Reconnect with "exponential backoff": wait 1 s, then 2, 4,
                // 8... up to 30 s. Hammering a struggling server makes things
                // worse, and Kalshi refuses clients that reconnect too fast.
                self.next_ws_attempt = Instant::now() + self.ws_backoff;
                self.ws_backoff = (self.ws_backoff * 2).min(MAX_BACKOFF);
            }
            Role::Rest => {
                self.rest_idle_since = None;
                if self.rest_in_flight {
                    // The state machine must hear about every REST action
                    // exactly once: a response body, or this.
                    self.rest_in_flight = false;
                    self.log.write(REST_CONN, "http_error", json!(why))?;
                }
                // Same backoff idea as the websocket. (A deliberate recycle
                // overrides this with "reconnect now".)
                self.next_rest_attempt = Instant::now() + self.rest_backoff;
                self.rest_backoff = (self.rest_backoff * 2).min(MAX_BACKOFF);
            }
        }
        Ok(())
    }

    fn on_ws_open(&mut self) -> Result<(), Error> {
        self.ws_backoff = Duration::from_secs(1); // healthy again
        if let Some(after) = self.test_drop_after {
            self.test_drop_at = Some(Instant::now() + after);
        }
        // Subscriptions don't survive a reconnect: (re)subscribe every time.
        for params in self.subscriptions.clone() {
            self.ws_command("subscribe", params)?;
        }
        Ok(())
    }

    /// Send a websocket command (logged first, like everything outgoing).
    fn ws_command(&mut self, cmd: &str, params: Value) -> Result<(), Error> {
        let text = json!({ "id": self.next_cmd_id, "cmd": cmd, "params": params }).to_string();
        self.next_cmd_id += 1;
        let Some(conn) = self.ws.as_mut().filter(|c| matches!(c.phase, Phase::Open)) else {
            return Ok(()); // not connected: the resubscribe on reconnect covers it
        };
        self.log.write(WS_CONN, "ws_out", json!(text))?;
        let Some(Transport::Ws(ws)) = conn.transport.as_mut() else { return Ok(()) };
        if let Err(e) = ws.send(Message::text(text)) {
            if !is_would_block(&e) {
                return self.close(Role::Ws, &e.to_string());
            }
            // WouldBlock: tungstenite keeps the frame and sends it when the
            // socket is writable again (the next drive).
        }
        Ok(())
    }

    // ---- actions ------------------------------------------------------------

    fn execute(&mut self, action: Action) -> Result<(), Error> {
        self.log.write(0, "action", serde_json::to_value(&action)?)?;
        if let Action::Resync { sid, tickers } = &action {
            let params = json!({ "sids": [sid], "market_tickers": tickers, "action": "get_snapshot" });
            return self.ws_command("update_subscription", params);
        }
        let Some(call) = kalshi::rest_call(&action) else { return Ok(()) };
        // Sign now; the timestamp must be within 30 s of Kalshi's clock
        // when the request arrives, which it easily will be.
        let headers = self.creds.auth_headers(now_ms(), call.method, &call.path);
        self.rest_queue.push_back(RestRequest {
            method: call.method,
            path: format!("{}{}", call.path, call.query),
            headers,
            body: call.body,
        });
        self.send_next_rest()
    }

    /// Send the next queued REST request, if the connection is ready and
    /// idle. One request at a time: responses then come back in order.
    fn send_next_rest(&mut self) -> Result<(), Error> {
        if self.rest_in_flight {
            return Ok(());
        }
        let Some(conn) = self.rest.as_mut().filter(|c| matches!(c.phase, Phase::Open)) else { return Ok(()) };
        let Some(req) = self.rest_queue.pop_front() else { return Ok(()) };
        let host = self.creds.env.rest_host();
        let build = |redact: bool| {
            let mut s = format!("{} {} HTTP/1.1\r\nHost: {host}\r\nAccept: application/json\r\n", req.method, req.path);
            if let Some(b) = &req.body {
                s.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", b.len()));
            }
            net::push_headers(&mut s, &req.headers, redact);
            s.push_str("\r\n");
            s.push_str(req.body.as_deref().unwrap_or(""));
            s
        };
        self.log.write(REST_CONN, "http_request", json!(build(true)))?; // log BEFORE sending
        self.rest_in_flight = true;
        self.rest_idle_since = None;
        // Hand the bytes to rustls (always succeeds: it buffers them), then
        // push encrypted bytes to the socket as far as it will take them.
        let result = conn.tls().conn.writer().write_all(build(false).as_bytes()).map_err(Error::from);
        let result = result.and_then(|_| flush_tls(conn.tls()));
        if let Err(e) = result {
            return self.close(Role::Rest, &e.to_string());
        }
        Ok(())
    }
}

// ---- driving one connection -------------------------------------------------

/// Push buffered encrypted bytes to the socket until it won't take more.
fn flush_tls(tls: &mut Tls) -> Result<(), Error> {
    while tls.conn.wants_write() {
        match tls.conn.write_tls(&mut tls.sock) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Read all available plaintext into `buf`. Err if the peer closed.
fn read_available(tls: &mut Tls, buf: &mut Vec<u8>) -> Result<(), Error> {
    let mut chunk = [0u8; 16384];
    loop {
        match tls.read(&mut chunk) {
            Ok(0) => return Err("connection closed by server".into()),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    }
}

/// `read_available` into the connection's own buffer. The buffer is moved
/// out and back because `conn.tls()` borrows the whole connection: Rust
/// won't allow two mutable borrows of `conn` at once (the borrow checker).
fn read_into_inbuf(conn: &mut Conn) -> Result<(), Error> {
    let mut buf = std::mem::take(&mut conn.inbuf);
    let result = read_available(conn.tls(), &mut buf);
    conn.inbuf = buf;
    result
}

fn is_would_block(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if io.kind() == io::ErrorKind::WouldBlock)
}

/// Advance one connection's state machine as far as it can go right now.
fn drive(conn: &mut Conn, log: &mut dyn EventWriter) -> Result<Progress, Error> {
    let mut progress = Progress::default();
    loop {
        match &conn.phase {
            Phase::TcpConnecting => {
                let sock = &conn.tls().sock;
                if let Some(e) = sock.take_error()? {
                    return Err(e.into());
                }
                // peer_addr fails with NotConnected until the connect is done.
                match sock.peer_addr() {
                    Ok(peer) => {
                        let local = sock.local_addr()?;
                        log.write(conn.id, "tcp_open", json!({ "local": local.to_string(), "peer": peer.to_string() }))?;
                        conn.phase = Phase::TlsHandshaking;
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotConnected => return Ok(progress),
                    Err(e) => return Err(e.into()),
                }
            }
            Phase::TlsHandshaking => {
                let tls = conn.tls();
                match tls.conn.complete_io(&mut tls.sock) {
                    Ok(_) => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e.into()),
                }
                if tls.conn.is_handshaking() {
                    flush_tls(tls)?;
                    return Ok(progress); // wait for the server's reply
                }
                let info = json!({
                    "version": format!("{:?}", tls.conn.protocol_version()),
                    "cipher": format!("{:?}", tls.conn.negotiated_cipher_suite().map(|s| s.suite())),
                });
                log.write(conn.id, "tls_open", info)?;
                match conn.role {
                    Role::Rest => {
                        conn.phase = Phase::Open;
                        progress.rest_ready = true;
                    }
                    Role::Ws => conn.phase = Phase::Upgrading { key: send_upgrade(conn, log)? },
                }
            }
            Phase::Upgrading { key } => {
                let key = key.clone();
                read_into_inbuf(conn)?;
                let Some(i) = find_from(&conn.inbuf, 0, b"\r\n\r\n") else { return Ok(progress) };
                let head = String::from_utf8_lossy(&conn.inbuf[..i + 4]).into_owned();
                log.write(conn.id, "http_response", json!(head))?;
                if !head.starts_with("HTTP/1.1 101") {
                    if let Ok(Some((resp, _))) = parse_response(&conn.inbuf) {
                        log.write(conn.id, "http_response_body", json!(resp.body_text()))?;
                    }
                    return Err(format!("websocket upgrade refused: {}", head.lines().next().unwrap_or("")).into());
                }
                if header_value(&head, "sec-websocket-accept") != Some(derive_accept_key(key.as_bytes()).as_str()) {
                    return Err("bad Sec-WebSocket-Accept from server".into());
                }
                // Bytes after the headers are already websocket data.
                let leftover = conn.inbuf.split_off(i + 4);
                conn.inbuf.clear();
                let Some(Transport::Tls(tls)) = conn.transport.take() else { unreachable!() };
                conn.transport = Some(Transport::Ws(WebSocket::from_partially_read(tls, leftover, WsRole::Client, None)));
                conn.phase = Phase::Open;
                progress.ws_opened = true;
            }
            Phase::Open => {
                match conn.role {
                    Role::Ws => drive_ws(conn, log)?,
                    Role::Rest => progress.rest_responses += drive_rest(conn, log)?,
                }
                return Ok(progress);
            }
        }
    }
}

/// Send the websocket upgrade request (a normal HTTP GET with extra headers).
fn send_upgrade(conn: &mut Conn, log: &mut dyn EventWriter) -> Result<String, Error> {
    let key = generate_key();
    let host = conn.host;
    let headers = std::mem::take(&mut conn.upgrade_headers);
    let build = |redact: bool| {
        let mut req = format!(
            "GET {} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n",
            kalshi::WS_PATH
        );
        net::push_headers(&mut req, &headers, redact);
        req.push_str("\r\n");
        req
    };
    log.write(conn.id, "http_request", json!(build(true)))?;
    conn.tls().conn.writer().write_all(build(false).as_bytes())?;
    flush_tls(conn.tls())?;
    Ok(key)
}

fn drive_ws(conn: &mut Conn, log: &mut dyn EventWriter) -> Result<(), Error> {
    let Some(Transport::Ws(ws)) = conn.transport.as_mut() else { return Ok(()) };
    loop {
        match ws.read() {
            Ok(Message::Text(text)) => log.write(conn.id, "ws_in", json!(text.as_str()))?,
            // Kalshi pings after 10 s without data; tungstenite queues the
            // pong, and the flush below sends it.
            Ok(Message::Ping(p)) => log.write(conn.id, "ws_ping", json!(p.len()))?,
            Ok(Message::Pong(p)) => log.write(conn.id, "ws_pong", json!(p.len()))?,
            Ok(Message::Binary(b)) => log.write(conn.id, "ws_in_binary", json!(b.len()))?,
            Ok(Message::Close(frame)) => log.write(conn.id, "ws_close", json!(format!("{frame:?}")))?,
            Ok(Message::Frame(_)) => {}
            Err(e) if is_would_block(&e) => break,
            Err(e) => return Err(e.into()),
        }
    }
    match ws.flush() {
        Ok(()) => Ok(()),
        Err(e) if is_would_block(&e) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Read and log every complete response that has arrived. Returns how many.
fn drive_rest(conn: &mut Conn, log: &mut dyn EventWriter) -> Result<usize, Error> {
    flush_tls(conn.tls())?;
    let read = read_into_inbuf(conn);
    // Log whatever complete responses arrived, even if the server then
    // closed the connection.
    let mut done = 0;
    while let Some((resp, used)) = parse_response(&conn.inbuf)? {
        conn.inbuf.drain(..used);
        log.write(conn.id, "http_response", json!(resp.head))?;
        log.write(conn.id, "http_response_body", json!(resp.body_text()))?;
        done += 1;
        if header_value(&resp.head, "connection").is_some_and(|v| v.eq_ignore_ascii_case("close")) {
            read?;
            return Err("server asked to close".into());
        }
    }
    read?;
    Ok(done)
}

fn now_ms() -> u64 {
    eventlog::now_nanos() / 1_000_000
}
