# NAD/Bravia Remote Bridge

A Home Assistant add-on that turns a Bluetooth LE remote control into a bridge between a NAD amplifier and a Sony Bravia (Google TV) television:

- **Volume up / down / mute** are sent directly to the NAD amplifier over its native TCP protocol (port 30001).
- **Every other button** (navigation, Home, Return, EPG, playback controls, channel up/down, and app-launch buttons such as Netflix, YouTube, Disney+, Prime Video, Sony Pictures Core) is relayed to the TV over the network, using Sony's legacy IRCC API for simple key commands and the `appControl` API to launch specific apps directly.

## Why

Cheap/legacy amplifiers like the NAD C338 have no HDMI-CEC. A TV's "external audio system" output mode only *tries* to forward volume via CEC to whatever it thinks is connected — if nothing real answers, the TV doesn't track any state HA can see, and nothing happens. This add-on sidesteps that entirely: the physical remote is paired directly to the Home Assistant host over Bluetooth LE (instead of the TV), and every button press is decoded from the raw HID Consumer-Page usage code and re-dispatched over the network to whichever device should actually handle it.

## How it works

1. The remote (tested with a Sony RMF-TX920U) is Bluetooth-paired to the Home Assistant host. Note: most modern Sony/Google-TV remotes are BLE-only (no IR fallback), so pairing it here means it can no longer control the TV directly — that control is fully replaced by this add-on's network relay.
2. The add-on (a small static Rust binary) connects to BlueZ over D-Bus, subscribes to GATT notifications on the remote's Consumer-Control HID report characteristic, and decodes each 16-bit usage code.
3. Each code is looked up in a small dispatch table and either:
   - sent straight to the NAD over its plain-text TCP protocol (`Main.Volume+`, `Main.Volume-`, `Main.Mute=On/Off`), or
   - sent to the TV as an IRCC command (`X_SendIRCC` SOAP call) or an app launch (`appControl.setActiveApp` with the app's URI), both authenticated with the TV's pre-shared key.
4. Press-and-hold: volume up/down, the four arrow keys and channel up/down auto-repeat while held. The first press is sent immediately; after 0.4 s the command repeats every 0.15 s until the remote reports the button release. All other buttons fire once per press. The timing (`HOLD_DELAY`, `HOLD_INTERVAL`) and the set of repeating keys (`Action::repeatable`) live in `src/main.rs`.
5. Automatic reconnect: if the Bluetooth connection to the remote drops, the add-on retries in the background.

Bravia/NAD-side details — installed app URIs, IRCC command codes, the remote's raw HID code table — are queried live from the TV itself (`getRemoteControllerInfo`, `getApplicationList`) rather than hardcoded from guesswork, so they should transfer to other Bravia models reasonably well; the exact HID codes are specific to the RMF-TX920U and would need re-capturing for a different remote model.

## Requirements

- Home Assistant OS (or Supervised) on hardware with a Bluetooth LE adapter reachable via the host's D-Bus/BlueZ (the add-on uses `host_dbus: true`).
- A NAD amplifier reachable on the network with its native TCP control port open (tested against a NAD C338, port 30001).
- A Sony Bravia (Google TV) with IP Control enabled and a pre-shared key configured (Settings → Network → Home Network → IP Control).
- The remote paired to the Home Assistant host's Bluetooth adapter *before* starting the add-on (e.g. via `bluetoothctl` — pair, trust, connect).

## Installation

The add-on image is pre-built by GitHub Actions for `amd64` and `aarch64` and published to the GitHub Container Registry (`ghcr.io/mschneider-git/nadbridge-{arch}`), so Home Assistant only downloads a ~2 MB image instead of building anything on the device.

1. In Home Assistant: **Settings → Add-ons → Add-on Store → ⋮ → Repositories**, add `https://github.com/mschneider-git/nad-bravia-bridge`.
2. Find "NAD/Bravia Remote Bridge" in the store and install it (Alternatively, copy `config.yaml` into `/addons/nadbridge/` and install it from **Local add-ons**; it still pulls the pre-built image.)
3. Set the options (`remote_mac`, `nad_host`, `nad_port`, `tv_host`, `tv_psk`) to match your setup.
4. Start the add-on and enable "Start on boot".

## Building

- Every push to `main` (and every manual run of the "Build add-on images" workflow) builds both architectures and pushes them tagged with the `version` from `config.yaml`, so bump that version for each release. Pull requests only check that the images build.
- The container packages on ghcr.io must be **public**, otherwise Home Assistant cannot pull them (GitHub → Packages → `nadbridge-amd64` / `nadbridge-aarch64` → Package settings → Change visibility).
- The image is `FROM scratch` with a single statically linked (musl) binary. It is cross-compiled on the build host, so no emulation is needed. To build locally: `docker buildx build --platform linux/arm64 -t nadbridge .`
- Log timestamps are in UTC, since the image has no timezone database.

## Configuration options

| Option | Description |
|---|---|
| `remote_mac` | Bluetooth MAC address of the paired remote |
| `nad_host` / `nad_port` | IP and TCP port of the NAD amplifier |
| `tv_host` | IP of the Bravia TV |
| `tv_psk` | Pre-shared key configured for IP Control on the TV |

## Adapting to your own remote / TV model

The HID usage code → action mapping, including the IRCC codes and app URIs, lives in `key_action()` in `src/main.rs`. To add support for a different remote:

1. Subscribe to GATT notifications on the remote's Consumer-Control report characteristic (BlueZ `org.bluez.GattCharacteristic1.StartNotify` over D-Bus) while pressing each button, and note the 16-bit little-endian usage code.
2. Query your TV's actual supported commands and installed apps live (`getRemoteControllerInfo` / `getApplicationList` over its REST API) rather than assuming a generic list — button names and available apps vary by model and region.
3. Extend `key_action()` in `src/main.rs` accordingly.

## Known limitations

- Sony's IRCC/`appControl` endpoints do not handle concurrent requests well (they time out); the add-on serializes all TV-bound requests behind a lock. NAD requests are not affected.

## License

MIT, see [LICENSE](LICENSE).
