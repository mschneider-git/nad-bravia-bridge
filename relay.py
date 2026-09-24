import asyncio
import json
import os
import time
import urllib.request
from dbus_fast import BusType, Message, MessageType
from dbus_fast.aio import MessageBus

REMOTE_MAC = os.environ["REMOTE_MAC"]
DEV_PATH = "/org/bluez/hci0/dev_" + REMOTE_MAC.replace(":", "_").upper()
CHAR_PATH = DEV_PATH + "/service0080/char008c"

NAD_HOST = os.environ["NAD_HOST"]
NAD_PORT = int(os.environ["NAD_PORT"])

TV_HOST = os.environ["TV_HOST"]
TV_PSK = os.environ["TV_PSK"]

IRCC_CODES = {
    "Home": "AAAAAQAAAAEAAABgAw==",
    "Return": "AAAAAgAAAJcAAAAjAw==",
    "Up": "AAAAAQAAAAEAAAB0Aw==",
    "Down": "AAAAAQAAAAEAAAB1Aw==",
    "Left": "AAAAAQAAAAEAAAA0Aw==",
    "Right": "AAAAAQAAAAEAAAAzAw==",
    "Confirm": "AAAAAQAAAAEAAABlAw==",
    "PlayPause": "AAAAAgAAAJcAAAAPAw==",
    "ChannelUp": "AAAAAQAAAAEAAAAQAw==",
    "ChannelDown": "AAAAAQAAAAEAAAARAw==",
    "EPG": "AAAAAgAAAKQAAABbAw==",
    "Netflix": "AAAAAgAAABoAAAB8Aw==",
    "TvInput": "AAAAAQAAAAEAAAAlAw==",
    "YouTube": "AAAAAgAAAMQAAABHAw==",
    "Help": "AAAAAgAAAMQAAABNAw==",
    "GoogleDashboard": "AAAAAgAAAMQAAABwAw==",
}

APP_URIS = {
    "TV": "com.sony.dtv.com.sony.dtv.tvlin.com.sony.dtv.tvlin.view.MainActivity",
    "Settings": "com.sony.dtv.com.android.tv.settings.com.android.tv.settings.MainSettings",
    "DisneyPlus": "com.sony.dtv.com.disney.disneyplus.com.bamtechmedia.dominguez.main.MainActivity",
    "PrimeVideo": "com.sony.dtv.com.amazon.amazonvideo.livingroom.com.amazon.ignition.IgnitionActivity",
    "SonyPicturesCore": "com.sony.dtv.com.sonypicturescore.com.sphe.bravialounge.SplashActivity",
    "ARDMediathek": "com.sony.dtv.de.swr.avp.ard.tv.de.swr.avp.ard.tv.TvActivity",
}

KEY_MAP = {
    0x00E9: ("nad", "volume_up"),
    0x00EA: ("nad", "volume_down"),
    0x00E2: ("nad", "mute"),
    0x0223: ("ircc", "Home"),
    0x0224: ("ircc", "Return"),
    0x0042: ("ircc", "Up"),
    0x0043: ("ircc", "Down"),
    0x0044: ("ircc", "Left"),
    0x0045: ("ircc", "Right"),
    0x0041: ("ircc", "Confirm"),
    0x00CD: ("ircc", "PlayPause"),
    0x009C: ("ircc", "ChannelUp"),
    0x009D: ("ircc", "ChannelDown"),
    0x008D: ("ircc", "EPG"),
    0x0547: ("ircc", "Netflix"),
    0x0533: ("ircc", "TvInput"),
    0x04E5: ("ircc", "YouTube"),
    0x03C3: ("ircc", "Help"),
    0x051F: ("ircc", "GoogleDashboard"),
    0x0089: ("app", "TV"),
    0x0586: ("app", "Settings"),
    0x04EB: ("app", "DisneyPlus"),
    0x04EA: ("app", "PrimeVideo"),
    0x04F0: ("app", "SonyPicturesCore"),
    0x04FB: ("app", "ARDMediathek"),
}

# Keys that auto-repeat while held down: the first press is sent immediately,
# then after HOLD_DELAY the action repeats every HOLD_INTERVAL until release.
REPEATABLE = {
    ("nad", "volume_up"),
    ("nad", "volume_down"),
    ("ircc", "Up"),
    ("ircc", "Down"),
    ("ircc", "Left"),
    ("ircc", "Right"),
    ("ircc", "ChannelUp"),
    ("ircc", "ChannelDown"),
}
HOLD_DELAY = 0.4
HOLD_INTERVAL = 0.15
# Safety net in case the release notification is lost.
HOLD_MAX = 30


def log(msg):
    print(f"[{time.strftime('%Y-%m-%d %H:%M:%S')}] {msg}", flush=True)


async def nad_send(cmd):
    reader, writer = await asyncio.open_connection(NAD_HOST, NAD_PORT)
    try:
        writer.write(cmd.encode("utf-8"))
        await writer.drain()
        try:
            data = await asyncio.wait_for(reader.read(256), timeout=1.5)
        except asyncio.TimeoutError:
            data = b""
    finally:
        writer.close()
    return data.decode("utf-8", "ignore")


async def handle_nad(action):
    if action == "volume_up":
        await nad_send("Main.Volume+\r")
    elif action == "volume_down":
        await nad_send("Main.Volume-\r")
    elif action == "mute":
        resp = await nad_send("Main.Mute?\r")
        currently_on = "Main.Mute=On" in resp
        await nad_send(f"Main.Mute={'Off' if currently_on else 'On'}\r")


def _http_post(path, headers, body):
    req = urllib.request.Request(
        f"http://{TV_HOST}{path}", data=body.encode("utf-8"), headers=headers, method="POST"
    )
    with urllib.request.urlopen(req, timeout=6) as resp:
        return resp.status


async def handle_ircc(name):
    code = IRCC_CODES[name]
    body = (
        '<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" '
        's:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">'
        "<s:Body>"
        '<u:X_SendIRCC xmlns:u="urn:schemas-sony-com:service:IRCC:1">'
        f"<IRCCCode>{code}</IRCCCode>"
        "</u:X_SendIRCC></s:Body></s:Envelope>"
    )
    headers = {
        "X-Auth-PSK": TV_PSK,
        "SOAPACTION": '"urn:schemas-sony-com:service:IRCC:1#X_SendIRCC"',
        "Content-Type": "text/xml; charset=UTF-8",
    }
    await asyncio.to_thread(_http_post, "/sony/ircc", headers, body)


async def handle_app(name):
    uri = APP_URIS[name]
    body = json.dumps({"method": "setActiveApp", "id": 1, "params": [{"uri": uri}], "version": "1.0"})
    headers = {"X-Auth-PSK": TV_PSK, "Content-Type": "application/json"}
    await asyncio.to_thread(_http_post, "/sony/appControl", headers, body)


_tv_lock = asyncio.Lock()


async def dispatch(target, action, quiet=False):
    try:
        if target == "nad":
            await handle_nad(action)
        elif target == "ircc":
            async with _tv_lock:
                await handle_ircc(action)
        elif target == "app":
            async with _tv_lock:
                await handle_app(action)
        if not quiet:
            log(f"OK {target}/{action}")
    except asyncio.CancelledError:
        raise
    except Exception as e:
        log(f"ERROR {target}/{action}: {e}")


async def hold_repeat(target, action):
    loop = asyncio.get_running_loop()
    count = 0
    try:
        await asyncio.sleep(HOLD_DELAY)
        deadline = loop.time() + HOLD_MAX
        while loop.time() < deadline:
            await dispatch(target, action, quiet=True)
            count += 1
            await asyncio.sleep(HOLD_INTERVAL)
        log(f"Hold {target}/{action} stopped after {HOLD_MAX}s without release")
    finally:
        if count:
            log(f"Hold {target}/{action} repeated {count}x")


def stop_hold(state):
    task = state.get("hold_task")
    if task is not None:
        task.cancel()
        state["hold_task"] = None


async def call(bus, path, iface, member, signature="", body=None, destination="org.bluez"):
    msg = Message(destination=destination, path=path, interface=iface, member=member,
                  signature=signature, body=body or [])
    reply = await bus.call(msg)
    if reply.message_type == MessageType.ERROR:
        raise Exception(f"{member} on {path} failed: {reply.body}")
    return reply


def make_handler(loop, state):
    def handler(msg):
        if msg.message_type != MessageType.SIGNAL or msg.member != "PropertiesChanged":
            return
        if msg.interface != "org.freedesktop.DBus.Properties":
            return
        iface, changed, _ = msg.body

        if msg.path == DEV_PATH and iface == "org.bluez.Device1" and "Connected" in changed:
            connected = changed["Connected"].value
            log(f"Device Connected={connected}")
            state["connected"] = connected
            if not connected:
                stop_hold(state)
                state["last_code"] = None
                loop.create_task(reconnect_loop(state["bus"], state))

        if msg.path == CHAR_PATH and iface == "org.bluez.GattCharacteristic1" and "Value" in changed:
            value = bytes(changed["Value"].value)
            code = value[0] | (value[1] << 8)
            if code == 0:
                stop_hold(state)
                state["last_code"] = None
                return
            if code == state.get("last_code"):
                return
            stop_hold(state)
            state["last_code"] = code
            log(f"RX code=0x{code:04x}")
            if code in KEY_MAP:
                target, action = KEY_MAP[code]
                loop.create_task(dispatch(target, action))
                if (target, action) in REPEATABLE:
                    state["hold_task"] = loop.create_task(hold_repeat(target, action))
            else:
                log(f"Unknown code 0x{code:04x}, ignoring")

    return handler


async def reconnect_loop(bus, state):
    if state.get("reconnecting"):
        return
    state["reconnecting"] = True
    try:
        while not state.get("connected"):
            try:
                log("Attempting to reconnect to remote...")
                await call(bus, DEV_PATH, "org.bluez.Device1", "Connect")
                await call(bus, CHAR_PATH, "org.bluez.GattCharacteristic1", "StartNotify")
                state["connected"] = True
                log("Reconnected and subscribed.")
            except Exception as e:
                log(f"Reconnect failed: {e}")
                await asyncio.sleep(10)
    finally:
        state["reconnecting"] = False


async def main():
    bus = await MessageBus(bus_type=BusType.SYSTEM).connect()
    loop = asyncio.get_event_loop()
    state = {"connected": False, "last_code": None, "bus": bus, "reconnecting": False,
             "hold_task": None}
    bus.add_message_handler(make_handler(loop, state))

    await call(bus, "/org/freedesktop/DBus", "org.freedesktop.DBus", "AddMatch", "s",
               ["type='signal',interface='org.freedesktop.DBus.Properties',member='PropertiesChanged'"],
               destination="org.freedesktop.DBus")

    try:
        await call(bus, DEV_PATH, "org.bluez.Device1", "Connect")
    except Exception as e:
        log(f"Initial connect failed, will retry: {e}")

    try:
        await call(bus, CHAR_PATH, "org.bluez.GattCharacteristic1", "StartNotify")
        state["connected"] = True
        log("Relay started and subscribed.")
    except Exception as e:
        log(f"Initial StartNotify failed: {e}")
        await reconnect_loop(bus, state)

    while True:
        await asyncio.sleep(30)
        if not state.get("connected"):
            await reconnect_loop(bus, state)


asyncio.run(main())
