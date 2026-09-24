use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{oneshot, Mutex};
use tokio::time::{sleep, timeout, Instant};
use zbus::zvariant::OwnedValue;
use zbus::{Connection, MatchRule, Message, MessageStream};

const OPTIONS_PATH: &str = "/data/options.json";

// Keys that auto-repeat while held down: the first press is sent immediately,
// then after HOLD_DELAY the action repeats every HOLD_INTERVAL until release.
const HOLD_DELAY: Duration = Duration::from_millis(400);
const HOLD_INTERVAL: Duration = Duration::from_millis(150);
// Safety net in case the release notification is lost.
const HOLD_MAX: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, PartialEq)]
enum Nad {
    VolumeUp,
    VolumeDown,
    Mute,
}

#[derive(Clone, Copy)]
enum Action {
    Nad(Nad),
    Ircc(&'static str, &'static str),
    App(&'static str, &'static str),
}

impl Action {
    fn label(&self) -> String {
        match self {
            Action::Nad(Nad::VolumeUp) => "nad/volume_up".into(),
            Action::Nad(Nad::VolumeDown) => "nad/volume_down".into(),
            Action::Nad(Nad::Mute) => "nad/mute".into(),
            Action::Ircc(name, _) => format!("ircc/{name}"),
            Action::App(name, _) => format!("app/{name}"),
        }
    }

    fn repeatable(&self) -> bool {
        match self {
            Action::Nad(n) => *n != Nad::Mute,
            Action::Ircc(name, _) => {
                matches!(*name, "Up" | "Down" | "Left" | "Right" | "ChannelUp" | "ChannelDown")
            }
            Action::App(..) => false,
        }
    }
}

fn key_action(code: u16) -> Option<Action> {
    use Action::*;
    Some(match code {
        0x00E9 => Nad(self::Nad::VolumeUp),
        0x00EA => Nad(self::Nad::VolumeDown),
        0x00E2 => Nad(self::Nad::Mute),
        0x0223 => Ircc("Home", "AAAAAQAAAAEAAABgAw=="),
        0x0224 => Ircc("Return", "AAAAAgAAAJcAAAAjAw=="),
        0x0042 => Ircc("Up", "AAAAAQAAAAEAAAB0Aw=="),
        0x0043 => Ircc("Down", "AAAAAQAAAAEAAAB1Aw=="),
        0x0044 => Ircc("Left", "AAAAAQAAAAEAAAA0Aw=="),
        0x0045 => Ircc("Right", "AAAAAQAAAAEAAAAzAw=="),
        0x0041 => Ircc("Confirm", "AAAAAQAAAAEAAABlAw=="),
        0x00CD => Ircc("PlayPause", "AAAAAgAAAJcAAAAPAw=="),
        0x009C => Ircc("ChannelUp", "AAAAAQAAAAEAAAAQAw=="),
        0x009D => Ircc("ChannelDown", "AAAAAQAAAAEAAAARAw=="),
        0x008D => Ircc("EPG", "AAAAAgAAAKQAAABbAw=="),
        0x0547 => Ircc("Netflix", "AAAAAgAAABoAAAB8Aw=="),
        0x0533 => Ircc("TvInput", "AAAAAQAAAAEAAAAlAw=="),
        0x04E5 => Ircc("YouTube", "AAAAAgAAAMQAAABHAw=="),
        0x03C3 => Ircc("Help", "AAAAAgAAAMQAAABNAw=="),
        0x051F => Ircc("GoogleDashboard", "AAAAAgAAAMQAAABwAw=="),
        0x0089 => App("TV", "com.sony.dtv.com.sony.dtv.tvlin.com.sony.dtv.tvlin.view.MainActivity"),
        0x0586 => App(
            "Settings",
            "com.sony.dtv.com.android.tv.settings.com.android.tv.settings.MainSettings",
        ),
        0x04EB => App(
            "DisneyPlus",
            "com.sony.dtv.com.disney.disneyplus.com.bamtechmedia.dominguez.main.MainActivity",
        ),
        0x04EA => App(
            "PrimeVideo",
            "com.sony.dtv.com.amazon.amazonvideo.livingroom.com.amazon.ignition.IgnitionActivity",
        ),
        0x04F0 => App(
            "SonyPicturesCore",
            "com.sony.dtv.com.sonypicturescore.com.sphe.bravialounge.SplashActivity",
        ),
        0x04FB => App("ARDMediathek", "com.sony.dtv.de.swr.avp.ard.tv.de.swr.avp.ard.tv.TvActivity"),
        _ => return None,
    })
}

#[derive(Deserialize)]
struct Options {
    remote_mac: String,
    nad_host: String,
    nad_port: u16,
    tv_host: String,
    tv_psk: String,
}

struct Ctx {
    opts: Options,
    dev_path: String,
    char_path: String,
    conn: Connection,
    nad: Mutex<NadLink>,
    tv: Mutex<Option<TcpStream>>,
    connected: AtomicBool,
    reconnecting: AtomicBool,
}

type Error = Box<dyn std::error::Error + Send + Sync>;

// Logs in UTC: the image has no timezone database.
fn log(msg: &str) {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rem) = ((secs / 86400) as i64, secs % 86400);
    // Civil-from-days (Howard Hinnant).
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    println!(
        "[{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}Z] {msg}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    );
}

// The C338 firmware does not free the slots of closed connections: a burst of
// short-lived connections (e.g. one per volume step while a key is held)
// locks it up until it is power-cycled. So the bridge keeps one persistent
// connection and only reconnects after an error, with a pause between
// failed attempts.
const NAD_RETRY_PAUSE: Duration = Duration::from_secs(5);

struct NadLink {
    stream: Option<TcpStream>,
    retry_after: Option<Instant>,
}

// Discards anything already buffered (e.g. unsolicited status updates) and
// reports whether the peer has closed or reset the connection.
fn drain_is_closed(stream: &TcpStream) -> bool {
    let mut buf = [0u8; 512];
    loop {
        match stream.try_read(&mut buf) {
            Ok(0) => return true,
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return false,
            Err(_) => return true,
        }
    }
}

fn set_keepalive(stream: &TcpStream) {
    // Detect a silently dead connection within about a minute.
    let ka = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(30))
        .with_interval(Duration::from_secs(10))
        .with_retries(3);
    if let Err(e) = socket2::SockRef::from(stream).set_tcp_keepalive(&ka) {
        log(&format!("NAD: could not enable TCP keepalive: {e}"));
    }
}

// Takes the stream out of the link for the duration of a command, so a
// command that fails halfway never leaves a desynchronized stream behind.
async fn nad_take_stream(ctx: &Ctx, link: &mut NadLink) -> Result<TcpStream, Error> {
    if let Some(stream) = link.stream.take() {
        if !drain_is_closed(&stream) {
            return Ok(stream);
        }
        log("NAD: connection closed by amplifier");
    }
    if let Some(at) = link.retry_after {
        let now = Instant::now();
        if now < at {
            return Err(format!("not reachable, next attempt in {}s", (at - now).as_secs() + 1).into());
        }
    }
    let addr = (ctx.opts.nad_host.as_str(), ctx.opts.nad_port);
    match timeout(Duration::from_secs(3), TcpStream::connect(addr)).await {
        Ok(Ok(stream)) => {
            link.retry_after = None;
            set_keepalive(&stream);
            log("NAD: connected");
            Ok(stream)
        }
        Ok(Err(e)) => {
            link.retry_after = Some(Instant::now() + NAD_RETRY_PAUSE);
            Err(format!("connect: {e}").into())
        }
        Err(_) => {
            link.retry_after = Some(Instant::now() + NAD_RETRY_PAUSE);
            Err("connect: timed out".into())
        }
    }
}

// Sends one command; with `expect`, waits for the reply line starting with it.
// Ok(None) means no reply arrived in time (the connection is kept).
async fn nad_command(
    ctx: &Ctx,
    link: &mut NadLink,
    cmd: &str,
    expect: Option<&str>,
) -> Result<Option<String>, Error> {
    let mut stream = nad_take_stream(ctx, link).await?;
    if let Err(e) = stream.write_all(cmd.as_bytes()).await {
        log(&format!("NAD: write failed, dropping connection: {e}"));
        return Err(e.into());
    }
    let Some(prefix) = expect else {
        link.stream = Some(stream);
        return Ok(None);
    };

    let mut pending = Vec::new();
    let read = timeout(Duration::from_millis(1500), async {
        let mut buf = [0u8; 256];
        loop {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                return Err::<String, Error>("connection closed by amplifier".into());
            }
            pending.extend_from_slice(&buf[..n]);
            while let Some(pos) = pending.iter().position(|&b| b == b'\n' || b == b'\r') {
                let line: Vec<u8> = pending.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line).trim().to_string();
                if line.starts_with(prefix) {
                    return Ok(line);
                }
            }
        }
    })
    .await;
    match read {
        Ok(Ok(line)) => {
            link.stream = Some(stream);
            Ok(Some(line))
        }
        Ok(Err(e)) => {
            log(&format!("NAD: {e}"));
            Err(e)
        }
        // No reply is not a broken connection: keep it rather than spend a slot.
        Err(_) => {
            link.stream = Some(stream);
            Ok(None)
        }
    }
}

async fn handle_nad(ctx: &Ctx, cmd: Nad) -> Result<(), Error> {
    let mut link = ctx.nad.lock().await;
    match cmd {
        Nad::VolumeUp => {
            nad_command(ctx, &mut link, "Main.Volume+\r", None).await?;
        }
        Nad::VolumeDown => {
            nad_command(ctx, &mut link, "Main.Volume-\r", None).await?;
        }
        Nad::Mute => {
            let resp = nad_command(ctx, &mut link, "Main.Mute?\r", Some("Main.Mute="))
                .await?
                .ok_or("no reply to Main.Mute?")?;
            let set = format!("Main.Mute={}\r", if resp == "Main.Mute=On" { "Off" } else { "On" });
            // Waiting for the echo reveals a connection the amplifier closed in
            // the meantime; setting mute is idempotent, so retry once.
            if nad_command(ctx, &mut link, &set, Some("Main.Mute=")).await.is_err() {
                nad_command(ctx, &mut link, &set, Some("Main.Mute=")).await?;
            }
        }
    }
    Ok(())
}

// Reads one HTTP response and returns (status line, whether the connection
// may be reused). Handles Content-Length and chunked bodies.
async fn read_http_response(stream: &mut TcpStream) -> Result<(String, bool), Error> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    async fn fill(stream: &mut TcpStream, buf: &mut Vec<u8>, chunk: &mut [u8]) -> Result<(), Error> {
        let n = stream.read(chunk).await?;
        if n == 0 {
            return Err("connection closed".into());
        }
        buf.extend_from_slice(&chunk[..n]);
        Ok(())
    }
    let find = |buf: &[u8], pat: &[u8]| buf.windows(pat.len()).position(|w| w == pat);

    let head_end = loop {
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            break pos;
        }
        fill(stream, &mut buf, &mut chunk).await?;
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    buf.drain(..head_end + 4);

    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("").to_string();
    let mut keep_alive = status_line.starts_with("HTTP/1.1");
    let mut length = None;
    let mut chunked = false;
    for line in lines {
        let Some((k, v)) = line.split_once(':') else { continue };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().to_ascii_lowercase());
        match k.as_str() {
            "content-length" => length = v.parse::<usize>().ok(),
            "transfer-encoding" => chunked = v.contains("chunked"),
            "connection" if v == "close" => keep_alive = false,
            "connection" if v == "keep-alive" => keep_alive = true,
            _ => {}
        }
    }

    if chunked {
        loop {
            let line_end = loop {
                if let Some(pos) = find(&buf, b"\r\n") {
                    break pos;
                }
                fill(stream, &mut buf, &mut chunk).await?;
            };
            let size_str = String::from_utf8_lossy(&buf[..line_end]).into_owned();
            let size = usize::from_str_radix(size_str.split(';').next().unwrap_or("").trim(), 16)
                .map_err(|_| format!("bad chunk size {size_str:?}"))?;
            buf.drain(..line_end + 2);
            if size == 0 {
                // Skip optional trailers up to the terminating empty line.
                loop {
                    let end = loop {
                        if let Some(pos) = find(&buf, b"\r\n") {
                            break pos;
                        }
                        fill(stream, &mut buf, &mut chunk).await?;
                    };
                    buf.drain(..end + 2);
                    if end == 0 {
                        break;
                    }
                }
                break;
            }
            while buf.len() < size + 2 {
                fill(stream, &mut buf, &mut chunk).await?;
            }
            buf.drain(..size + 2);
        }
    } else if let Some(len) = length {
        while buf.len() < len {
            fill(stream, &mut buf, &mut chunk).await?;
        }
    } else {
        // Body delimited by connection close.
        keep_alive = false;
    }
    Ok((status_line, keep_alive))
}

// Sony's IRCC/appControl endpoints time out on concurrent requests, so all
// TV-bound requests are serialized behind the lock that also guards the
// kept-alive connection.
async fn tv_post(ctx: &Ctx, path: &str, headers: &[(&str, &str)], body: &str) -> Result<(), Error> {
    let mut head = format!(
        "POST {path} HTTP/1.1\r\nHost: {}\r\nX-Auth-PSK: {}\r\nContent-Length: {}\r\n",
        ctx.opts.tv_host,
        ctx.opts.tv_psk,
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let request = [head.as_bytes(), body.as_bytes()].concat();

    let mut slot = ctx.tv.lock().await;
    for attempt in 0..2 {
        // Taken out of the slot so an interrupted request never leaves a
        // half-read response on a connection that is reused later.
        let reused = slot.take().filter(|s| !drain_is_closed(s));
        let is_reused = reused.is_some();
        let exchange = async {
            let mut stream = match reused {
                Some(s) => s,
                None => TcpStream::connect((ctx.opts.tv_host.as_str(), 80)).await?,
            };
            stream.write_all(&request).await?;
            let (status_line, keep_alive) = read_http_response(&mut stream).await?;
            Ok::<_, Error>((stream, status_line, keep_alive))
        };
        match timeout(Duration::from_secs(6), exchange).await {
            Err(_) => return Err(format!("HTTP {path}: timed out").into()),
            // The TV may have closed an idle kept-alive connection just as we
            // reused it: retry once on a fresh connection.
            Ok(Err(_)) if is_reused && attempt == 0 => continue,
            Ok(Err(e)) => return Err(format!("HTTP {path}: {e}").into()),
            Ok(Ok((stream, status_line, keep_alive))) => {
                if keep_alive {
                    *slot = Some(stream);
                }
                let status: u16 =
                    status_line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
                if !(200..300).contains(&status) {
                    return Err(format!("HTTP {path}: {status_line:?}").into());
                }
                return Ok(());
            }
        }
    }
    unreachable!("second attempt always returns")
}

async fn handle_ircc(ctx: &Ctx, code: &str) -> Result<(), Error> {
    let body = format!(
        "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body>\
         <u:X_SendIRCC xmlns:u=\"urn:schemas-sony-com:service:IRCC:1\">\
         <IRCCCode>{code}</IRCCCode>\
         </u:X_SendIRCC></s:Body></s:Envelope>"
    );
    let headers = [
        ("SOAPACTION", "\"urn:schemas-sony-com:service:IRCC:1#X_SendIRCC\""),
        ("Content-Type", "text/xml; charset=UTF-8"),
    ];
    tv_post(ctx, "/sony/ircc", &headers, &body).await
}

async fn handle_app(ctx: &Ctx, uri: &str) -> Result<(), Error> {
    let body = serde_json::json!({
        "method": "setActiveApp",
        "id": 1,
        "params": [{"uri": uri}],
        "version": "1.0",
    })
    .to_string();
    tv_post(ctx, "/sony/appControl", &[("Content-Type", "application/json")], &body).await
}

// Returns whether the action succeeded.
async fn dispatch(ctx: &Ctx, action: Action, quiet: bool) -> bool {
    let result = match action {
        Action::Nad(cmd) => handle_nad(ctx, cmd).await,
        Action::Ircc(_, code) => handle_ircc(ctx, code).await,
        Action::App(_, uri) => handle_app(ctx, uri).await,
    };
    match result {
        Ok(()) => {
            if !quiet {
                log(&format!("OK {}", action.label()));
            }
            true
        }
        Err(e) => {
            log(&format!("ERROR {}: {e}", action.label()));
            false
        }
    }
}

// Logs the repeat count when the hold task ends.
struct HoldReport {
    label: String,
    count: u32,
}

impl Drop for HoldReport {
    fn drop(&mut self) {
        if self.count > 0 {
            log(&format!("Hold {} repeated {}x", self.label, self.count));
        }
    }
}

// Repeats until `stop` fires (or its sender is dropped). Stopping only happens
// between repeats, so a command already on the wire always completes.
async fn hold_repeat(ctx: Arc<Ctx>, action: Action, mut stop: oneshot::Receiver<()>) {
    let mut report = HoldReport { label: action.label(), count: 0 };
    let deadline = Instant::now() + HOLD_DELAY + HOLD_MAX;
    let mut wait = HOLD_DELAY;
    loop {
        tokio::select! {
            _ = &mut stop => return,
            _ = sleep(wait) => {}
        }
        if Instant::now() >= deadline {
            log(&format!("Hold {} stopped after {}s without release", report.label, HOLD_MAX.as_secs()));
            return;
        }
        if !dispatch(&ctx, action, true).await {
            log(&format!("Hold {} stopped after an error", report.label));
            return;
        }
        report.count += 1;
        wait = HOLD_INTERVAL;
    }
}

async fn bluez_call(ctx: &Ctx, path: &str, iface: &str, method: &str) -> Result<(), zbus::Error> {
    ctx.conn.call_method(Some("org.bluez"), path, Some(iface), method, &()).await?;
    Ok(())
}

async fn connect_and_subscribe(ctx: &Ctx) -> Result<(), zbus::Error> {
    // Connect fails if the remote is already connected; StartNotify below is
    // what decides whether we are subscribed.
    if let Err(e) = bluez_call(ctx, &ctx.dev_path, "org.bluez.Device1", "Connect").await {
        log(&format!("Connect: {e}"));
    }
    match bluez_call(ctx, &ctx.char_path, "org.bluez.GattCharacteristic1", "StartNotify").await {
        Err(zbus::Error::MethodError(name, _, _)) if name.as_str() == "org.bluez.Error.InProgress" => Ok(()),
        r => r,
    }
}

fn spawn_reconnect(ctx: &Arc<Ctx>) {
    if ctx.reconnecting.swap(true, Ordering::SeqCst) {
        return;
    }
    let ctx = ctx.clone();
    tokio::spawn(async move {
        loop {
            log("Connecting to remote...");
            match connect_and_subscribe(&ctx).await {
                Ok(()) => {
                    ctx.connected.store(true, Ordering::SeqCst);
                    log("Connected and subscribed.");
                    break;
                }
                Err(e) => {
                    log(&format!("Subscribe failed, retrying in 10s: {e}"));
                    sleep(Duration::from_secs(10)).await;
                }
            }
        }
        ctx.reconnecting.store(false, Ordering::SeqCst);
    });
}

struct KeyState {
    last_code: Option<u16>,
    // Dropping the sender stops the running hold task.
    hold: Option<oneshot::Sender<()>>,
}

impl KeyState {
    fn stop_hold(&mut self) {
        self.hold = None;
    }
}

type Changed = (String, HashMap<String, OwnedValue>, Vec<String>);

fn handle_signal(ctx: &Arc<Ctx>, keys: &mut KeyState, msg: &Message) {
    let header = msg.header();
    let Some(path) = header.path() else { return };
    let Ok((iface, mut changed, _)) = msg.body().deserialize::<Changed>() else { return };

    if path.as_str() == ctx.dev_path && iface == "org.bluez.Device1" {
        if let Some(connected) = changed.remove("Connected").and_then(|v| bool::try_from(v).ok()) {
            log(&format!("Device Connected={connected}"));
            ctx.connected.store(connected, Ordering::SeqCst);
            if !connected {
                keys.stop_hold();
                keys.last_code = None;
                spawn_reconnect(ctx);
            }
        }
    }

    if path.as_str() == ctx.char_path && iface == "org.bluez.GattCharacteristic1" {
        let Some(value) = changed.remove("Value").and_then(|v| Vec::<u8>::try_from(v).ok()) else {
            return;
        };
        if value.len() < 2 {
            return;
        }
        let code = u16::from_le_bytes([value[0], value[1]]);
        if code == 0 {
            keys.stop_hold();
            keys.last_code = None;
            return;
        }
        if keys.last_code == Some(code) {
            return;
        }
        keys.stop_hold();
        keys.last_code = Some(code);
        log(&format!("RX code=0x{code:04x}"));
        match key_action(code) {
            Some(action) => {
                let c = ctx.clone();
                tokio::spawn(async move { dispatch(&c, action, false).await });
                if action.repeatable() {
                    let (stop_tx, stop_rx) = oneshot::channel();
                    tokio::spawn(hold_repeat(ctx.clone(), action, stop_rx));
                    keys.hold = Some(stop_tx);
                }
            }
            None => log(&format!("Unknown code 0x{code:04x}, ignoring")),
        }
    }
}

async fn run() -> Result<(), Error> {
    let path = std::env::var("OPTIONS_PATH").unwrap_or_else(|_| OPTIONS_PATH.into());
    let opts: Options = serde_json::from_slice(&std::fs::read(&path).map_err(|e| format!("{path}: {e}"))?)?;
    let dev_path = format!("/org/bluez/hci0/dev_{}", opts.remote_mac.replace(':', "_").to_uppercase());
    let char_path = format!("{dev_path}/service0080/char008c");
    log(&format!("Starting NAD/Bravia remote bridge (remote {})", opts.remote_mac));

    let conn = Connection::system().await?;
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.DBus.Properties")?
        .member("PropertiesChanged")?
        .path_namespace(dev_path.clone())?
        .build();
    let mut stream = MessageStream::for_match_rule(rule, &conn, Some(64)).await?;

    let ctx = Arc::new(Ctx {
        opts,
        dev_path,
        char_path,
        conn,
        nad: Mutex::new(NadLink { stream: None, retry_after: None }),
        tv: Mutex::new(None),
        connected: AtomicBool::new(false),
        reconnecting: AtomicBool::new(false),
    });
    let mut keys = KeyState { last_code: None, hold: None };

    // As PID 1 in a scratch image, SIGTERM is ignored unless handled.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    spawn_reconnect(&ctx);
    let mut watchdog = tokio::time::interval(Duration::from_secs(30));
    loop {
        tokio::select! {
            msg = stream.next() => match msg {
                Some(Ok(msg)) => handle_signal(&ctx, &mut keys, &msg),
                Some(Err(e)) => log(&format!("D-Bus error: {e}")),
                None => return Err("D-Bus connection closed".into()),
            },
            _ = sigterm.recv() => {
                log("Stopping.");
                return Ok(());
            }
            _ = watchdog.tick() => {
                if !ctx.connected.load(Ordering::SeqCst) {
                    spawn_reconnect(&ctx);
                }
            }
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(e) = run().await {
        log(&format!("FATAL: {e}"));
        std::process::exit(1);
    }
}
