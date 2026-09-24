use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
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
    tv_lock: Mutex<()>,
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

async fn nad_send(ctx: &Ctx, cmd: &str) -> Result<String, Error> {
    let addr = (ctx.opts.nad_host.as_str(), ctx.opts.nad_port);
    let mut stream = timeout(Duration::from_secs(3), TcpStream::connect(addr)).await??;
    stream.write_all(cmd.as_bytes()).await?;
    let mut buf = [0u8; 256];
    let n = match timeout(Duration::from_millis(1500), stream.read(&mut buf)).await {
        Ok(r) => r?,
        Err(_) => 0,
    };
    Ok(String::from_utf8_lossy(&buf[..n]).into_owned())
}

async fn handle_nad(ctx: &Ctx, cmd: Nad) -> Result<(), Error> {
    match cmd {
        Nad::VolumeUp => {
            nad_send(ctx, "Main.Volume+\r").await?;
        }
        Nad::VolumeDown => {
            nad_send(ctx, "Main.Volume-\r").await?;
        }
        Nad::Mute => {
            let resp = nad_send(ctx, "Main.Mute?\r").await?;
            let next = if resp.contains("Main.Mute=On") { "Off" } else { "On" };
            nad_send(ctx, &format!("Main.Mute={next}\r")).await?;
        }
    }
    Ok(())
}

async fn tv_post(ctx: &Ctx, path: &str, headers: &[(&str, &str)], body: &str) -> Result<(), Error> {
    let req = async {
        let mut stream = TcpStream::connect((ctx.opts.tv_host.as_str(), 80)).await?;
        let mut head = format!(
            "POST {path} HTTP/1.1\r\nHost: {}\r\nX-Auth-PSK: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
            ctx.opts.tv_host,
            ctx.opts.tv_psk,
            body.len()
        );
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(body.as_bytes()).await?;

        // Only the status line matters.
        let mut resp = Vec::new();
        let mut buf = [0u8; 512];
        while !resp.contains(&b'\n') {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            resp.extend_from_slice(&buf[..n]);
        }
        let status_line = String::from_utf8_lossy(&resp);
        let status_line = status_line.lines().next().unwrap_or("");
        let status: u16 = status_line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        if !(200..300).contains(&status) {
            return Err(format!("HTTP {path}: {status_line:?}").into());
        }
        Ok::<(), Error>(())
    };
    timeout(Duration::from_secs(6), req).await.map_err(|_| format!("HTTP {path}: timed out"))?
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

// Sony's IRCC/appControl endpoints time out on concurrent requests, so all
// TV-bound requests are serialized behind a lock.
async fn dispatch(ctx: &Ctx, action: Action, quiet: bool) {
    let result = match action {
        Action::Nad(cmd) => handle_nad(ctx, cmd).await,
        Action::Ircc(_, code) => {
            let _guard = ctx.tv_lock.lock().await;
            handle_ircc(ctx, code).await
        }
        Action::App(_, uri) => {
            let _guard = ctx.tv_lock.lock().await;
            handle_app(ctx, uri).await
        }
    };
    match result {
        Ok(()) if !quiet => log(&format!("OK {}", action.label())),
        Ok(()) => {}
        Err(e) => log(&format!("ERROR {}: {e}", action.label())),
    }
}

// Logs the repeat count when the hold task ends, including when it is aborted.
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

async fn hold_repeat(ctx: Arc<Ctx>, action: Action) {
    let mut report = HoldReport { label: action.label(), count: 0 };
    sleep(HOLD_DELAY).await;
    let deadline = Instant::now() + HOLD_MAX;
    while Instant::now() < deadline {
        dispatch(&ctx, action, true).await;
        report.count += 1;
        sleep(HOLD_INTERVAL).await;
    }
    log(&format!("Hold {} stopped after {}s without release", report.label, HOLD_MAX.as_secs()));
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
    hold: Option<JoinHandle<()>>,
}

impl KeyState {
    fn stop_hold(&mut self) {
        if let Some(task) = self.hold.take() {
            task.abort();
        }
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
                    keys.hold = Some(tokio::spawn(hold_repeat(ctx.clone(), action)));
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
        tv_lock: Mutex::new(()),
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
