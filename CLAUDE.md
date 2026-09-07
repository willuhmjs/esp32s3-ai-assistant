# esp32s3-voice-assistant — self-hosted voice assistant firmware

Bare-metal Rust (`no_std` + `alloc`), embassy-async, running on a **Spotpear
ESP32-S3 1.28" round touch box**. Goal: tap the screen, speak, get a spoken
reply from any OpenAI-compatible chat endpoint via any OpenAI-compatible
STT/TTS endpoint, fully custom firmware — no ESP-IDF, no FreeRTOS. Nothing is
compiled in: the endpoints, models and WiFi credentials are all configured on
the device itself. Development targets a self-hosted agent (Hermes) and
self-hosted Speaches for STT/TTS, but the firmware knows nothing about either
beyond the standard OpenAI paths.

The repo lives at `~/custom-os`; the crate, the binary and the GitHub repo are
all `esp32s3-voice-assistant`. Only the working directory still carries the old
`custom-os` name.

**Board compatibility.** The firmware is written against ESP32-S3 boards
pairing a GC9A01 240x240 round LCD, a CST816x touch controller and an ES8311
codec, and is tested on exactly one: the Spotpear `sp-esp32-s3-1.28-box`.
Nothing in the code abstracts over boards — `src/pins.rs` is the single source
of the GPIO map, so another board in that class is a `pins.rs` edit rather than
a port.

**Current status**: the voice pipeline works end to end — the long-running
mic/I2S-RX bug is fixed (see "Audio, and the bug that dominated early work").
On top of it there is now a card-stack UI (assistant → configure → settings), VAD
auto-stop, barge-in, swipe-to-cancel, flash-persisted settings, and a SoftAP
setup portal that is now the only way to configure the device.
**Those newer features are written and compile clean but are not yet
verified on hardware** — see "What is and isn't verified".

## Hardware

Spotpear ESP32-S3 1.28" round touch box (identified via factory-firmware
strings — it runs `xiaozhi-esp32`, board id `sp-esp32-s3-1.28-box`).
Pinout is in `src/pins.rs`, pulled from that project's own board config
(`main/boards/spotpear/sp-esp32-s3-1.28-box/config.h` in
github.com/78/xiaozhi-esp32) — treat those pin numbers as ground truth.

- Display: GC9A01 round LCD, 240x240, SPI2
- Touch: CST816D, I2C1, address 0x15
- Audio codec: ES8311 (single chip, both mic ADC and speaker DAC), I2C0 for
  control registers, I2S0 for audio data (full duplex, MCLK/BCLK/WS shared)
- Status LED: WS2812 (addressable RGB) on GPIO48, driven via RMT
- 16MB flash, 8MB octal PSRAM
- Board's original firmware is fully backed up at
  `~/esp32s3-backup/full_flash_backup_16MB.bin` — restore with:
  `esptool --port <port> write-flash 0x0 ~/esp32s3-backup/full_flash_backup_16MB.bin`

## Architecture / file map

- `src/bin/main.rs` — peripheral init, WiFi bring-up, settings load, and the
  main state machine (Idle → Listening → Thinking → Speaking → Idle) plus the
  settings card, all inline in one big `async fn main`. This flat structure
  is deliberate — reqwless's `HttpClient` and lcd-async's `Display` have
  hairy generic types that are painful to name in separate function
  signatures, so almost everything lives in one scope and the helpers that DO
  exist (`speak`, `enter_portal`, `show`, `find_sentence_end`, `drain_taps`,
  `configure_menu`, `setting_entries`, `save_settings`, `mean_abs_level`) take
  concrete/generic params explicitly.
- `src/ui.rs` — round-display UI. `UiState` enum (Idle/Listening/Thinking/
  Speaking/Error/Menu/Portal/Notice) + one `render()` function generic over
  the display interface type, called with a persistent PSRAM framebuffer.
  Menu geometry (`MENU_X0/X1`, `menu_row_top`, `menu_row_at`, `row_zone`)
  is shared between drawing and hit-testing so the two can't drift apart.
- `src/touch.rs` — CST816D driver plus `TouchEvent` (Tap/SwipeLeft/Right/
  Up/Down) and `classify()`.
- `src/settings.rs` — runtime-editable config, persisted to the `nvs`
  partition. This is the **only** source of configuration — there are no
  compile-time defaults, so a device with no valid record is "unconfigured"
  and boots into the setup portal.
- `src/portal.rs` — SoftAP setup portal (DHCP + captive DNS + HTTP form).
- `src/wav.rs` — WAV header build/parse helpers.
- `src/pins.rs` — board pinout constants.
Every module is a `#[path = "../x.rs"] mod x;` include into the bin crate
(not a normal crate module, since it's included from `src/bin/`), so inside
them `crate::` refers to the binary.

**There is no `src/secrets.rs`, no `.env` and no `build.rs` dotenv loading any
more** — they were deleted when compile-time config was removed. The built
binary contains no credentials, so `target/` artifacts are no longer sensitive.
If you find a stale `.env` at the repo root it is unused; nothing reads it.

## Configuration model

Everything the firmware needs at runtime — WiFi SSID/password, both base URLs,
both API keys, three model names — lives in one flash record and is entered on
the device through the setup portal. `settings::FIELD_ORDER` is the list;
`Field::is_required()` marks which ones the device cannot run without (the two
API keys and the WiFi password are optional, since unauthenticated endpoints
and open networks are legitimate).

**Base URLs carry the API prefix.** `Field::AgentBaseUrl` /
`Field::SpeechBaseUrl` are `host[:port][/prefix]` with no scheme and no
trailing slash — e.g. `llm.example.edu/v1` or `chat.example.net/api/v1`. The
firmware only ever appends a standard OpenAI operation path
(`settings::OP_CHAT_COMPLETIONS`, `OP_TRANSCRIPTIONS`, `OP_SPEECH`) via
`Settings::endpoint()`. This exists because the prefix genuinely differs
between servers, and a hardcoded `/api/v1` was silently 404ing against a host
rooted at `/v1`. `Settings::set()` normalizes what gets pasted in: it strips
the scheme, a trailing slash, and a trailing operation path.

The agent side is plain OpenAI SSE streaming. `event: hermes.tool.progress`
lines are a Hermes-specific extension used for the tool-progress label;
servers that don't emit them just never hit that branch. STT/TTS format
matches the device's audio natively — 24kHz mono 16-bit PCM, zero resampling.
TLS is reqwless + embedded-tls with `TlsVerify::None`, since these are
self-signed/internal-cert endpoints.

**Boot goes straight to the setup portal** when `Settings::first_missing()`
reports a blank required field (first boot, or after Erase Settings) or when
the saved network fails to join within `WIFI_JOIN_TICKS`. Both paths call
`enter_portal()`, which is also what the Configure card's "Wi-Fi Setup" row
calls — one code path, three entry points, and it returns `!`. The one case
that deliberately does *not* force the portal is a missing `nvs` partition:
with nowhere to save, entering it would reboot into the same state forever.

## UI / navigation model

Two cards. Swipe **left** from the assistant card to reach settings, swipe
**right** to come back (volume changes are flushed to flash on the way out,
so holding "+" doesn't erase a flash sector per tap).

Swipe direction is derived from the first and last coordinates of a touch,
**not** from the CST816's own gesture byte — the chip's vertical labels are
inverted relative to this panel's orientation (a physical downward swipe
reports `SlideUp`). `touch::SWIPE_MIN_PX` (40) is the tap/swipe threshold.

Settings rows are `ROW_WIFI` / `ROW_VOLUME` / `ROW_RESET`. The volume row is
a stepper: `ui::row_zone(x)` splits it into a decrement zone, a body, and an
increment zone. Erase is two-tap confirmed and reboots afterwards.

## Settings persistence

`src/settings.rs` writes a magic+version+CRC32 record to the ESP-IDF-style
`nvs` data partition (0x9000, 24 KB) via `esp-storage 0.9` — **not** the NVS
key/value format, just a raw record in that partition's space. Any failure
(bad magic, bad CRC, wrong version, no partition) reads as *unconfigured*, so
a corrupt record can't brick the device — it sends it to the setup portal.
`VERSION` is currently **2**; bumping it deliberately invalidates every
existing record, which is the right behaviour whenever `FIELD_ORDER` or a
field's meaning changes.

**The one non-obvious constraint**: `esp-storage` disables the instruction
cache for the duration of a flash operation, and on the ESP32-S3 PSRAM is
reached *through* that cache. Every buffer handed to a flash read or write
must therefore be in internal RAM. Since the global allocator is the PSRAM
one, any `Vec` would be wrong — hence the `static mut SCRATCH` /
`TABLE_SCRATCH` arrays in `.bss` and the `encode_into(&mut [u8])` shape
instead of a `encode() -> Vec<u8>`. A big stack array is also not an option
here given this project's history with stack overflow.

`esp-storage` is pinned to **0.9**: 0.10 requires esp-hal ~1.2.0-rc.0, and
`ws2812-rmt 0.2.0` is hard-pinned to esp-hal ~1.1.0, so bumping it breaks
the status LED.

Related: `stack.wait_config_up()` at boot is wrapped in a 25-second
`with_timeout`. Bad saved credentials must not be able to lock the user out
of the settings menu, which is the only place they can be fixed.

## The setup portal (`src/portal.rs`)

Tapping "Wi-Fi Setup" is a **one-way door**: it signals `connection_task`,
which abandons the station reconnect loop and `set_config`s the radio into
AP mode (`voice-assistant-setup`, open, 192.168.4.1). The station interface is gone
at that point, so every exit path — saved, save-failed, no-nvs, cancelled —
ends in `software_reset()`.

Three servers run concurrently on the AP stack:
- **DHCP** (:67) — a phone gets no address otherwise. Also advertises the
  RFC 8910 captive-portal URL option.
- **DNS** (:53) — every A query answers 192.168.4.1, so whatever probe URL
  the phone tries resolves to us.
- **HTTP** (:80) — answers *every* GET with the settings form, including
  `/generate_204` and `/hotspot-detect.html`. Handing HTML to a connectivity
  probe is exactly what makes the OS decide it's behind a captive portal and
  open the page by itself. `POST /save` is the only special-cased path.

Secrets (WiFi password, both API keys) render as empty `type=password` boxes
with an "unchanged" placeholder and are **never echoed back** over an open
AP; an empty submission means "keep the stored value" (see `apply_form`).

`edge-dhcp` and `edge-captive` are pulled in with `default-features = false`.
That drops their `io` feature, which needs `edge-nal-embassy` — and the only
release of that compatible with edge-net 0.8 is pinned to **embassy-net 0.8**,
which would have landed a second, incompatible copy of embassy-net next to
the 0.9 used everywhere else. Without `io` they are pure codecs, driven over
embassy-net's own sockets in `dhcp_task`/`dns_task`.

Both network stacks are created at boot, because both interfaces come out of
the same `esp_radio::wifi::new()` call — the alternative is keeping
`Interfaces` alive across the whole program just to consume it later. The AP
stack's socket buffers are heap (PSRAM) `Vec`s, which is safe here in a way
it is not for DMA: embassy-net only touches them with CPU loads/stores, and
the one window where PSRAM is unreachable (esp-storage's cache-off flash
write) runs inside a critical section.

## Audio

- **Full duplex on one codec.** Only GPIO9/GPIO45 carry BCLK/WS and the GPIO
  matrix can route one peripheral output to a pad, so TX owns the clocks and
  `with_signal_loopback(true)` makes RX ride them (RX slave, TX master).
  Without it the RX unit free-runs on a clock the ES8311 never sees, DIN
  stays static, and every recording is digital silence. A side effect worth
  remembering: mic and speaker are locked to the same 24 kHz rate.
- **`I2S_TX_STOP_EN` is deliberately cleared** and TX kicked once at init, so
  BCLK/WS free-run for the whole program. esp-hal sets that bit, which would
  gate the clocks off whenever the TX FIFO drains — leaving the codec
  unclocked except while actually playing audio, and so unable to shift ADC
  data out at all.
- **`RX_CHUNK_LEN = 4092` is load-bearing.** esp-hal's `rx_start()` programs
  `I2S_RX_EOF_NUM` straight from the byte length, but that register is 12
  bits and counts in units of `(RX_BITS_MOD + 1)` bits, so most lengths ask
  the hardware to signal EOF at the wrong point. 4092 is one full GDMA
  descriptor (esp-hal's `CHUNK_SIZE`) and the largest length that reliably
  completes and fills the whole buffer; 6144/8184/16384 either error out with
  `DmaError(DescriptorError)` or return `Ok` after a partial transfer.
- **DMA cannot read PSRAM**, so playback stages through the internal-RAM
  `tx_buffer` from `dma_buffers!(16384, 8192)`. `rx_buffer` has to come from
  that macro too — GDMA RX descriptors need word-aligned memory.
- **ES8311 register fixes.** The `es8311` crate (crates.io, MIT, by
  QuackHack-McBlindy) misses several registers the vendor driver always
  writes (`es8311_start()` in
  `esp-adf/components/esp_codec_dev/device/es8311/es8311.c`); without them
  the ADC produces literal silence. Raw I2C pokes are layered on top of the
  crate, in `main.rs` right after `codec.microphone_gain_set(...)`. Two of
  them are counter-intuitive:
  - **`0x16 = 0x24`.** The crate's `microphone_gain_set` treats REG16 as a
    6-bit 0x00..0x3F gain field and writes 0x28 for 24 dB, but the real field
    is bits [2:0]; 0x28 makes the ADC output *exact digital zero*. Mic gain
    actually lives in REG14's PGA field, which `microphone_config` sets.
  - **`0x44 = 0x00`.** GPIO_REG44 [6:4] is the SDOUT source mux; 0x00 is
    plain ADC data on both TDM slots. Do **not** use esp-idf's 0x50/0x58 —
    those are "ADC mixed with DAC" monitoring modes that bleed playback into
    the recording, and 0x58 keeps ADC data off SDOUT entirely. (An earlier
    version of this file recommended 0x58; that was wrong.)
- **Speaker levels**: audible at codec volume 70/100; below that is inaudible
  on this speaker/amp, and 80+ sustained for several seconds is
  uncomfortably loud. 70 is `settings::DEFAULT_VOLUME`.

### Audio, and the bug that dominated early work

`read_dma_circular_async` is **unusable** here, and the reason is in
esp-hal's `RxCircularState`: `update()` walks the descriptors the DMA has
handed back and returns `DmaError::Late` the moment it finds the *whole* ring
is CPU-owned — i.e. we didn't drain in time. Descriptors are only ever handed
back to the DMA inside `pop()`, which `update()` returns before reaching, so a
single missed deadline wedges the transfer permanently. That matches what was
measured: after simulating a 10-second gap, 3.7 million consecutive polls over
10 seconds produced zero successes and no recovery. `pop()` also rejects any
read buffer smaller than the accumulated `available`, which is a second way
to wedge it. The transfer consumes the `I2sRx` with no way to get it back, so
there is no reset path — the only safe use would be "never stop polling for
the program's whole lifetime", which cannot be promised across WiFi/TLS work.

Two other findings from that investigation, both still true and both worth
not rediscovering:

- **A circular DMA transfer handed to a spawned `#[embassy_executor::task]`
  errors on ~100% of reads**, while the identical object polled from
  `main()`'s own task gives zero errors. Recording is therefore done inline
  in `main()`.
- **A persistent circular TX transfer running continuously corrupts RX.** TX
  now uses the one-shot, non-consuming `write_dma_async(&mut self, ...)`, so
  it sits fully idle except while actually pushing audio. Keep it that way.
  (This turned out not to be the root cause of the RX failures, but it is a
  good change on its own.)

The fix was to drop circular transfers entirely: one-shot reads have no
carried state, since each call resets the RX unit and rebuilds the descriptor
chain from scratch.

Also from that era, and unrelated but load-bearing: the discretionary
internal-SRAM heap pool was shrunk from 64KB to 8KB to fix a real stack
overflow in `main()`'s state machine. PSRAM handles all the big allocations.
**Do not revert that.**

## Voice activity detection

The Listening loop ends on whichever comes first: the user tapping, ~0.9 s of
silence after they've clearly spoken, or a 15 s hard cap. One RX chunk is
2045 samples ≈ 85 ms at 24 kHz, which is the unit every VAD constant counts
in (`VAD_CALIBRATION_CHUNKS`, `VAD_HANG_CHUNKS`, `VAD_NO_SPEECH_CHUNKS`).

The threshold calibrates against the first few chunks of the actual room
rather than a fixed number, with `VAD_MIN_SPEECH_LEVEL` as an absolute floor
so a dead-silent room can't set a threshold low enough for fan noise to read
as talking. The level metric is mean-absolute-value, not RMS, because `core`
has no `sqrt` without pulling in `libm`.

`speak()` returns `true` if the user tapped to cut it off (barge-in); the SSE
loop uses that to abandon the rest of the reply. The assistant's text is
still pushed to history either way, so history matches what was heard.

## Touch event handling

A spawned `touch_task` blocks on the controller's INT line (with a 500 ms
poll fallback that should never fire), tracks first/last coordinates for the
duration of a touch, and sends one `TouchEvent` on release into
`TOUCH_EVENTS: Channel<_, TouchEvent, 8>`.

**Important**: taps that arrive during boot/wifi-connect (people naturally
tap a screen to check it's alive) get queued and, if not drained, get
misinterpreted as start/stop signals once the main loop starts, corrupting
the first few turns. `drain_taps()` (a `while try_receive().is_ok() {}` loop)
is called before the main loop starts, right after consuming the "start
listening" tap, and around each confirmation prompt.

## What is and isn't verified

Verified on hardware: display orientation (plain `Orientation::new()` — do
**not** `.flip_horizontal()`, that mirrors text backwards, unlike the
original ESP-IDF firmware which needed mirroring for its own driver); touch
giving real distinct coordinates; WiFi association; TLS to all three
endpoints; the audio pipeline end to end; WS2812 via `ws2812-rmt` 0.2.0
(`Timing::WS2812B_AT_12_5NS_TICK_ESP32C3` with `clk_divider=1` — correct for
the S3 too, same APB-clock RMT source as the C3).

Also verified: three-card swipe navigation (assistant → configure →
settings), a tap landing on the Wi-Fi Setup row, the radio switching to
`voice-assistant-setup`, cancel-and-reboot, and swipe-to-cancel during Listening
(`listening loop exited: cancelled` in the serial log).

**Not yet verified on hardware** (written, compiles clean):
- Touch-coordinate-to-display-coordinate mapping for menu hit-testing, and
  swipe polarity. This is the thing most likely to need physical iteration.
- The volume stepper, erase-and-reboot.
- VAD auto-stop thresholds and barge-in.
- Settings persisting across a reboot.
- Swipe-to-cancel during Thinking (`turn cancelled during transcription` /
  `turn cancelled during reply`).
- The status LED following the on-screen state (`ui::led_color`).
- The whole portal: AP bring-up, DHCP handout, the captive-portal sheet
  actually popping, form round-trip, save-and-reboot.
- **The entire no-compile-time-config change**: boot-into-portal on a blank
  record, boot-into-portal on a failed join, the v1→v2 record invalidation,
  and base URLs with an API prefix actually producing working request URLs.

Grep the serial log for `touch (x,y) -> (x,y) = Event` to check the mapping,
and `portal: METHOD path` / `portal: dhcp ...` for the portal.

## Build / flash / test workflow

```
source "$HOME/.cargo/env" && source ~/export-esp.sh   # every new shell
cd ~/custom-os
ESP_LOG=info cargo build --release   # ESP_LOG=info needed for log::info!() to show anything
```

Find the port first — it changes across replugs/reboots:
```
ls /dev/cu.usbmodem*
```

**Flashing + capturing output reliably** (avoids the classic "missed the
first few seconds of boot" race from running `espflash flash` then a
separate `cat` afterward — the first ~100ms to few seconds of boot output is
silently dropped if nothing has the port open when the chip resets):
```
espflash flash --port /dev/cu.usbmodemXXXX --monitor --non-interactive target/xtensa-esp32s3-none-elf/release/esp32s3-voice-assistant > /tmp/some.log 2>&1 &
FLASHPID=$!
sleep 20   # however long you need
kill $FLASHPID 2>/dev/null; wait $FLASHPID 2>/dev/null
```
(`--monitor` alone needs a real TTY for its Ctrl+C/Ctrl+R handling and fails
in a non-interactive shell; `--non-interactive` avoids that.)

For open-ended live interaction testing (waiting on the user to physically
tap the device), flash first, then start a **detached** capture —
`nohup cat /dev/cu.usbmodemXXXX > /tmp/log 2>&1 &` — since a plain background
job dies when the shell invocation ends. Kill it with
`pkill -f "cat /dev/cu.usbmodem"` once the user says they're done.

**Always `pkill -f espflash` and `pkill -f "cat /dev/cu.usbmodem"` before a
new flash attempt** — a stale background capture or flash process holding
the port causes `Device or resource busy` or `Failed to open serial port`.

The device backlight/screen going blank or a total silence in serial output
almost always means a panic (stack overflow, unwrap on a real error, etc.)
happened before reaching whatever log line you expected — the
`--monitor --non-interactive` combo will show the actual panic + backtrace
if so; don't assume "no output" means "hung", check for a panic first.

## Known open items

- **Espressif wake-word (ESP-SR) was researched and shelved.** Findings worth
  keeping: `esp_sr_sys 0.1.0` has no esp-hal/esp-idf dependency (just `libm`
  + `cc`), bundles all ten Espressif `.a` files, and `include_bytes!`es the
  packed `srmodels.bin` directly — so **no partition-table work is needed**.
  The image is currently ~1.1 MB of 4.1 MB. Remaining blockers: ESP-SR is
  hard-locked to 16 kHz / 512-sample frames while our TX/RX clocks are shared
  at 24 kHz; internal-SRAM budget (AEC alone wants ~122 KiB, we have ~72 KB
  discretionary); "Hi ESP" is the only bundled English wake word; and both
  `esp_sr_sys` and `ember_esp_sr` are unvetted 0.1.0 single-author releases.
- `wav::find_data_chunk` is unused — either wire it in (more robust than the
  hardcoded 44-byte WAV header skip in `speak()`) or remove it.
- Several `pins.rs` constants are unused (dead-code warnings) — harmless,
  they document pins not currently driven from Rust.
- The RWX LOAD-segment linker warning is expected and pre-existing.
- Debug `println!`s from the mic investigation are still scattered around
  `main.rs`; worth trimming once the newer features are confirmed stable.
- A failed join now reboots into the setup portal. If the network is merely
  down (not misconfigured) that is a visible loop: 25 s of joining, portal,
  tap to cancel, reboot, repeat. Escapable, but if it becomes annoying the fix
  is a "keep trying offline" option on the failure notice.
