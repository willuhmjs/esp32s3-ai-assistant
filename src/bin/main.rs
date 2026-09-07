#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

extern crate alloc;

#[path = "../pins.rs"]
mod pins;
#[path = "../touch.rs"]
mod touch;
#[path = "../wav.rs"]
mod wav;
#[path = "../ui.rs"]
mod ui;
#[path = "../settings.rs"]
mod settings;
#[path = "../portal.rs"]
mod portal;

use alloc::{format, string::String, string::ToString, vec, vec::Vec};

use embassy_executor::Spawner;
use embassy_net::{
    dns::DnsSocket,
    tcp::client::{TcpClient, TcpClientState},
    Runner, StackResources,
};
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel, signal::Signal,
};
use embassy_time::{Delay, Duration, Timer};
use embedded_hal_bus::spi::ExclusiveDevice;
use embedded_io_async::Read as _;
use es8311::{ClockConfig as Es8311ClockConfig, Es8311, Resolution};
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull},
    i2c::master::{Config as I2cConfig, I2c},
    i2s::master::{Channels, Config as I2sConfig, DataFormat, I2s},
    rmt::Rmt,
    rng::Rng,
    spi::{
        master::{Config as SpiConfig, Spi},
        Mode,
    },
    time::Rate,
    timer::timg::TimerGroup,
    Async,
};
use esp_println::println;
use esp_radio::wifi::{
    ap::AccessPointConfig, sta::StationConfig, AuthenticationMethod, Config, ControllerConfig,
    Interface, WifiController,
};
use lcd_async::{
    interface::SpiInterface,
    models::GC9A01,
    options::{ColorInversion, ColorOrder, Orientation},
    Builder,
};
use log::info;
use reqwless::{
    client::{HttpClient, TlsConfig, TlsVerify},
    request::{Method, RequestBuilder},
};
use serde::{Deserialize, Serialize};
use settings::{Field, Settings};
use touch::TouchEvent;
use ui::{MenuRow, NoticeColor, UiState};
use ws2812_rmt::{buffer_len, Timing, Ws2812, RGB8};

esp_bootloader_esp_idf::esp_app_desc!();

const AUDIO_SAMPLE_RATE: u32 = pins::AUDIO_SAMPLE_RATE_HZ;
const TTS_VOICE: &str = "alloy";
const MAX_RECORDING_SECS: u64 = 15;
/// Bytes per one-shot I2S RX read - one full GDMA descriptor.
const RX_CHUNK_LEN: usize = 4092;
const IDLE_HISTORY_TIMEOUT_SECS: u64 = 120;

// ---------- Voice activity detection ----------
// One RX chunk is (RX_CHUNK_LEN - 2) bytes = 2045 samples = ~85 ms at 24 kHz,
// which is the unit all of these are counted in.

/// Chunks sampled at the start of a recording to learn the room's noise floor.
const VAD_CALIBRATION_CHUNKS: usize = 4;
/// Absolute floor for "this is speech", as mean-absolute-value of i16 samples.
/// Stops a dead-silent room from setting a threshold so low that fan noise
/// reads as talking.
///
/// Keep this *well* under the quietest speech the mic produces: in a genuinely
/// quiet room the calibrated floor lands around 8, so `noise_floor * 3` is
/// ~24 and this constant is what actually decides. Measured normal speech at
/// arm's length peaks around 110 before the PGA bump below, so 250 (the
/// previous value) silently made auto-stop impossible anywhere quiet - every
/// turn ran to "nothing heard". 60 clears the room by a wide margin while
/// still being under the quietest real speech seen.
const VAD_MIN_SPEECH_LEVEL: u32 = 60;
/// Silence after speech that ends the turn (~0.75 s).
///
/// A chunk is ~85 ms, so this is the whole knob for how patient auto-stop is,
/// and it is felt directly: it is dead time between the last word and the STT
/// request going out. Tuned by ear downwards - 14 (~1.2 s) and 12 (~1.0 s) both
/// read as waiting for the device, and 11 was the long-standing value they were
/// raised from. 9 ends the turn promptly while still being over twice the pause
/// inside an ordinary sentence.
const VAD_HANG_CHUNKS: usize = 9;
/// Give up if the user never says anything (~6 s).
const VAD_NO_SPEECH_CHUNKS: usize = 70;

/// How long a reply/audio stream may produce nothing at all before the turn is
/// abandoned. Without this a server that holds the connection open after its
/// last token wedges the state machine forever: the loop sits in `read()`, the
/// last frame drawn stays on screen, and no touch event can get it back to
/// Idle. Generous enough to cover first-token latency on a slow local model.
const STREAM_STALL_SECS: u64 = 30;
/// Frame interval for the Thinking spinner while waiting on the reply stream.
const SPINNER_FRAME_MS: u64 = 120;
/// Silence that ends a reply stream *after* tokens have started arriving.
///
/// Much shorter than `STREAM_STALL_SECS`, which has to cover a slow model's
/// whole think-before-first-token wait. Once deltas are flowing they arrive
/// milliseconds apart, so a multi-second gap means the stream is not coming
/// back - and this endpoint drops reply streams intermittently, at a random
/// point, leaving the socket open and simply never sending again. Waiting the
/// full 30s for that gained nothing but a 30s pause before a truncated answer.
const STREAM_IDLE_SECS: u64 = 8;
/// How many times to send the same reply request before giving up.
///
/// Retries exist for the dropped-stream case above and nothing else: a stream
/// that ends properly is never retried, however short the answer. Because the
/// drop lands at a random point, a second attempt usually just works.
const AGENT_ATTEMPTS: usize = 3;
/// Reply length, in characters, below which a cut-off stream counts as having
/// produced nothing and is worth resending. Above it the partial answer is
/// spoken as-is. A dropped stream typically leaves a word or two behind - "It
/// happened in nineteen thirty" - which reads to the user as the device
/// answering and then giving up mid-sentence.
const AGENT_MIN_USABLE_CHARS: usize = 40;

/// Minimum audio buffered before the first sample is played, at 24kHz/16-bit
/// mono - 48,000 bytes is one second. A floor, not the whole rule: `speak` also
/// refuses to start until the clip is *projected* to finish downloading before
/// it finishes playing (see the prebuffer comment there). This value only has
/// to absorb jitter around a delivery rate that is already fast enough, so it
/// is deliberately small - every byte is latency the user waits through.
const TTS_PREBUFFER_BYTES: usize = 48_000;
/// `(write size in bytes, seconds)` for each leg of the boot tone sweep; empty
/// disables it, which is the normal setting - it beeps for six seconds at every
/// boot. See `test_tone`: this is the only way to hear the audio path with the
/// server, the network and PSRAM all taken out of it, and the sizes are 6, 47
/// and 187 DMA boundaries a second on identical audio, which is what
/// distinguished the boundary artefact from an analog one.
const TEST_TONE_SWEEP: &[(usize, u32)] = &[];


/// How many raw SSE `data:` lines to echo to the serial log per reply. Off by
/// default: it is a blocking UART write in the middle of the read loop, so left
/// on it slows the very stream it is watching. Raise it when a server's replies
/// don't look like what it claims to be sending - that is how `delta.reasoning`
/// and the missing `[DONE]` were found.
const AGENT_TRACE_LINES: usize = 0;
/// Slack in the "can this be streamed?" projection. The estimate is built from
/// an average delivery rate, and this is how far behind that average a burst is
/// allowed to fall without the speaker running dry. Also covers the estimate
/// being wrong about the clip's total length.
const TTS_START_MARGIN_MS: u64 = 2_000;
/// Rough bytes of 24kHz PCM per character of input text, used only to advance
/// captions when the server tells us neither a Content-Length nor a real WAV
/// data size. Measured at 2,965 on this voice; being wrong here just means the
/// captions drift, never that the audio is wrong.
const TTS_BYTES_PER_CHAR: usize = 2_900;

/// Volume step per tap on the settings volume row.
const VOLUME_STEP: u8 = 5;

/// Spinner frame interval while a network request is in flight. Also the
/// granularity at which the cancel swipe is noticed during that request.
const THINKING_FRAME: Duration = Duration::from_millis(120);

/// Wifi join wait, sliced so the boot screen can keep animating through it.
/// 50 x 500ms = the same 25 second budget as before.
const WIFI_JOIN_TICK: Duration = Duration::from_millis(500);
const WIFI_JOIN_TICKS: usize = 50;

static TOUCH_EVENTS: Channel<CriticalSectionRawMutex, TouchEvent, 8> = Channel::new();

/// Raised by `button_task` when the BOOT button is pressed: throw away the
/// conversation and start fresh.
///
/// A `Signal` rather than a channel on purpose. It coalesces - three impatient
/// presses are one clear, not three - and it latches, so a press during a reply
/// takes effect the moment the turn finishes instead of being swallowed.
static NEW_CONVERSATION: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Raised by the main loop to tell `connection_task` to give up on the station
/// interface and bring the setup access point up instead. One-way: the only way
/// back to station mode is a reboot, which is what every portal exit does.
static PORTAL_REQUEST: Signal<CriticalSectionRawMutex, ()> = Signal::new();
/// Raised by `connection_task` once the radio is actually in AP mode, so the
/// main loop doesn't start serving DHCP before there is anything to serve it to.
static PORTAL_UP: Signal<CriticalSectionRawMutex, bool> = Signal::new();

/// Which card the user is looking at. Swiping left walks forward through the
/// stack (assistant -> configure -> settings) and swiping right walks back.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Assistant,
    /// Actions: Wi-Fi setup portal, volume, erase.
    Configure,
    /// Read-only list of the values currently in effect.
    Settings,
}

/// Configure menu rows, in display order. Kept as indices so the hit-testing and
/// the row construction can't drift out of sync.
const CONFIGURE_ROWS: usize = 3;
const ROW_WIFI: usize = 0;
const ROW_VOLUME: usize = 1;
const ROW_RESET: usize = 2;

macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write($val);
        x
    }};
}

#[derive(Serialize, Clone)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a Vec<ChatMessage>,
    stream: bool,
}

#[derive(Deserialize)]
struct SttResponse {
    text: String,
}

#[derive(Serialize)]
struct TtsRequest<'a> {
    model: &'a str,
    input: &'a str,
    voice: &'a str,
    response_format: &'a str,
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();
    println!("=== boot ===");

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 8 * 1024);
    // Framebuffer + recorded audio + JSON/HTTP buffers all live in PSRAM -
    // internal SRAM is reserved for latency-sensitive DMA/network structures.
    esp_alloc::psram_allocator!(peripherals.PSRAM, esp_hal::psram);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    // ---------- Settings ----------
    // Load before anything that needs credentials. This is the *only* source
    // of configuration - nothing is compiled into the image - so a missing
    // partition, a missing record or a record that fails to validate all mean
    // the same thing: the device is unconfigured and belongs in the setup
    // portal (see the provisioning check after the radio comes up).
    let mut flash = esp_storage::FlashStorage::new(peripherals.FLASH);
    let store = settings::Store::locate(&mut flash);
    if store.is_none() {
        println!("no nvs partition found - settings will not persist");
    }
    let stored = store.and_then(|s| s.load(&mut flash));
    // Surfaced on the config card: "did my save actually take" is the first
    // thing you want to know after using the setup portal.
    let mut settings_from_flash = stored.is_some();
    let mut settings = stored
        .inspect(|_| info!("Loaded saved settings from flash"))
        .unwrap_or_else(|| {
            info!("No saved settings - device is unconfigured");
            Settings::unconfigured()
        });

    // ---------- Display ----------
    info!("Initializing display");
    let mut backlight = Output::new(peripherals.GPIO42, Level::High, OutputConfig::default());
    let dc = Output::new(peripherals.GPIO47, Level::Low, OutputConfig::default());
    let rst = Output::new(peripherals.GPIO38, Level::High, OutputConfig::default());
    let cs = Output::new(peripherals.GPIO5, Level::High, OutputConfig::default());

    let display_spi = Spi::new(
        peripherals.SPI2,
        SpiConfig::default()
            .with_frequency(Rate::from_mhz(40))
            .with_mode(Mode::_0),
    )
    .unwrap()
    .with_sck(peripherals.GPIO4)
    .with_mosi(peripherals.GPIO2)
    .into_async();

    let spi_device = ExclusiveDevice::new_no_delay(display_spi, cs).unwrap();
    let di = SpiInterface::new(spi_device, dc);

    let mut delay = Delay;
    let mut display = Builder::new(GC9A01, di)
        .reset_pin(rst)
        .display_size(pins::DISPLAY_WIDTH, pins::DISPLAY_HEIGHT)
        .orientation(Orientation::new())
        .invert_colors(ColorInversion::Inverted)
        .color_order(ColorOrder::Bgr)
        .init(&mut delay)
        .await
        .expect("display init failed");
    backlight.set_low(); // active-low backlight enable

    let mut ui_frame = vec![0u8; pins::DISPLAY_WIDTH as usize * pins::DISPLAY_HEIGHT as usize * 2];
    // From here until the main loop starts, every milestone repaints the boot
    // card. The device is unresponsive to touch for several seconds while the
    // codec comes up and wifi joins, and showing the idle "tap to talk" screen
    // through all of that just invites taps that go nowhere.
    boot_screen(&mut display, &mut ui_frame, "display", 10).await;
    info!("Display initialized");

    // ---------- Touch ----------
    info!("Initializing touch controller");
    let touch_i2c = I2c::new(
        peripherals.I2C1,
        I2cConfig::default().with_frequency(Rate::from_khz(400)),
    )
    .unwrap()
    .with_sda(peripherals.GPIO11)
    .with_scl(peripherals.GPIO7)
    .into_async();

    let mut touch_rst = Output::new(peripherals.GPIO6, Level::High, OutputConfig::default());
    touch_rst.set_low();
    Timer::after(Duration::from_millis(5)).await;
    touch_rst.set_high();
    Timer::after(Duration::from_millis(50)).await;

    // The CST816 pulls this low when it has new touch data, so the task can
    // block on it instead of polling I2C 50x/sec while nothing is happening.
    let touch_int = Input::new(
        peripherals.GPIO12,
        InputConfig::default().with_pull(Pull::Up),
    );

    match touch::Cst816d::new(touch_i2c, touch::CST816D_ADDR).await {
        Ok(touch_dev) => spawner.spawn(touch_task(touch_dev, touch_int).expect("spawn touch_task")),
        Err(e) => println!("CST816D init failed: {e:?}"),
    }

    // ---------- BOOT button (GPIO0) ----------
    // The board's only other button. Active-low against its pull-up, and used
    // here for one thing: drop the conversation history and start over.
    let boot_button = Input::new(
        peripherals.GPIO0,
        InputConfig::default().with_pull(Pull::Up),
    );
    spawner.spawn(button_task(boot_button).expect("spawn button_task"));

    // ---------- Status LED (WS2812 on GPIO48) ----------
    // Matches the factory firmware's color scheme: blue while connecting,
    // red while listening, green while speaking, off when idle.
    let rmt = Rmt::new(peripherals.RMT, Rate::from_mhz(80)).expect("RMT init failed");
    let mut status_led = Ws2812::<{ buffer_len(1) }>::new(
        rmt.channel0,
        peripherals.GPIO48,
        Timing::WS2812B_AT_12_5NS_TICK_ESP32C3,
        1,
    )
    .expect("ws2812 init failed");
    // The LED only exists from here on, so the first two boot frames run
    // dark; everything after this point rides on `show`.
    set_led(&mut status_led, &UiState::Boot { step: "", progress: 0 });
    boot_screen(&mut display, &mut ui_frame, "touch", 25).await;

    // ---------- Audio codec (ES8311) ----------
    info!("Initializing ES8311 audio codec");
    let mut codec_i2c = I2c::new(
        peripherals.I2C0,
        I2cConfig::default().with_frequency(Rate::from_khz(400)),
    )
    .unwrap()
    .with_sda(peripherals.GPIO15)
    .with_scl(peripherals.GPIO14);

    let mut pa_enable = Output::new(peripherals.GPIO46, Level::Low, OutputConfig::default());

    let codec = Es8311::new(pins::AUDIO_CODEC_ES8311_ADDR);
    let clk_cfg = Es8311ClockConfig {
        mclk_inverted: false,
        sclk_inverted: false,
        mclk_from_mclk_pin: true,
        mclk_frequency: AUDIO_SAMPLE_RATE * 256,
        sample_frequency: AUDIO_SAMPLE_RATE,
    };
    codec
        .init(&mut codec_i2c, &clk_cfg, Resolution::Bits16, Resolution::Bits16, &mut delay)
        .expect("ES8311 init failed");
    codec
        .microphone_config(&mut codec_i2c, false) // analog mic
        .expect("mic config failed");
    {
        // The `es8311` crate's init()/microphone_config() don't write several
        // registers the official esp-idf driver (esp_codec_dev's es8311.c,
        // es8311_init() + es8311_start()) always does, and it gets two of them
        // outright wrong. Poke the vendor values in by hand.
        let mut w = |reg: u8, val: u8| {
            codec_i2c
                .write(pins::AUDIO_CODEC_ES8311_ADDR, &[reg, val])
                .expect("es8311 raw register write failed");
        };
        w(0x0B, 0x00);
        w(0x0C, 0x00);
        w(0x10, 0x1F);
        w(0x11, 0x7F);
        w(0x15, 0x40); // ADC_REG15: ADC ramp rate
        // ADC_REG16 ("ADC gain scale up"). The crate's `microphone_gain_set`
        // treats this as a 6-bit 0x00..0x3F gain field and writes 0x28 for
        // 24 dB, but the real field is only bits [2:0]; 0x28 makes the ADC
        // output *exact digital zero*. 0x24 is the vendor driver's value and
        // is what actually produces audio. Mic gain lives in REG14's PGA
        // field instead (0x1A = analog mic, PGA 10 = 30 dB), which
        // `microphone_config` above already sets.
        w(0x16, 0x24);
        w(0x17, 0xBF); // ADC_REG17: ADC digital volume (crate's default is near-mute)
        w(0x1B, 0x0A);
        // GPIO_REG44 [6:4] is the SDOUT source mux. 0x00 = plain ADC data on
        // both TDM slots, which is what we want. Do NOT use esp-idf's 0x50 /
        // 0x58 here: those select "ADC mixed with DAC" / "DAC on the right
        // slot" monitoring modes, which bleed playback into the recording and
        // (at 0x58) keep ADC data off SDOUT entirely.
        w(0x44, 0x00);
        w(0x45, 0x00);
    }
    codec
        .volume_set(&mut codec_i2c, settings.codec_volume(), None)
        .expect("volume set failed");
    codec.mute(&mut codec_i2c, false).expect("unmute failed");
    pa_enable.set_low(); // keep the amp muted except while actively speaking
    info!("ES8311 codec initialized");
    boot_screen(&mut display, &mut ui_frame, "audio codec", 45).await;

    // ---------- I2S (full duplex) ----------
    info!("Initializing I2S");
    let dma_channel = peripherals.DMA_CH0;
    // Both directions use one-shot transfers, so neither side keeps a live
    // DMA engine running while the other is idle. `rx_buffer` has to come
    // from `dma_buffers!` rather than being a plain stack array: GDMA RX
    // descriptors must point at word-aligned memory, and that macro is what
    // guarantees the alignment.
    let (rx_buffer, rx_descriptors, tx_buffer, tx_descriptors) =
        esp_hal::dma_buffers!(16384, 8192);

    let i2s = I2s::new(
        peripherals.I2S0,
        dma_channel,
        I2sConfig::new_tdm_philips()
            .with_sample_rate(Rate::from_hz(AUDIO_SAMPLE_RATE))
            .with_data_format(DataFormat::Data16Channel16)
            .with_channels(Channels::MONO)
            // Full duplex on one codec: only GPIO9/GPIO45 carry BCLK/WS, and
            // the GPIO matrix can only route one peripheral output to a pad,
            // so the TX unit owns them and I2S_SIG_LOOPBACK makes the RX unit
            // ride the same clocks (RX becomes the slave, TX stays master).
            // Without this the RX unit free-runs on its own internal clock
            // that never reaches the ES8311, so DIN stays static and every
            // recording comes back as digital silence.
            .with_signal_loopback(true),
    )
    .unwrap()
    .with_mclk(peripherals.GPIO16)
    .into_async();

    let i2s_tx = i2s
        .i2s_tx
        .with_bclk(peripherals.GPIO9)
        .with_ws(peripherals.GPIO45)
        .with_dout(peripherals.GPIO8)
        .build(tx_descriptors);
    let i2s_rx = i2s.i2s_rx.with_din(peripherals.GPIO10).build(rx_descriptors);

    let mut i2s_tx = i2s_tx;
    let mut i2s_rx = i2s_rx;

    {
        // esp-hal sets I2S_TX_STOP_EN, which gates BCLK/WS off as soon as the
        // TX FIFO runs dry. Since RX now rides TX's clocks (see the config
        // above), that would leave the ES8311 unclocked for everything except
        // the moments we're actively playing audio. Clear it and kick the TX
        // unit once, so BCLK/WS free-run for the rest of the program - the
        // codec needs them to shift ADC data out at all. This mirrors what
        // esp-idf does for a full-duplex channel.
        let i2s0 = esp_hal::peripherals::I2S0::regs();
        i2s0.tx_conf().modify(|_, w| w.tx_stop_en().clear_bit());
        i2s0.tx_conf().modify(|_, w| w.tx_update().set_bit());
        // Kick it through esp-hal's own start path once (a plain tx_start bit
        // poke isn't enough to get the unit going), then re-clear tx_stop_en
        // since `write_dma_async` sets it again on the way out.
        tx_buffer[..512].fill(0);
        if let Err(e) = i2s_tx.write_dma_async(&mut tx_buffer[..512]).await {
            println!("i2s tx kick error: {e:?}");
        }
        i2s0.tx_conf().modify(|_, w| {
            w.tx_stop_en().clear_bit();
            w.tx_start().set_bit()
        });
    }

    // Neither direction uses a circular transfer.
    //
    // TX: a persistent circular write running continuously alongside RX
    // corrupted RX's DMA state entirely (confirmed by direct test). The
    // one-shot `write_dma_async` (reusable, `&mut self`) avoids that - TX
    // sits fully idle except while actually speaking.
    //
    // RX: `read_dma_circular_async` is unusable here, and the reason is in
    // esp-hal's `RxCircularState`. `update()` walks the descriptors the DMA
    // has handed back and returns `DmaError::Late` the moment it finds the
    // *whole* ring is CPU-owned - i.e. we didn't drain in time. Descriptors
    // are only ever handed back to the DMA inside `pop()`, which `update()`
    // returns before reaching, so a single missed deadline wedges the
    // transfer permanently (matches the observed 3.7M consecutive errors
    // with zero recoveries). `pop()` also rejects any read buffer smaller
    // than the accumulated `available`, which is a second way to wedge it.
    // Since the transfer also consumes the `I2sRx` with no way to get it
    // back, there is no reset path - the only safe use is "never stop
    // polling for the entire program lifetime", which we cannot promise
    // across WiFi/TLS work. One-shot reads have no such state: each call
    // resets the RX unit and rebuilds the descriptor chain from scratch.

    // esp-hal's `rx_start()` programs I2S_RX_EOF_NUM straight from the byte
    // length, but that register is only 12 bits wide and counts in units of
    // (RX_BITS_MOD + 1) bits, so most lengths ask the hardware to signal EOF
    // at the wrong point - the transfer then either ends early or runs off
    // the end of the descriptor chain, surfacing as DmaError(DescriptorError)
    // (DmaRxFuture reports DescriptorEmpty/ErrorEof under that same name).
    // RX_CHUNK_LEN (4092) is one full GDMA descriptor (esp-hal's CHUNK_SIZE)
    // and is the largest length that reliably completes and fills the whole
    // buffer - 6144/8184/16384 either error out or return Ok after a partial
    // transfer.
    info!("Audio pipeline ready");
    boot_screen(&mut display, &mut ui_frame, "audio pipeline", 60).await;


    // Radio still off, nothing else contending for a bus: the cleanest the
    // speaker will ever be. Repeated after the join for the comparison.
    //
    // The three block sizes play the identical waveform and differ only in how
    // often a DMA write ends - 6, 47 and 187 boundaries a second. If the fault
    // gets denser down the sweep it lives at the boundary; if all three sound
    // alike it is the analog stage and no amount of buffering will touch it.
    for (block, secs) in TEST_TONE_SWEEP {
        test_tone(
            &mut i2s_tx,
            tx_buffer,
            &mut pa_enable,
            "pre-wifi",
            *block,
            *secs,
        )
        .await;
        Timer::after(Duration::from_millis(400)).await;
    }

    // ---------- WiFi ----------
    info!("Bringing up wifi");
    let station_config = Config::Station(
        StationConfig::default()
            .with_ssid(settings.wifi_ssid())
            .with_password(settings.wifi_password().into()),
    );
    let (controller, interfaces) = esp_radio::wifi::new(
        peripherals.WIFI,
        ControllerConfig::default().with_initial_config(station_config),
    )
    .expect("failed to init wifi controller");

    let wifi_interface = interfaces.station;
    let ap_interface = interfaces.access_point;
    let net_config = embassy_net::Config::dhcpv4(Default::default());
    let rng = Rng::new();
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    let (stack, runner) = embassy_net::new(
        wifi_interface,
        net_config,
        mk_static!(StackResources<4>, StackResources::<4>::new()),
        seed,
    );

    // The AP stack is built now, at boot, even though the radio won't be in AP
    // mode until the user asks for the portal. Both interfaces come out of the
    // same `esp_radio::wifi::new()` call, so the alternative would be keeping
    // `interfaces` alive across the whole program just to consume it later -
    // and an idle stack on a down link costs nothing but its own resources.
    let (ap_stack, ap_runner) = embassy_net::new(
        ap_interface,
        embassy_net::Config::ipv4_static(embassy_net::StaticConfigV4 {
            address: embassy_net::Ipv4Cidr::new(portal::PORTAL_IP, 24),
            // No default route: nothing on this network is off-subnet, and
            // smoltcp short-circuits 255.255.255.255 (which the DHCP replies
            // need) before it ever consults the routing table.
            gateway: None,
            dns_servers: Default::default(),
        }),
        mk_static!(StackResources<4>, StackResources::<4>::new()),
        seed ^ 0x5a5a_5a5a,
    );

    spawner.spawn(connection_task(controller).expect("spawn connection_task"));
    spawner.spawn(net_task(runner).expect("spawn net_task"));
    spawner.spawn(ap_net_task(ap_runner).expect("spawn ap_net_task"));

    // Nothing is compiled in, so a device that has never been set up has no
    // network to join and no endpoint to call. Skip straight to the portal
    // rather than spending 25 seconds failing to associate with an empty SSID.
    let setup_reason = if let Some(bad) = settings.first_invalid() {
        info!("Not usable yet (check \"{}\") - starting setup", bad.label());
        Some("nothing is configured yet - starting the setup hotspot")
    } else {
        // Stepped rather than one long `with_timeout` so the boot ring keeps
        // filling: this is by far the longest part of startup, and a frozen
        // progress ring for 25 seconds reads as a crash.
        let mut joined = false;
        for tick in 0..WIFI_JOIN_TICKS {
            if embassy_time::with_timeout(WIFI_JOIN_TICK, stack.wait_config_up())
                .await
                .is_ok()
            {
                joined = true;
                break;
            }
            let progress = 60 + (tick as u32 + 1) * 38 / WIFI_JOIN_TICKS as u32;
            boot_screen(&mut display, &mut ui_frame, "joining wi-fi", progress as u8).await;
        }
        if joined {
            if let Some(cfg) = stack.config_v4() {
                println!("Got IP: {}", cfg.address);
            }
            None
        } else {
            // The saved credentials are the only ones there are, so a join
            // failure is a configuration problem by definition - hand the user
            // the form instead of an assistant that can't reach anything.
            println!("wifi did not come up in time - starting setup");
            Some("could not join the saved network - starting the setup hotspot")
        }
    };

    if let Some(reason) = setup_reason {
        // One exception: with no nvs partition the portal has nowhere to write,
        // so entering it would reboot into the same state forever. Better a
        // visibly broken assistant than an unbreakable boot loop.
        if store.is_some() {
            enter_portal(
                &mut display,
                &mut ui_frame,
                &mut status_led,
                ap_stack,
                store,
                &mut flash,
                &settings,
                reason,
            )
            .await;
        }
        println!("no storage partition - cannot run setup, continuing unconfigured");
    }

    let tcp_client_state = mk_static!(
        TcpClientState<1, 4096, 4096>,
        TcpClientState::<1, 4096, 4096>::new()
    );
    let tcp_client = TcpClient::new(stack, tcp_client_state);
    let dns_client = DnsSocket::new(stack);
    let read_buf = mk_static!([u8; 16384], [0u8; 16384]);
    let write_buf = mk_static!([u8; 16384], [0u8; 16384]);
    let tls_config = TlsConfig::new(seed, read_buf, write_buf, TlsVerify::None);
    let mut client = HttpClient::new_with_tls(&tcp_client, &dns_client, tls_config);

    // A second, fully independent client for TTS: TTS calls happen *while*
    // the agent's SSE response reader above is still open (per-sentence
    // playback during streaming), so they can't share a connection/buffers
    // with the main client.
    let tcp_client_state2 = mk_static!(
        TcpClientState<1, 4096, 4096>,
        TcpClientState::<1, 4096, 4096>::new()
    );
    let tcp_client2 = TcpClient::new(stack, tcp_client_state2);
    let dns_client2 = DnsSocket::new(stack);
    let read_buf2 = mk_static!([u8; 16384], [0u8; 16384]);
    let write_buf2 = mk_static!([u8; 16384], [0u8; 16384]);
    let tls_config2 = TlsConfig::new(seed, read_buf2, write_buf2, TlsVerify::None);
    let mut tts_client = HttpClient::new_with_tls(&tcp_client2, &dns_client2, tls_config2);

    // Every URL is the user's base plus one standard OpenAI-compatible
    // operation path, so a server rooted at `/v1` and one rooted at `/api/v1`
    // both work without the firmware knowing which is which.
    let stt_url = settings.endpoint(Field::SpeechBaseUrl, settings::OP_TRANSCRIPTIONS);
    let tts_url = settings.endpoint(Field::SpeechBaseUrl, settings::OP_SPEECH);
    let speech_auth = settings.bearer(Field::SpeechApiKey);
    let stt_auth = speech_auth.clone();
    let tts_auth = speech_auth;
    let agent_url = settings.endpoint(Field::AgentBaseUrl, settings::OP_CHAT_COMPLETIONS);
    let agent_auth = settings.bearer(Field::AgentApiKey);
    let stt_model = settings.get(Field::SpeechSttModel).to_string();
    let tts_model = settings.get(Field::SpeechTtsModel).to_string();
    let agent_model = settings.get(Field::AgentModel).to_string();

    // The full URLs actually in effect, which is the fastest way to catch a
    // base URL saved with the wrong API prefix. No keys here.
    info!(
        "Endpoints: agent {} ({}), stt {} ({}), tts {} ({}) [{}]",
        agent_url,
        agent_model,
        stt_url,
        stt_model,
        tts_url,
        tts_model,
        if settings_from_flash { "from flash" } else { "unconfigured" },
    );

    boot_screen(&mut display, &mut ui_frame, "ready", 100).await;

    // Same tone, same buffer, same code path - the only thing that changed is
    // that the radio is now associated and the stacks are running. If this one
    // is rough and the pre-wifi one wasn't, the crackle is interference, not
    // the codec.
    if let Some((block, secs)) = TEST_TONE_SWEEP.first() {
        test_tone(
            &mut i2s_tx,
            tx_buffer,
            &mut pa_enable,
            "post-wifi",
            *block,
            *secs,
        )
        .await;
    }
    info!("Ready");

    // ---------- Main state machine ----------
    let mut history: Vec<ChatMessage> = vec![system_message()];
    let mut idle_elapsed_secs: u64 = 0;
    let mut anim: u32 = 0;
    let mut screen = Screen::Assistant;
    let mut settings_dirty = false;
    let mut settings_offset = 0usize;

    // Taps that arrived during boot/wifi-connect (people naturally tap a
    // screen to see if it's alive) would otherwise sit queued and get
    // consumed as soon as the loop starts, corrupting the first few turns.
    drain_taps();

    loop {
        // ----- Settings card (read-only) -----
        if screen == Screen::Settings {
            let entries = setting_entries(&settings, settings_from_flash);
            // Scrolls a whole page at a time, so the offset is always on a page
            // boundary and the last page is never a part-scrolled sliver.
            let max_offset =
                entries.len().saturating_sub(1) / ui::SETTINGS_PAGE_ROWS * ui::SETTINGS_PAGE_ROWS;
            settings_offset = settings_offset.min(max_offset);
            show(
                &mut display,
                &mut ui_frame,
                &mut status_led,
                &UiState::Settings { entries, offset: settings_offset },
                0,
            )
            .await;
            match TOUCH_EVENTS.receive().await {
                TouchEvent::SwipeRight => screen = Screen::Configure,
                TouchEvent::SwipeUp => {
                    settings_offset = (settings_offset + ui::SETTINGS_PAGE_ROWS).min(max_offset);
                }
                TouchEvent::SwipeDown => {
                    settings_offset = settings_offset.saturating_sub(ui::SETTINGS_PAGE_ROWS);
                }
                _ => {}
            }
            continue;
        }

        // ----- Configure card (actions) -----
        if screen == Screen::Configure {
            show(&mut display, &mut ui_frame, &mut status_led, &configure_menu(&settings), 0).await;
            match TOUCH_EVENTS.receive().await {
                TouchEvent::SwipeLeft => {
                    // Flush before showing the read-only list, so it can never
                    // display a volume that isn't the one in flash.
                    if settings_dirty && save_settings(&store, &mut flash, &settings) {
                        settings_dirty = false;
                        settings_from_flash = true;
                    }
                    settings_offset = 0;
                    screen = Screen::Settings;
                }
                TouchEvent::SwipeRight => {
                    // Persist a volume change once, on the way out, rather
                    // than erasing a flash sector on every tap of "+".
                    if settings_dirty && save_settings(&store, &mut flash, &settings) {
                        settings_dirty = false;
                        settings_from_flash = true;
                    }
                    screen = Screen::Assistant;
                }
                TouchEvent::Tap { x, y } => {
                    match ui::menu_row_at(x as i32, y as i32, CONFIGURE_ROWS) {
                        Some(ROW_WIFI) => {
                            enter_portal(
                                &mut display,
                                &mut ui_frame,
                                &mut status_led,
                                ap_stack,
                                store,
                                &mut flash,
                                &settings,
                                "leaving your network and starting the setup hotspot",
                            )
                            .await
                        }
                        Some(ROW_VOLUME) => {
                            let delta = match ui::row_zone(x as i32) {
                                ui::RowZone::Decrement => -(VOLUME_STEP as i32),
                                ui::RowZone::Increment => VOLUME_STEP as i32,
                                ui::RowZone::Body => 0,
                            };
                            let next = (settings.volume as i32 + delta).clamp(0, 100) as u8;
                            if next != settings.volume {
                                settings.volume = next;
                                settings_dirty = true;
                                // The row shows the 0..=100 percentage; the
                                // codec gets that scaled into its usable range.
                                let codec_level = settings.codec_volume();
                                if let Err(e) =
                                    codec.volume_set(&mut codec_i2c, codec_level, None)
                                {
                                    println!("volume set failed: {e:?}");
                                }
                            }
                        }
                        Some(ROW_RESET) => {
                            show(
                                &mut display,
                                &mut ui_frame,
                                &mut status_led,
                                &UiState::Notice {
                                    title: "Erase?".into(),
                                    body: "tap again to erase saved settings and restart into setup".into(),
                                    color: NoticeColor::Warn,
                                },
                                0,
                            )
                            .await;
                            drain_taps();
                            let confirmed = matches!(
                                embassy_time::with_timeout(
                                    Duration::from_secs(6),
                                    TOUCH_EVENTS.receive()
                                )
                                .await,
                                Ok(TouchEvent::Tap { .. })
                            );
                            if confirmed {
                                if let Some(store) = store {
                                    match store.clear(&mut flash) {
                                        Ok(()) => info!("Settings erased"),
                                        Err(e) => println!("settings erase failed: {e}"),
                                    }
                                }
                                show(
                                    &mut display,
                                    &mut ui_frame,
                                    &mut status_led,
                                    &UiState::Notice {
                                        title: "Erased".into(),
                                        body: "restarting".into(),
                                        color: NoticeColor::Good,
                                    },
                                    0,
                                )
                                .await;
                                Timer::after(Duration::from_secs(2)).await;
                                esp_hal::system::software_reset();
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            continue;
        }

        // ----- Idle -----
        let wifi_ok = stack.is_link_up();
        show(
            &mut display,
            &mut ui_frame,
            &mut status_led,
            &UiState::Idle { wifi_ok, exchanges: exchange_count(&history) },
            anim,
        )
        .await;
        // Waiting on the button here as well as touch is what makes a press
        // during a reply still land: `Signal` holds the raise until someone
        // waits on it, so the clear happens as soon as the turn is over.
        let idle_event = embassy_time::with_timeout(
            Duration::from_secs(20),
            embassy_futures::select::select(TOUCH_EVENTS.receive(), NEW_CONVERSATION.wait()),
        )
        .await;
        match idle_event {
            Ok(embassy_futures::select::Either::First(TouchEvent::Tap { .. })) => {}
            Ok(embassy_futures::select::Either::First(TouchEvent::SwipeLeft)) => {
                screen = Screen::Configure;
                continue;
            }
            // Any other swipe on the assistant card means nothing; don't let
            // it fall through and start a recording the user didn't ask for.
            Ok(embassy_futures::select::Either::First(_)) => continue,
            Ok(embassy_futures::select::Either::Second(())) => {
                let had = exchange_count(&history);
                history.truncate(1);
                idle_elapsed_secs = 0;
                info!("Cleared conversation history: button ({had} exchanges)");
                show(
                    &mut display,
                    &mut ui_frame,
                    &mut status_led,
                    &UiState::Notice {
                        title: "New chat".into(),
                        body: if had == 0 {
                            "already empty".into()
                        } else {
                            format!("{had} exchanges cleared")
                        },
                        color: NoticeColor::Info,
                    },
                    0,
                )
                .await;
                Timer::after(Duration::from_millis(1200)).await;
                continue;
            }
            Err(_) => {
                idle_elapsed_secs += 20;
                // Back to just the standing instructions, not empty: a cleared
                // history has to keep the system message or the device turns
                // into a chatbot again after the first idle timeout.
                if idle_elapsed_secs >= IDLE_HISTORY_TIMEOUT_SECS && history.len() > 1 {
                    history.truncate(1);
                    info!("Cleared conversation history after idle timeout");
                }
                continue;
            }
        }
        idle_elapsed_secs = 0;
        // Eat any bounce/duplicate of the tap that just started this turn
        // so it isn't immediately misread as the "stop listening" tap.
        drain_taps();

        // ----- Listening -----
        // Recorded inline, one-shot chunk at a time, directly in this task -
        // a spawned task doing this same DMA read corrupted its state
        // entirely (see the audio pipeline setup comment above).
        //
        // Ends on whichever comes first: the user tapping, the user swiping to
        // throw the turn away, ~0.9s of silence after they've clearly spoken,
        // or the hard cap. The silence detector calibrates against the first
        // few chunks of the actual room rather than a fixed threshold, so it
        // works in a quiet room and a noisy one.
        anim = 0;
        println!("entering listening loop");
        let mut pcm: Vec<u8> =
            Vec::with_capacity(MAX_RECORDING_SECS as usize * AUDIO_SAMPLE_RATE as usize * 2);
        let deadline = embassy_time::Instant::now() + Duration::from_secs(MAX_RECORDING_SECS);
        let mut stop_reason = "max duration";
        let mut noise_floor: u32 = 0;
        let mut chunk_index: usize = 0;
        let mut speech_started = false;
        let mut quiet_run: usize = 0;
        let mut level_ui: u8 = 0;
        let mut cancelled = false;
        // Loudest chunk seen after calibration. Only used for the exit log, but
        // it's the one number that separates "the mic is dead" from "the gate
        // was set too high", which the noise floor alone can't tell you.
        let mut peak_level: u32 = 0;

        while embassy_time::Instant::now() < deadline {
            // Pushing the framebuffer takes ~20ms of SPI DMA; run it
            // concurrently with the capture so it doesn't punch a hole in
            // the recording between one-shot reads.
            let (read_res, ()) = embassy_futures::join::join(
                i2s_rx.read_dma_async(&mut rx_buffer[..RX_CHUNK_LEN]),
                show(
                    &mut display,
                    &mut ui_frame,
                    &mut status_led,
                    &UiState::Listening { level: level_ui },
                    anim,
                ),
            )
            .await;
            anim = anim.wrapping_add(1);
            match read_res {
                // The first sample of every one-shot read is a stale word left
                // in the RX FIFO by the previous transfer (measured: a lone
                // out-of-range spike ahead of otherwise clean audio). Drop it.
                Ok(()) => {
                    let samples = &rx_buffer[2..RX_CHUNK_LEN];
                    let level = mean_abs_level(samples);
                    pcm.extend_from_slice(samples);

                    if chunk_index < VAD_CALIBRATION_CHUNKS {
                        // Learn the room. Take the loudest calibration chunk so
                        // one quiet moment can't set an unusably low floor.
                        noise_floor = noise_floor.max(level);
                    } else {
                        let speech_gate = (noise_floor * 3).max(VAD_MIN_SPEECH_LEVEL);
                        let silence_gate = (noise_floor * 2).max(VAD_MIN_SPEECH_LEVEL / 2);
                        level_ui = (level * 10 / speech_gate.max(1)).min(10) as u8;
                        peak_level = peak_level.max(level);
                        if level >= speech_gate {
                            speech_started = true;
                            quiet_run = 0;
                        } else if level <= silence_gate {
                            quiet_run += 1;
                        } else {
                            // In the band between the two gates: ambiguous, so
                            // hold rather than counting toward end-of-speech.
                            quiet_run = 0;
                        }
                        if speech_started && quiet_run >= VAD_HANG_CHUNKS {
                            stop_reason = "silence";
                            break;
                        }
                        if !speech_started && chunk_index >= VAD_NO_SPEECH_CHUNKS {
                            stop_reason = "nothing heard";
                            break;
                        }
                    }
                    chunk_index += 1;
                }
                Err(e) => println!("mic read error: {e:?}"),
            }
            match TOUCH_EVENTS.try_receive() {
                Ok(TouchEvent::Tap { .. }) => {
                    stop_reason = "tap";
                    break;
                }
                // Any swipe abandons the turn. It's the natural "no, forget
                // it" gesture and it can't be mistaken for the tap that sends,
                // which matters because there is no undo once the audio has
                // gone to the transcriber.
                Ok(_) => {
                    stop_reason = "cancelled";
                    cancelled = true;
                    break;
                }
                Err(_) => {}
            }
        }
        println!(
            "listening loop exited: {stop_reason} ({} bytes, noise floor {noise_floor}, gate {}, peak {peak_level}, speech {speech_started})",
            pcm.len(),
            (noise_floor * 3).max(VAD_MIN_SPEECH_LEVEL),
        );
        drain_taps(); // eat any bounce of the "stop" tap before the next turn
        if cancelled {
            // Drop the audio on the floor: nothing is transcribed, nothing is
            // sent, and history is left exactly as it was.
            drop(pcm);
            cancel_notice(&mut display, &mut ui_frame, &mut status_led).await;
            continue;
        }
        if pcm.len() < AUDIO_SAMPLE_RATE as usize || !speech_started {
            // Less than ~0.5s of audio, or nothing that rose above the room's
            // own noise - either way there's nothing worth transcribing.
            show(
                &mut display,
                &mut ui_frame,
                &mut status_led,
                &UiState::Error("didn't hear anything".into()),
                0,
            )
            .await;
            Timer::after(Duration::from_secs(2)).await;
            continue;
        }

        // ----- Thinking: STT -----
        show(&mut display, &mut ui_frame, &mut status_led, &UiState::Thinking { tool: None }, 0).await;
        let wav_data = wav::wav_bytes(&pcm, AUDIO_SAMPLE_RATE);
        let boundary = "----esp32s3-voice-assistant-boundary";
        let mut body = Vec::with_capacity(wav_data.len() + 256);
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(b"Content-Disposition: form-data; name=\"model\"\r\n\r\n");
        body.extend_from_slice(stt_model.as_bytes());
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\nContent-Type: audio/wav\r\n\r\n",
        );
        body.extend_from_slice(&wav_data);
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

        let content_type = format!("multipart/form-data; boundary={boundary}");
        let mut rx_buf = vec![0u8; 8192];
        let audio_secs = pcm.len() as u64 * 1000 / (AUDIO_SAMPLE_RATE as u64 * 2);
        let stt = async {
            // Split the same three ways as the TTS timings, so the two are
            // directly comparable: `connect` is TCP + the TLS handshake,
            // `send` is the multipart upload plus the server's think time,
            // and `read` is pulling the (tiny) JSON back.
            let t0 = embassy_time::Instant::now();
            match client.request(Method::POST, &stt_url).await {
                Ok(req) => {
                    let t_connect = t0.elapsed().as_millis();
                    let body_slice: &[u8] = &body;
                    let headers = [
                        ("Authorization", stt_auth.as_str()),
                        ("Content-Type", content_type.as_str()),
                        ("Connection", "close"),
                    ];
                    let mut req = req.body(body_slice).headers(&headers);
                    match req.send(&mut rx_buf).await {
                        Ok(response) => {
                            let t_send = t0.elapsed().as_millis();
                            match response.body().read_to_end().await {
                                Ok(data) => match serde_json::from_slice::<SttResponse>(data) {
                                    Ok(parsed) => {
                                        println!(
                                            "STT timings: connect {t_connect}ms, send+infer {}ms, read {}ms, total {}ms ({} bytes wav, {}ms audio)",
                                            t_send - t_connect,
                                            t0.elapsed().as_millis() - t_send,
                                            t0.elapsed().as_millis(),
                                            body.len(),
                                            audio_secs,
                                        );
                                        Some(parsed.text)
                                    }
                                    Err(e) => {
                                        println!("STT JSON parse error: {e:?}");
                                        None
                                    }
                                },
                                Err(e) => {
                                    println!("STT body read error: {e:?}");
                                    None
                                }
                            }
                        }
                        Err(e) => {
                            println!("STT send error: {e:?}");
                            None
                        }
                    }
                }
                Err(e) => {
                    println!("STT request build error: {e:?}");
                    None
                }
            }
        };
        // Runs alongside the upload: keeps the spinner turning through what can
        // be several seconds of TLS + multipart POST, and watches for the swipe
        // that abandons the turn. Dropping the request future aborts the socket,
        // which is fine - every request here already sends `Connection: close`.
        let spin_and_watch = async {
            let mut tick = 0u32;
            loop {
                if embassy_time::with_timeout(THINKING_FRAME, wait_for_swipe()).await.is_ok() {
                    return;
                }
                tick = tick.wrapping_add(1);
                show(&mut display, &mut ui_frame, &mut status_led, &UiState::Thinking { tool: None }, tick)
                    .await;
            }
        };
        let stt_outcome = embassy_futures::select::select(stt, spin_and_watch).await;
        let transcript: Option<String> = match stt_outcome {
            embassy_futures::select::Either::First(t) => t,
            embassy_futures::select::Either::Second(()) => {
                println!("turn cancelled during transcription");
                cancel_notice(&mut display, &mut ui_frame, &mut status_led).await;
                continue;
            }
        };

        let transcript = match transcript {
            Some(t) if !t.trim().is_empty() => t,
            _ => {
                show(&mut display, &mut ui_frame, &mut status_led, &UiState::Error("didn't catch that".into()), 0).await;
                Timer::after(Duration::from_secs(2)).await;
                continue;
            }
        };
        info!("Transcript: {transcript}");
        history.push(ChatMessage { role: "user".into(), content: transcript });

        // ----- Thinking: agent (streaming) -----
        let chat_req = ChatRequest { model: &agent_model, messages: &history, stream: true };
        let chat_body = serde_json::to_vec(&chat_req).expect("serialize chat request");

        let mut assistant_text = String::new();
        let mut got_response = false;
        let mut cancelled_turn = false;
        anim = 0;
        // Absolute deadline for the next animation frame, held across every
        // future rebuilt below so none of them push it back.
        let mut next_frame = embassy_time::Instant::now();
        let thinking_plain = UiState::Thinking { tool: None };
        // Same three-way split as STT and TTS, plus time-to-first-token, which
        // is the number that actually decides how long the spinner spins.
        let t_agent = embassy_time::Instant::now();
        let mut t_first_token: Option<u64> = None;
        // Set only by a stream that terminated the way the protocol says it
        // should - a `finish_reason` or a `[DONE]`. Anything else is a stream
        // that was cut off, which is what the retry below is for.
        let mut stream_ok = false;

        for attempt in 0..AGENT_ATTEMPTS {
        if attempt > 0 {
            println!("agent retry {attempt} after truncated stream");
            assistant_text.clear();
            t_first_token = None;
        }
        let connect = embassy_futures::select::select3(
            client.request(Method::POST, &agent_url),
            wait_for_swipe(),
            animate(
                &mut display,
                &mut ui_frame,
                &mut status_led,
                &mut anim,
                &mut next_frame,
                &thinking_plain,
            ),
        )
        .await;
        let connect = match connect {
            embassy_futures::select::Either3::First(r) => Some(r),
            embassy_futures::select::Either3::Second(()) => {
                cancelled_turn = true;
                None
            }
            embassy_futures::select::Either3::Third(()) => unreachable!("animate never returns"),
        };
        match connect {
            None => {}
            Some(Err(e)) => println!("agent request build error: {e:?}"),
            Some(Ok(req)) => {
                let t_connect = t_agent.elapsed().as_millis();
                let body_slice: &[u8] = &chat_body;
                let headers = [
                    ("Authorization", agent_auth.as_str()),
                    ("Content-Type", "application/json"),
                    ("Accept", "text/event-stream"),
                    ("Connection", "close"),
                ];
                let mut req = req.body(body_slice).headers(&headers);
                let mut rx_buf2 = vec![0u8; 8192];
                // `send` doesn't return until the response headers arrive, so
                // on a slow model this is a long wait with nothing on screen.
                let sent = embassy_futures::select::select3(
                    req.send(&mut rx_buf2),
                    wait_for_swipe(),
                    animate(
                        &mut display,
                        &mut ui_frame,
                        &mut status_led,
                        &mut anim,
                        &mut next_frame,
                        &thinking_plain,
                    ),
                )
                .await;
                let sent = match sent {
                    embassy_futures::select::Either3::First(r) => Some(r),
                    embassy_futures::select::Either3::Second(()) => {
                        cancelled_turn = true;
                        None
                    }
                    embassy_futures::select::Either3::Third(()) => unreachable!("animate never returns"),
                };
                match sent {
                    None => {}
                    Some(Err(e)) => println!("agent send error: {e:?}"),
                    Some(Ok(response)) => {
                        let t_send = t_agent.elapsed().as_millis();
                        let mut reader = response.body().reader();
                        let mut chunk = [0u8; 512];
                        let mut line = String::new();
                        let mut current_event: Option<String> = None;
                        let mut done = false;
                        // Latest tool-progress label, so the spinner below can
                        // keep redrawing it instead of it vanishing on the next
                        // frame.
                        let mut tool_label: Option<String> = None;
                        let mut finish_reason: Option<String> = None;
                        let mut reasoning_chars = 0usize;
                        let mut tool_call_deltas = 0usize;
                        let mut traced = 0usize;
                        let mut data_lines = 0usize;

                        while !done {
                            // Three-way race:
                            //  - the read itself, bounded so a stream that
                            //    never terminates can't wedge the turn;
                            //  - a swipe, which abandons the turn;
                            //  - a spinner that never completes, so it is
                            //    always the loser and only ever gets dropped.
                            // Without that third arm nothing renders between
                            // the request going out and the first sentence
                            // coming back, and the animation visibly freezes
                            // for the whole of the model's thinking time.
                            //
                            // The read future is only dropped on the way out of
                            // the loop (stall or swipe both `break`), so there
                            // is never a half-consumed TLS record to resume
                            // from.
                            let thinking = UiState::Thinking { tool: tool_label.clone() };
                            // Patient until the first byte, impatient after it.
                            let idle = if data_lines == 0 { STREAM_STALL_SECS } else { STREAM_IDLE_SECS };
                            let read = embassy_futures::select::select3(
                                embassy_time::with_timeout(
                                    Duration::from_secs(idle),
                                    reader.read(&mut chunk),
                                ),
                                wait_for_swipe(),
                                animate(
                                    &mut display,
                                    &mut ui_frame,
                                    &mut status_led,
                                    &mut anim,
                                    &mut next_frame,
                                    &thinking,
                                ),
                            )
                            .await;
                            let n = match read {
                                embassy_futures::select::Either3::First(Ok(Ok(0))) => break,
                                embassy_futures::select::Either3::First(Ok(Ok(n))) => n,
                                embassy_futures::select::Either3::First(Ok(Err(e))) => {
                                    println!("agent stream read error: {e:?}");
                                    break;
                                }
                                embassy_futures::select::Either3::First(Err(_)) => {
                                    println!("agent stream stalled - no data for {idle}s");
                                    break;
                                }
                                embassy_futures::select::Either3::Second(()) => {
                                    cancelled_turn = true;
                                    break;
                                }
                                embassy_futures::select::Either3::Third(()) => continue,
                            };
                            // The fast path: `select` returns the moment the
                            // read is ready and never polls the arms after it,
                            // so while tokens are flowing neither the swipe nor
                            // the frame timer above ever gets a look in. Both
                            // have to be driven from the loop body instead.
                            match TOUCH_EVENTS.try_receive() {
                                Ok(TouchEvent::Tap { .. }) => {}
                                Ok(_) => {
                                    cancelled_turn = true;
                                    break;
                                }
                                Err(_) => {}
                            }
                            frame_if_due(
                                &mut display,
                                &mut ui_frame,
                                &mut status_led,
                                &mut anim,
                                &mut next_frame,
                                &thinking,
                                SPINNER_FRAME_MS,
                            )
                            .await;
                            for &b in &chunk[..n] {
                                if b != b'\n' {
                                    if b != b'\r' {
                                        line.push(b as char);
                                    }
                                    continue;
                                }
                                if line.is_empty() {
                                    current_event = None;
                                    line.clear();
                                    continue;
                                }
                                if let Some(rest) = line.strip_prefix("event: ") {
                                    current_event = Some(rest.to_string());
                                } else if let Some(rest) = line.strip_prefix("data: ") {
                                    got_response = true;
                                    data_lines += 1;
                                    if rest == "[DONE]" {
                                        done = true;
                                        stream_ok = true;
                                    // A Hermes extension to the OpenAI SSE
                                    // stream, not part of the standard. Plain
                                    // OpenAI-compatible servers never emit it,
                                    // so this branch simply never fires there.
                                    } else if current_event.as_deref() == Some("hermes.tool.progress") {
                                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(rest) {
                                            let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
                                            if status == "running" {
                                                let label = v
                                                    .get("label")
                                                    .and_then(|s| s.as_str())
                                                    .or_else(|| v.get("tool").and_then(|s| s.as_str()))
                                                    .unwrap_or("working");
                                                let emoji = v.get("emoji").and_then(|s| s.as_str()).unwrap_or("");
                                                tool_label = Some(format!("{emoji} {label}"));
                                                show(
                                                    &mut display,
                                                    &mut ui_frame,
                                                    &mut status_led,
                                                    &UiState::Thinking { tool: tool_label.clone() },
                                                    anim,
                                                )
                                                .await;
                                            }
                                        }
                                        current_event = None;
                                    } else {
                                        // Raw trace of what the server actually
                                        // sends. Bounded because this is a
                                        // blocking UART write in the middle of
                                        // a stream - unbounded it would slow
                                        // the read loop enough to change the
                                        // behaviour it is meant to observe.
                                        if traced < AGENT_TRACE_LINES {
                                            traced += 1;
                                            let cut = rest.char_indices().nth(200).map_or(rest.len(), |(i, _)| i);
                                            println!("sse: {}{}", &rest[..cut], if cut < rest.len() { "..." } else { "" });
                                            if traced == AGENT_TRACE_LINES {
                                                println!("sse: (trace truncated)");
                                            }
                                        }
                                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(rest) {
                                            let choice = &v["choices"][0];
                                            if let Some(content) = choice["delta"]["content"].as_str() {
                                                if t_first_token.is_none() && !content.is_empty() {
                                                    t_first_token = Some(t_agent.elapsed().as_millis());
                                                }
                                                assistant_text.push_str(content);
                                            }
                                            // Reasoning models stream their
                                            // scratchpad in a field of its own
                                            // and only then start on `content`.
                                            // It is deliberately not spoken -
                                            // counting it just distinguishes "a
                                            // model that thought and then got
                                            // cut off" from "a model that said
                                            // nothing at all".
                                            if let Some(r) = choice["delta"]["reasoning_content"]
                                                .as_str()
                                                .or_else(|| choice["delta"]["reasoning"].as_str())
                                            {
                                                reasoning_chars += r.len();
                                            }
                                            if !choice["delta"]["tool_calls"].is_null() {
                                                tool_call_deltas += 1;
                                            }
                                            // The turn is over the moment the
                                            // server states why it stopped.
                                            // Waiting for `[DONE]` as well cost
                                            // a flat 30s of stall timeout on
                                            // every reply from a server that
                                            // ends the stream without one, and
                                            // the tail of a truncated answer
                                            // got spoken as if it were whole.
                                            if let Some(reason) = choice["finish_reason"].as_str() {
                                                finish_reason = Some(reason.to_string());
                                                done = true;
                                                stream_ok = true;
                                            }
                                        }
                                        current_event = None;
                                    }
                                }
                                line.clear();
                            }
                        }
                        let t_done = t_agent.elapsed().as_millis();
                        println!(
                            "agent stream ended (finish_reason {}, {} chars, {reasoning_chars} reasoning chars, {tool_call_deltas} tool-call deltas, {data_lines} data lines)",
                            finish_reason.as_deref().unwrap_or("none"),
                            assistant_text.len(),
                        );
                        // `first token` is measured from the very start of the
                        // request, so it includes connect and send: it is the
                        // whole "nothing on screen yet" window, not just the
                        // model's own latency.
                        println!(
                            "AGENT timings: connect {t_connect}ms, send {}ms, first token {}ms, stream {}ms, total {t_done}ms ({} chars, {} msgs in context)",
                            t_send - t_connect,
                            t_first_token.map(|t| t as i64).unwrap_or(-1),
                            t_done - t_first_token.unwrap_or(t_send),
                            assistant_text.len(),
                            history.len(),
                        );
                    }
                }
            }
        }
        // A cut-off stream is only worth resending when it left us with
        // nothing worth saying. A truncated but substantial answer is kept as
        // it stands: re-asking would throw away a reply the user is already
        // watching appear, and cost them the wait a second time.
        if cancelled_turn || stream_ok || assistant_text.trim().len() >= AGENT_MIN_USABLE_CHARS {
            break;
        }
        }

        // One pass here covers everything downstream: the TTS request body, the
        // captions `speak` cuts out of it, and the copy that goes into history.
        let assistant_text = speakable(&assistant_text);

        if cancelled_turn {
            println!("turn cancelled during reply");
            // Whatever was already spoken was genuinely heard, so it belongs in
            // history. If nothing came back at all, drop the user message too
            // rather than leaving a dangling question as context for the next
            // turn - and don't speak the buffered tail of a reply the user just
            // asked to stop.
            if assistant_text.trim().is_empty() {
                history.pop();
            } else {
                history.push(ChatMessage { role: "assistant".into(), content: assistant_text });
            }
            cancel_notice(&mut display, &mut ui_frame, &mut status_led).await;
            continue;
        }

        if !assistant_text.trim().is_empty() {
            let interrupted = speak(
                &mut tts_client,
                &tts_url,
                &tts_auth,
                &tts_model,
                &assistant_text,
                &mut i2s_tx,
                tx_buffer,
                &mut pa_enable,
                &mut status_led,
                &mut display,
                &mut ui_frame,
                anim,
            )
            .await;
            if interrupted {
                println!("reply interrupted by tap");
                drain_taps();
            }
        }

        // Whatever the model managed to say before being cut off still counts
        // as its turn - dropping it would leave the history inconsistent with
        // what the user actually heard.
        if got_response && !assistant_text.trim().is_empty() {
            history.push(ChatMessage { role: "assistant".into(), content: assistant_text });
        } else {
            show(&mut display, &mut ui_frame, &mut status_led, &UiState::Error("no response".into()), 0).await;
            Timer::after(Duration::from_secs(2)).await;
        }
    }
}

/// The single WS2812 on GPIO48.
type StatusLed = Ws2812<'static, { buffer_len(1) }>;

/// Points the status LED at whatever colour `state`'s ring is drawn in.
fn set_led(led: &mut StatusLed, state: &UiState) {
    let (r, g, b) = ui::led_color(state);
    led.write(&[RGB8::new(r, g, b)]).ok();
}

/// Plays a pure tone straight out of `tx_buffer`, with no network, no server
/// and no PSRAM in the path, for `secs` seconds in writes of `block_bytes`.
///
/// The crackle survived every fix in the data path - zero starved blocks, no
/// clipping, and the display pushed down to two frames a reply - so this takes
/// the data out of the question altogether. The table is 64 entries and
/// `block_bytes` is always a multiple of 128, so every block holds a whole
/// number of cycles: the same buffer can be written over and over with the
/// waveform staying perfectly continuous across every boundary. Any audible
/// defect is therefore the codec, the amplifier, the I2S clocking, or the
/// boundary between one one-shot DMA write and the next - nothing else.
///
/// `block_bytes` is the variable that separates those last two. It sets how
/// often a boundary happens without changing a single sample of the waveform,
/// so sweeping it and listening tells us directly whether the fault tracks the
/// boundary rate (our problem, fixable) or ignores it (the analog stage).
///
/// The elapsed-vs-expected figure is the other half: `write_dma_async` returns
/// on *DMA* completion, so if the TX FIFO still held audio at that point,
/// esp-hal's `reset_tx()` - which toggles `tx_fifo_reset` at the top of the
/// next write - would be discarding it, and playback would finish measurably
/// early. Running long instead means the FIFO is empty at each boundary and
/// the gap is dead air, not deleted audio.
async fn test_tone(
    i2s_tx: &mut esp_hal::i2s::master::I2sTx<'static, Async>,
    tx_buffer: &mut [u8],
    pa_enable: &mut Output<'static>,
    label: &str,
    block_bytes: usize,
    secs: u32,
) {
    // One cycle of a sine at 1/64 the sample rate: 375 Hz at 24kHz. Stepping
    // exactly one entry per sample makes this an exactly-sampled sine rather
    // than an approximation of one, so there is no quantisation buzz of its own
    // to confuse with the fault. Amplitude 20000 is close to the 23909 peak a
    // real reply measured, so the analog stage sees a representative level.
    const SINE: [i16; 64] = [
        0, 1960, 3902, 5806, 7654, 9428, 11111, 12688, 14142, 15460, 16629, 17638, 18478, 19139,
        19616, 19904, 20000, 19904, 19616, 19139, 18478, 17638, 16629, 15460, 14142, 12688, 11111,
        9428, 7654, 5806, 3902, 1960, 0, -1960, -3902, -5806, -7654, -9428, -11111, -12688, -14142,
        -15460, -16629, -17638, -18478, -19139, -19616, -19904, -20000, -19904, -19616, -19139,
        -18478, -17638, -16629, -15460, -14142, -12688, -11111, -9428, -7654, -5806, -3902, -1960,
    ];
    // A whole number of cycles per block keeps the repeat phase-continuous.
    let block = (block_bytes / (SINE.len() * 2) * (SINE.len() * 2)).clamp(128, tx_buffer.len());
    for (i, s) in tx_buffer[..block].chunks_exact_mut(2).enumerate() {
        s.copy_from_slice(&SINE[i % SINE.len()].to_le_bytes());
    }
    let bytes_per_sec = AUDIO_SAMPLE_RATE as usize * 2;
    let blocks = (secs as usize * bytes_per_sec / block).max(1);
    println!(
        "test tone: {label} - 375Hz, {block}-byte blocks ({} boundaries/s), {blocks} writes",
        bytes_per_sec / block,
    );
    pa_enable.set_high();
    let mut gap_us_total: u64 = 0;
    let mut gap_us_max: u64 = 0;
    let mut last: Option<embassy_time::Instant> = None;
    let t0 = embassy_time::Instant::now();
    for _ in 0..blocks {
        let now = embassy_time::Instant::now();
        if let Some(prev) = last {
            let gap = (now - prev).as_micros();
            gap_us_total += gap;
            gap_us_max = gap_us_max.max(gap);
        }
        if let Err(e) = i2s_tx.write_dma_async(&mut tx_buffer[..block]).await {
            println!("test tone tx error: {e:?}");
            break;
        }
        last = Some(embassy_time::Instant::now());
    }
    let elapsed = t0.elapsed().as_millis();
    let expected = (blocks * block * 1000 / bytes_per_sec) as u64;
    // Leave the held sample at zero rather than mid-swing, or stopping is
    // itself a DC step into the speaker.
    tx_buffer[..block].fill(0);
    let _ = i2s_tx.write_dma_async(&mut tx_buffer[..block]).await;
    pa_enable.set_low();
    println!(
        "test tone: {label} done, {elapsed}ms vs {expected}ms expected ({}{}ms), gap total {}ms / max {}us, {}us avg",
        if elapsed >= expected { "+" } else { "-" },
        elapsed.abs_diff(expected),
        gap_us_total / 1000,
        gap_us_max,
        gap_us_total / (blocks.max(2) - 1) as u64,
    );
}

/// Sets the status LED to its state colour scaled by a triangular envelope, so
/// the light breathes.
///
/// This is the Speaking card's animation now. The panel's was retired because
/// one frame is a 115,200-byte SPI push of the entire framebuffer no matter how
/// few pixels changed, and that transfer audibly couples into the codec -
/// dropping from a redraw per DMA block (175 a reply) to one per sentence is
/// what quietened the long-standing crackle. The LED is 24 bits over RMT and
/// shares neither the SPI bus, GDMA nor PSRAM with the audio path, so it can
/// move as freely as it likes.
fn pulse_led(led: &mut StatusLed, (r, g, b): (u8, u8, u8), tick: u32) {
    // A full cycle is 2*STEPS ticks; called once per ~170ms DMA block that is
    // about two seconds, which reads as breathing rather than blinking.
    const STEPS: u32 = 6;
    // Out of 255. The light dips but never goes out, because a status LED that
    // reaches zero looks like the device died mid-sentence.
    const FLOOR: u32 = 90;
    let phase = tick % (STEPS * 2);
    let rise = if phase < STEPS { phase } else { STEPS * 2 - phase };
    let scale = FLOOR + (255 - FLOOR) * rise / STEPS;
    let dim = |c: u8| ((c as u32 * scale) / 255) as u8;
    led.write(&[RGB8::new(dim(r), dim(g), dim(b))]).ok();
}

/// Renders a state *and* sets the status LED from it.
///
/// Everything past LED init goes through here rather than calling
/// `ui::render` directly, so there is no way to change what's on screen
/// without the light following along.
async fn show<DI, RST>(
    display: &mut lcd_async::Display<DI, GC9A01, RST>,
    ui_frame: &mut [u8],
    led: &mut StatusLed,
    state: &UiState,
    anim_tick: u32,
) where
    DI: lcd_async::interface::Interface<Word = u8>,
    RST: embedded_hal::digital::OutputPin,
{
    set_led(led, state);
    ui::render(display, ui_frame, state, anim_tick).await;
}

fn drain_taps() {
    while TOUCH_EVENTS.try_receive().is_ok() {}
}

/// Resolves on the first swipe, discarding any taps along the way.
///
/// Raced against the long network waits so a turn can be abandoned mid-flight.
/// Eating the taps is deliberate: left in the channel they would otherwise sit
/// there and fire as barge-in the instant the reply starts playing.
async fn wait_for_swipe() {
    loop {
        if !matches!(TOUCH_EVENTS.receive().await, TouchEvent::Tap { .. }) {
            return;
        }
    }
}

/// The "you cancelled, nothing was sent" beat between an abandoned turn and
/// dropping back to Idle.
async fn cancel_notice<DI, RST>(
    display: &mut lcd_async::Display<DI, GC9A01, RST>,
    ui_frame: &mut [u8],
    led: &mut StatusLed,
) where
    DI: lcd_async::interface::Interface<Word = u8>,
    RST: embedded_hal::digital::OutputPin,
{
    drain_taps();
    show(
        display,
        ui_frame,
        led,
        &UiState::Notice {
            title: "Cancelled".into(),
            body: "nothing sent".into(),
            color: NoticeColor::Info,
        },
        0,
    )
    .await;
    Timer::after(Duration::from_millis(900)).await;
}

/// Switches the radio into setup-AP mode, serves the configuration form,
/// persists whatever comes back, and reboots.
///
/// A one-way door, hence the `!` return: `connection_task` abandons the station
/// reconnect loop to do this, so there is no network left to resume the
/// assistant on and every exit path - saved, save-failed, cancelled, or the AP
/// never coming up - ends in `software_reset()`.
///
/// `reason` is the one-line explanation shown before the switch, which differs
/// by how we got here: the user asking for it, a first boot with nothing
/// configured, or a failure to join the saved network.
#[allow(clippy::too_many_arguments)]
async fn enter_portal<DI, RST, F>(
    display: &mut lcd_async::Display<DI, GC9A01, RST>,
    ui_frame: &mut [u8],
    led: &mut StatusLed,
    ap_stack: embassy_net::Stack<'static>,
    store: Option<settings::Store>,
    flash: &mut F,
    current: &Settings,
    reason: &str,
) -> !
where
    DI: lcd_async::interface::Interface<Word = u8>,
    RST: embedded_hal::digital::OutputPin,
    F: embedded_storage::nor_flash::NorFlash,
{
    show(
        display,
        ui_frame,
        led,
        &UiState::Notice {
            title: "Wi-Fi Setup".into(),
            body: reason.into(),
            color: NoticeColor::Info,
        },
        0,
    )
    .await;

    PORTAL_REQUEST.signal(());
    let ap_ok = matches!(
        embassy_time::with_timeout(Duration::from_secs(10), PORTAL_UP.wait()).await,
        Ok(true)
    );
    if !ap_ok {
        show(
            display,
            ui_frame,
            led,
            &UiState::Notice {
                title: "Hotspot Failed".into(),
                body: "could not start the setup network - restarting".into(),
                color: NoticeColor::Warn,
            },
            0,
        )
        .await;
        Timer::after(Duration::from_secs(3)).await;
        esp_hal::system::software_reset();
    }

    drain_taps();
    // `portal::run` drives the display itself and has no LED, so set the amber
    // its ring uses once here and let it hold for the whole session.
    set_led(
        led,
        &UiState::Portal { ap_ssid: String::new(), url: String::new(), clients: 0 },
    );
    let outcome = portal::run(ap_stack, display, ui_frame, &TOUCH_EVENTS, current).await;

    let (title, body, color) = match outcome {
        portal::PortalOutcome::Saved(new_settings) => match &store {
            Some(store) => match store.save(flash, &new_settings) {
                Ok(()) => {
                    info!("Saved settings from portal");
                    ("Saved", "restarting", NoticeColor::Good)
                }
                Err(e) => {
                    println!("portal save failed: {e}");
                    ("Save Failed", "restarting", NoticeColor::Warn)
                }
            },
            // No nvs partition: the values were accepted but there is nowhere
            // to put them, so don't claim they were saved.
            None => ("Not Saved", "no storage partition - restarting", NoticeColor::Warn),
        },
        portal::PortalOutcome::Cancelled => ("Cancelled", "restarting", NoticeColor::Info),
    };
    show(
        display,
        ui_frame,
        led,
        &UiState::Notice { title: title.into(), body: body.into(), color },
        0,
    )
    .await;
    Timer::after(Duration::from_secs(2)).await;
    esp_hal::system::software_reset();
}

/// Folds a model reply down to printable ASCII, for both the speaker and the
/// screen.
///
/// Two problems, one fix. `ui.rs` draws with embedded-graphics' *ascii*
/// `FONT_6X10`, which has no glyph outside 0x20..=0x7E and substitutes `?` for
/// everything else - so a smart quote, an em dash or an emoji shows up as a
/// literal question mark in the caption. And the TTS side either reads those
/// characters out by name or chokes on them. Rather than drop the non-ASCII
/// characters that actually carry meaning, spell the common ones out the way
/// they'd be said; `°` matters more as "degrees" than as a missing glyph.
///
/// Markdown emphasis markers are stripped too. The system prompt asks the model
/// not to emit them, but a stray `**` gets read aloud as "asterisk asterisk" by
/// some voices, and this is cheaper than trusting the instruction.
fn speakable(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    // Tracks whether the previous character written was a space, so runs of
    // whitespace (including whatever the substitutions below introduce)
    // collapse instead of stacking up.
    let mut at_line_start = true;

    for c in s.chars() {
        let sub: &str = match c {
            // Quotes and apostrophes.
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '\u{2032}' => "'",
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' | '\u{2033}' => "\"",
            '\u{00AB}' => "\"",
            '\u{00BB}' => "\"",
            // Dashes are left as dashes. Em dashes used to become ", " here,
            // which rewrote the model's punctuation behind its back; the system
            // prompt tells it not to use them instead. All that happens now is
            // the fold to an ASCII hyphen the font can actually draw, since
            // FONT_6X10 has no glyph for the Unicode ones and would show `?`.
            '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}' | '\u{2212}' => "-",
            '\u{2026}' => "...",
            // Spaces of every width, plus bullets and other list furniture.
            '\u{00A0}' | '\u{2007}' | '\u{2009}' | '\u{200A}' | '\u{202F}' => " ",
            '\u{2022}' | '\u{2023}' | '\u{25CF}' | '\u{25AA}' | '\u{00B7}' => " ",
            // Symbols that mean something out loud.
            '\u{00B0}' => " degrees",
            '\u{00D7}' => " times ",
            '\u{00F7}' => " divided by ",
            '\u{00B1}' => " plus or minus ",
            '\u{2260}' => " not equal to ",
            '\u{2264}' => " less than or equal to ",
            '\u{2265}' => " greater than or equal to ",
            '\u{2248}' => " about ",
            '\u{2192}' | '\u{2794}' => " to ",
            '\u{00BC}' => " one quarter",
            '\u{00BD}' => " one half",
            '\u{00BE}' => " three quarters",
            '\u{20AC}' => " euros",
            '\u{00A3}' => " pounds",
            '\u{00A5}' => " yen",
            '\u{00A9}' => "",
            '\u{00AE}' | '\u{2122}' => "",
            // Markdown leftovers.
            '*' | '`' | '_' => "",
            '#' if at_line_start => "",
            // Everything else printable passes through; anything outside the
            // font's range (emoji, CJK, box drawing) is dropped rather than
            // rendered as `?`.
            _ if c.is_ascii_graphic() => {
                out.push(c);
                at_line_start = false;
                continue;
            }
            _ if c.is_whitespace() => " ",
            _ => "",
        };
        for sc in sub.chars() {
            if sc == ' ' && (out.is_empty() || out.ends_with(' ')) {
                continue;
            }
            out.push(sc);
        }
        // `#` only counts as a heading marker at the start of a line, and a
        // heading marker doesn't end one.
        if c == '\n' {
            at_line_start = true;
        } else if !c.is_whitespace() && c != '#' {
            at_line_start = false;
        }
    }

    while out.ends_with(' ') {
        out.pop();
    }
    out
}

fn find_sentence_end(s: &str) -> Option<usize> {
    for (i, c) in s.char_indices() {
        if c == '.' || c == '!' || c == '?' || c == '\n' {
            return Some(i + c.len_utf8());
        }
    }
    // Flush if the buffer is getting long even without punctuation, so a
    // long run-on doesn't delay speech indefinitely.
    if s.len() > 200 {
        return Some(s.len());
    }
    None
}

/// Redraws `state` forever, advancing its animation tick. Never finishes: it's
/// meant to be the losing arm of a `select` against a network wait. Any await
/// that can leave the screen static for more than a moment should race against
/// this - a frozen animation is indistinguishable from a crash.
/// `due` is an *absolute* deadline carried by the caller across iterations, not
/// a fresh `Timer::after`. A read loop rebuilds this future on every pass, and
/// a relative timer would restart with it - which is exactly why the spinner
/// used to sit frozen for a whole reply: tokens arrived every few milliseconds,
/// so the 120ms timer never once got to expire.
async fn animate<DI, RST>(
    display: &mut lcd_async::Display<DI, GC9A01, RST>,
    ui_frame: &mut [u8],
    led: &mut StatusLed,
    tick: &mut u32,
    due: &mut embassy_time::Instant,
    state: &UiState,
) where
    DI: lcd_async::interface::Interface<Word = u8>,
    RST: embedded_hal::digital::OutputPin,
{
    loop {
        Timer::at(*due).await;
        frame(display, ui_frame, led, tick, due, state, SPINNER_FRAME_MS).await;
    }
}

/// Draws one animation frame and schedules the next `every` milliseconds out.
async fn frame<DI, RST>(
    display: &mut lcd_async::Display<DI, GC9A01, RST>,
    ui_frame: &mut [u8],
    led: &mut StatusLed,
    tick: &mut u32,
    due: &mut embassy_time::Instant,
    state: &UiState,
    every: u64,
) where
    DI: lcd_async::interface::Interface<Word = u8>,
    RST: embedded_hal::digital::OutputPin,
{
    *due = embassy_time::Instant::now() + Duration::from_millis(every);
    *tick = tick.wrapping_add(1);
    show(display, ui_frame, led, state, *tick).await;
}

/// Draws a frame only if one is due. Needed in loops that spin fast enough that
/// the `animate` arm of a `select` never wins - when every read returns
/// immediately, the first future is always ready and the timer is never polled.
///
/// `every` is the interval to schedule the *next* frame at, and callers in a
/// data path should pass `STREAM_FRAME_MS` rather than `SPINNER_FRAME_MS`: here
/// the frame is drawn inline, so its cost comes straight out of the loop's
/// read throughput.
async fn frame_if_due<DI, RST>(
    display: &mut lcd_async::Display<DI, GC9A01, RST>,
    ui_frame: &mut [u8],
    led: &mut StatusLed,
    tick: &mut u32,
    due: &mut embassy_time::Instant,
    state: &UiState,
    every: u64,
) where
    DI: lcd_async::interface::Interface<Word = u8>,
    RST: embedded_hal::digital::OutputPin,
{
    if embassy_time::Instant::now() >= *due {
        frame(display, ui_frame, led, tick, due, state, every).await;
    }
}

/// Standing instructions sent as the first message of every conversation.
///
/// The reply is both spoken by a text-to-speech engine and captioned on the
/// round display, which is why brevity is a hard constraint rather than a
/// style preference. The Speaking card fits roughly 330 characters (30 columns
/// by 11 rows, see `ui.rs`) before `wrap()` truncates it, and the whole reply
/// is synthesised before a single word comes out - so length is also directly
/// how long the user stares at a spinner. Left alone, a chat-tuned model
/// answers like it's writing a web page: paragraphs, bullets, bold headers.
///
/// The length rule asks for one to three sentences *scaled to the question*
/// rather than a flat cap. A flat "one or two" pinned everything at one
/// sentence, including "what is X" questions where the extra clause is the
/// whole point; three medium sentences is also about where the 330 characters
/// run out, so the ceiling and the display agree.
const SYSTEM_PROMPT: &str = "\
You are a voice assistant built into a small handheld speaker with a round \
screen, like an Alexa or a Google Home. The person is talking to you out loud. \
They hear your answer read aloud and at the same time see it captioned on a \
screen that fits about 330 characters. Anything longer is cut off and takes \
noticeably longer before it starts playing.

Answer in one to three spoken sentences, matched to what was asked. A lookup \
like the time, the date or a quick fact gets one sentence. A question about \
what something is, how it works, or why it happens gets two or three: the \
direct answer, then the detail that actually makes it useful. Never go past \
three sentences. If the full story needs more than that, give the most useful \
part and stop, so they can ask a follow-up.

Get straight to it with no preamble, no restating the question, and no offers \
of further help unless asked.

Write the way a person talks, since every word is both spoken and shown as \
plain text. Plain sentences only: no markdown, no bullet points, no numbered \
lists, no headings, no bold or italics, and no emoji. Write numbers, units, \
dates and symbols out as they should be spoken.

Never use a dash of any kind in your reply. No em dashes, no en dashes, and no \
hyphens used as punctuation. Where you would reach for one, use a comma, a \
colon, or a separate sentence. This is a hard rule, not a preference.

If you don't know something, say so in one sentence.

Three sentences is roughly all the screen holds, so it is a real ceiling, not \
a style note.";

/// Back-and-forth pairs in `history`, for the Idle card's counter.
///
/// Element 0 is always the system prompt and never counts. Rounding up rather
/// than down is deliberate: a turn the model never answered (a stall, a
/// transport error) leaves a user message with no reply, and that message is
/// still context the next request carries - showing 0 there would say the
/// conversation is fresh when it isn't.
fn exchange_count(history: &[ChatMessage]) -> usize {
    history.len().saturating_sub(1).div_ceil(2)
}

/// The standing instructions as a chat message. Also what a cleared history
/// resets to, since dropping it would quietly turn the device back into a
/// chatbot after an idle timeout.
fn system_message() -> ChatMessage {
    ChatMessage { role: "system".into(), content: SYSTEM_PROMPT.into() }
}

/// Why the reply is synthesised in one request rather than in chunks:
///
/// Splitting it means one `client.request()` per chunk, and that call is a TCP
/// connect plus a full TLS handshake. The handshake is solid CPU work on a
/// single-core cooperative executor, so nothing else - including the screen -
/// gets to run while it happens. Chunking therefore bought a shorter wait
/// before the first word at the cost of a multi-second frozen screen at every
/// seam, which is worse. Making chunking viable needs connection reuse
/// (`HttpClient::resource`), not a smaller chunk size.
const _: () = ();

/// Paints one step of the startup progress ring.
async fn boot_screen<DI, RST>(
    display: &mut lcd_async::Display<DI, GC9A01, RST>,
    ui_frame: &mut [u8],
    step: &'static str,
    progress: u8,
) where
    DI: lcd_async::interface::Interface<Word = u8>,
    RST: embedded_hal::digital::OutputPin,
{
    ui::render(display, ui_frame, &UiState::Boot { step, progress }, 0).await;
}

/// Speaks one sentence. Returns `true` if the user tapped the screen to cut it
/// off, which the caller uses to abandon the rest of the reply.
#[allow(clippy::too_many_arguments)]
async fn speak<'a, C, DI, RST>(
    client: &mut HttpClient<'a, C, DnsSocket<'a>>,
    tts_url: &str,
    tts_auth: &str,
    tts_model: &str,
    text: &str,
    i2s_tx: &mut esp_hal::i2s::master::I2sTx<'static, Async>,
    tx_buffer: &mut [u8],
    pa_enable: &mut Output<'static>,
    led: &mut Ws2812<'static, { buffer_len(1) }>,
    display: &mut lcd_async::Display<DI, GC9A01, RST>,
    ui_frame: &mut [u8],
    anim: u32,
) -> bool
where
    C: embedded_nal_async::TcpConnect,
    DI: lcd_async::interface::Interface<Word = u8>,
    RST: embedded_hal::digital::OutputPin,
{
    let text = text.trim();
    if text.is_empty() {
        return false;
    }

    // Split the reply into captions up front. The audio is one clip, so the
    // only thing sentences are used for now is deciding what's on screen.
    let mut segments: Vec<String> = Vec::new();
    {
        let mut rest = text;
        while let Some(cut) = find_sentence_end(rest) {
            let (head, tail) = rest.split_at(cut);
            if !head.trim().is_empty() {
                segments.push(head.trim().to_string());
            }
            rest = tail;
        }
        if !rest.trim().is_empty() {
            segments.push(rest.trim().to_string());
        }
    }
    if segments.is_empty() {
        segments.push(text.to_string());
    }
    // Running byte offsets, used to map "fraction of the clip played" onto a
    // caption. Sum of the trimmed segments, not `text.len()`, so the fractions
    // line up with what's actually in `segments`.
    let mut bounds: Vec<usize> = Vec::with_capacity(segments.len());
    let mut acc = 0usize;
    for s in &segments {
        acc += s.len();
        bounds.push(acc);
    }
    let total_len = acc.max(1);

    let tts_req = TtsRequest { model: tts_model, input: text, voice: TTS_VOICE, response_format: "wav" };
    let Ok(tts_body) = serde_json::to_vec(&tts_req) else { return false };

    // Still Thinking until there is actually audio to play. Switching to the
    // Speaking card here instead made the device look like it had started
    // talking seconds before any sound came out.
    let mut tick = anim;
    let fetching = UiState::Thinking { tool: None };
    let t0 = embassy_time::Instant::now();
    let mut next_frame = t0;

    // Connect and send are bounded too, not just the body read. These are
    // where a turn actually got stranded: a TLS handshake or request that never
    // completes has no timeout of its own, so the state machine sat here with
    // the last frame frozen on screen and no way back to Idle.
    let connect = match embassy_futures::select::select(
        embassy_time::with_timeout(
            Duration::from_secs(STREAM_STALL_SECS),
            client.request(Method::POST, tts_url),
        ),
        animate(display, ui_frame, led, &mut tick, &mut next_frame, &fetching),
    )
    .await
    {
        embassy_futures::select::Either::First(r) => r,
        embassy_futures::select::Either::Second(()) => return false,
    };
    let Ok(Ok(req)) = connect else {
        println!("TTS request build error/timeout");
        return false;
    };
    let t_connect = t0.elapsed().as_millis();
    let body_slice: &[u8] = &tts_body;
    let headers = [
        ("Authorization", tts_auth),
        ("Content-Type", "application/json"),
        ("Connection", "close"),
    ];
    let mut req = req.body(body_slice).headers(&headers);
    let mut rx_buf = vec![0u8; 4096];
    let sent = match embassy_futures::select::select(
        embassy_time::with_timeout(Duration::from_secs(STREAM_STALL_SECS), req.send(&mut rx_buf)),
        animate(display, ui_frame, led, &mut tick, &mut next_frame, &fetching),
    )
    .await
    {
        embassy_futures::select::Either::First(r) => r,
        embassy_futures::select::Either::Second(()) => return false,
    };
    let response = match sent {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            println!("TTS send error: {e:?}");
            return false;
        }
        Err(_) => {
            println!("TTS send stalled - no response for {STREAM_STALL_SECS}s");
            return false;
        }
    };
    let t_send = t0.elapsed().as_millis();

    // ----- Streaming playback -----
    //
    // The clip is played while it is still downloading, so the seconds that
    // used to be spent waiting for the last byte hide behind audio that has
    // already started. Two things make that safe.
    //
    // First, the download runs as its own future for the whole of playback
    // rather than being interleaved with it. With I2S_TX_STOP_EN cleared a
    // drained TX FIFO does not pause - it repeats its last sample as a buzz -
    // so a socket read sitting *between* two DMA writes is audible. The earlier
    // version bounded each read to a byte quota and `join`ed it with the write
    // it overlapped, which sounds right and isn't: `join` waits for the slowest
    // arm, so a slow server turned straight into FIFO drain. It measured 149
    // blocks of 170ms audio taking 736ms each - 110 seconds of buzz across one
    // reply. Reading in a separate future means a slow read delays nothing, it
    // just leaves less audio ready, which the next point handles.
    //
    // Second, playback doesn't start until the clip is projected to finish
    // downloading before it finishes playing. The speaker drains a fixed
    // 48 KB/s and cannot be slowed down, so a server that synthesises slower
    // than realtime - this one has been measured at 2 KB/s on a long reply -
    // cannot be streamed at all, however the reads are scheduled. For those
    // clips the projection degrades on its own into download-then-play and the
    // audio stays clean; when the server is fast only the floor applies and
    // short replies keep the halved time-to-first-word.
    let content_length = response.content_length;
    let mut reader = response.body().reader();
    let mut clip: Vec<u8> = Vec::with_capacity(TTS_PREBUFFER_BYTES * 2);
    let mut eof = false;

    // Prebuffer. This is the only part the user waits through, and it still
    // animates and still honours the stall timeout.
    let mut plan: Option<(usize, usize)> = None; // (data start, total PCM bytes)
    {
        let mut chunk = [0u8; 2048];
        loop {
            if eof {
                break;
            }
            // The projection needs the WAV header, so it can only be evaluated
            // once the floor is met - which costs nothing, since starting
            // before the floor is a bad idea regardless of the arithmetic.
            if clip.len() >= TTS_PREBUFFER_BYTES {
                let (data_start, total_pcm) = *plan.get_or_insert_with(|| {
                    let data_start = wav::find_data_chunk(&clip);
                    // Content-Length is exact when the server sends one; the
                    // WAV header's own data size is the fallback (rejected by
                    // `data_chunk_len` when it's a streaming placeholder); and
                    // a rate estimate from the text is the last resort.
                    let total_pcm = content_length
                        .map(|cl| cl.saturating_sub(data_start))
                        .filter(|n| *n > 0)
                        .or_else(|| wav::data_chunk_len(&clip))
                        .unwrap_or(text.len() * TTS_BYTES_PER_CHAR)
                        .max(1);
                    (data_start, total_pcm)
                });
                // Average delivery rate since the response headers landed,
                // recomputed every chunk so a server that speeds up gets
                // noticed instead of being judged on its slowest moment.
                let ms = t0.elapsed().as_millis().saturating_sub(t_send).max(1);
                let rate = (clip.len() as u64 * 1000 / ms).max(1);
                let left_to_get = (data_start + total_pcm).saturating_sub(clip.len());
                let left_to_play = total_pcm.saturating_sub(clip.len().saturating_sub(data_start));
                let download_ms = left_to_get as u64 * 1000 / rate;
                let playback_ms = left_to_play as u64 * 1000 / (AUDIO_SAMPLE_RATE as u64 * 2);
                if download_ms + TTS_START_MARGIN_MS <= playback_ms {
                    break;
                }
            }
            let read = match embassy_futures::select::select(
                embassy_time::with_timeout(
                    Duration::from_secs(STREAM_STALL_SECS),
                    reader.read(&mut chunk),
                ),
                animate(display, ui_frame, led, &mut tick, &mut next_frame, &fetching),
            )
            .await
            {
                embassy_futures::select::Either::First(r) => r,
                embassy_futures::select::Either::Second(()) => return false,
            };
            // Same fast-path problem as the reply stream: while the body is
            // arriving the read always wins the select, so the frame has to be
            // driven from the loop body.
            frame_if_due(display, ui_frame, led, &mut tick, &mut next_frame, &fetching, SPINNER_FRAME_MS)
                .await;
            match read {
                Ok(Ok(0)) => eof = true,
                Ok(Ok(n)) => clip.extend_from_slice(&chunk[..n]),
                Ok(Err(e)) => {
                    println!("TTS body read error: {e:?}");
                    eof = true;
                }
                Err(_) => {
                    println!("TTS body stalled - no data for {STREAM_STALL_SECS}s");
                    eof = true;
                }
            }
        }
    }
    let t_prebuffer = t0.elapsed().as_millis();

    // A clip short enough to finish inside the floor never reached the
    // projection, so it has no plan to reuse.
    let data_start = match plan {
        Some((d, _)) => d,
        None => wav::find_data_chunk(&clip),
    };
    if clip.len() <= data_start {
        println!("TTS returned no audio ({} bytes)", clip.len());
        return false;
    }
    // Total PCM length, used to map playback position onto a caption. With the
    // body already complete its real length beats any estimate; mid-stream the
    // projection's estimate is all there is. Being wrong here only drifts the
    // captions, never the audio.
    let total_pcm = if eof {
        clip.len() - data_start
    } else {
        plan.map(|(_, t)| t).unwrap_or(1)
    }
    .max(1);

    // A tap that arrived while the clip was buffering is a request to skip, not
    // a barge-in on audio that hasn't started yet - but treat it the same way:
    // the user wants this reply to stop.
    if TOUCH_EVENTS.try_receive().is_ok() {
        return true;
    }

    // Show the sentence only now, not before the request. Buffering takes about
    // a second, and putting the text up first meant the screen ran ahead of the
    // voice by exactly that much.
    show(display, ui_frame, led, &UiState::Speaking { snippet: segments[0].clone() }, tick).await;

    pa_enable.set_high();
    // DMA can't read PSRAM, so stage through `tx_buffer` (internal RAM, from
    // `dma_buffers!`). Blocks are a multiple of 4 bytes because the I2S DMA
    // requires it; the only short/padded block is the last one, so sample byte
    // alignment is never disturbed mid-stream.
    //
    // One buffer, one write at a time, deliberately. Splitting this into a
    // ping-pong pair to overlap the PSRAM staging copy with the DMA was tried
    // and reverted: it halves the block to ~85ms, and a `show()` joined with a
    // write no longer fits inside one. `join` waits for the slowest arm, so the
    // render overshoots the block, and with I2S_TX_STOP_EN cleared the overshoot
    // is the TX unit repeating its last sample - audibly worse and slower than
    // the 540us staging gap it was meant to remove.
    let block = tx_buffer.len() & !3;

    // Shared between the two futures below. A `RefCell`/`Cell` pair rather than
    // a channel because both live on this one task and neither holds a borrow
    // across an await - the reader appends, the player copies out - so the
    // borrows can never overlap.
    let clip = core::cell::RefCell::new(clip);
    let eof = core::cell::Cell::new(eof);

    // Pulls the rest of the body for as long as playback lasts, at whatever
    // rate the server manages. Nothing waits on it: when it's slow the player
    // just finds less audio ready. It parks instead of returning when the body
    // ends, so that finishing first can't cut playback short in the `select`.
    let download = async {
        let mut chunk = [0u8; 2048];
        while !eof.get() {
            match embassy_time::with_timeout(
                Duration::from_secs(STREAM_STALL_SECS),
                reader.read(&mut chunk),
            )
            .await
            {
                Ok(Ok(0)) => eof.set(true),
                Ok(Ok(n)) => clip.borrow_mut().extend_from_slice(&chunk[..n]),
                Ok(Err(e)) => {
                    println!("TTS body read error: {e:?}");
                    eof.set(true);
                }
                Err(_) => {
                    println!("TTS body stalled - no data for {STREAM_STALL_SECS}s");
                    eof.set(true);
                }
            }
        }
        core::future::pending::<()>().await
    };

    let play = async {
        let mut seg = 0usize;
        let mut pos = data_start;
        let mut interrupted = false;
        // Blocks the download was behind on. Should be zero now that the start
        // is gated on the projection; a nonzero count means the server's rate
        // collapsed partway through a clip it had promised it could sustain.
        let mut starved = 0usize;
        let mut t_first_sample = 0u64;
        // How much longer an iteration took than the audio it just played. With
        // I2S_TX_STOP_EN cleared every one of those milliseconds is the TX FIFO
        // repeating its last sample, so this is the number that says whether
        // the clicking is actually gone.
        let mut overrun_total: u64 = 0;
        let mut overrun_max: u64 = 0;
        let mut blocks: usize = 0;
        // The other half of the same question, and the part `overrun` can't
        // see: the wall time between one DMA transfer ending and the next
        // starting. Nothing is feeding the TX FIFO across that window, and the
        // FIFO is only a couple of milliseconds deep, so if this is large it is
        // the crackle - one pop per block boundary, which is what a constant
        // background crackle independent of the network would look like.
        let mut gap_us_total: u64 = 0;
        let mut gap_us_max: u64 = 0;
        let mut last_write_end: Option<embassy_time::Instant> = None;
        let mut redraws: usize = 0;
        // Segment 0's card is already up, drawn just before `pa_enable`, so the
        // first block has nothing new to push.
        let mut last_seg = 0usize;
        let speaking_rgb = ui::led_color(&UiState::Speaking { snippet: String::new() });

        loop {
            // Barge-in: checked once per DMA block (~170ms of audio at 24kHz),
            // which is responsive enough to feel immediate and costs nothing.
            if TOUCH_EVENTS.try_receive().is_ok() {
                interrupted = true;
                break;
            }
            let finished = eof.get();
            let available = clip.borrow().len() - pos;
            if available == 0 && finished {
                break;
            }
            // Every write is a full block except the clip's very last one. A
            // short write finishes before the `show` it is joined with (~20ms
            // of SPI) and the FIFO drains in the gap, so a shortfall is padded
            // with silence instead: a gap in the speech is a far smaller
            // artefact than a held sample.
            let end = if finished {
                let n = block.min(available);
                tx_buffer[..n].copy_from_slice(&clip.borrow()[pos..pos + n]);
                let end = n.next_multiple_of(4).min(tx_buffer.len());
                tx_buffer[n..end].fill(0);
                pos += n;
                end
            } else {
                let n = block.min(available) & !3;
                if n > 0 {
                    tx_buffer[..n].copy_from_slice(&clip.borrow()[pos..pos + n]);
                }
                tx_buffer[n..block].fill(0);
                if n < block {
                    starved += 1;
                }
                pos += n;
                block
            };

            // Which sentence the voice is on, by fraction of the clip played.
            // Approximate - it assumes an even speaking rate across the clip -
            // but there is no gap in the audio for the drift to show up in.
            let spoken = (((pos - data_start) * total_len) / total_pcm).min(total_len);
            while seg + 1 < segments.len() && spoken >= bounds[seg] {
                seg += 1;
            }

            // Only when the caption actually changes, which is why the ring is
            // static while speaking. A frame is a full-framebuffer render (two
            // arcs, ~54,000 pixel tests each) plus a 115KB SPI push, and it is
            // joined with the DMA write - `join` waits for the slowest arm, so
            // any frame that outlasts its block drags the boundary out, and
            // with I2S_TX_STOP_EN cleared that overshoot is the TX unit
            // repeating its last sample. A moving ring needs a frame on a timer
            // whatever the audio is doing; a static one needs a handful a
            // reply. The animation lives on the LED instead, which is 24 bits
            // over RMT and shares neither the SPI bus, GDMA nor PSRAM with the
            // audio path.
            let redraw = seg != last_seg;
            last_seg = seg;
            pulse_led(led, speaking_rgb, tick);
            tick = tick.wrapping_add(1);
            if t_first_sample == 0 {
                t_first_sample = t0.elapsed().as_millis();
            }
            // Milliseconds of audio this block represents, to compare against
            // how long the iteration actually takes.
            let block_ms = (end as u64 * 1000) / (AUDIO_SAMPLE_RATE as u64 * 2);
            let t_block = embassy_time::Instant::now();
            if let Some(prev) = last_write_end {
                let gap = (t_block - prev).as_micros();
                gap_us_total += gap;
                gap_us_max = gap_us_max.max(gap);
            }

            let res = if redraw {
                redraws += 1;
                embassy_futures::join::join(
                    i2s_tx.write_dma_async(&mut tx_buffer[..end]),
                    show(
                        display,
                        ui_frame,
                        led,
                        &UiState::Speaking { snippet: segments[seg].clone() },
                        tick,
                    ),
                )
                .await
                .0
            } else {
                i2s_tx.write_dma_async(&mut tx_buffer[..end]).await
            };
            last_write_end = Some(embassy_time::Instant::now());
            let elapsed = t_block.elapsed().as_millis();
            blocks += 1;
            if elapsed > block_ms {
                overrun_total += elapsed - block_ms;
                overrun_max = overrun_max.max(elapsed - block_ms);
            }
            if let Err(e) = res {
                println!("i2s tx error: {e:?}");
                break;
            }
        }
        (
            interrupted,
            t_first_sample,
            starved,
            blocks,
            overrun_total,
            overrun_max,
            gap_us_total,
            gap_us_max,
            redraws,
        )
    };

    let (
        interrupted,
        t_first_sample,
        starved,
        blocks,
        overrun_total,
        overrun_max,
        gap_us_total,
        gap_us_max,
        redraws,
    ) = match embassy_futures::select::select(play, download).await {
        embassy_futures::select::Either::First(r) => r,
        // `download` parks forever rather than returning, so it can't win.
        embassy_futures::select::Either::Second(()) => unreachable!(),
    };
    let clip = clip.into_inner();

    // What the streaming bought and what it cost. `connect` is TCP plus the TLS
    // handshake and is the one phase that can't be animated through - it is
    // CPU-bound, so the executor never gets to run the redraw.
    println!(
        "TTS timings: connect {t_connect}ms, send {}ms, prebuffer {}ms, first sample {t_first_sample}ms, total {}ms, {} bytes",
        t_send - t_connect,
        t_prebuffer - t_send,
        t0.elapsed().as_millis(),
        clip.len(),
    );
    // The two numbers that decide whether this is fixed. `overrun` is FIFO
    // drain time, audible as clicks and held tones; `delivery` against the
    // 48,000 B/s drain says whether the server could have kept up at all, and
    // so whether the projection was right to make the user wait.
    println!(
        "TTS audio: {blocks} blocks, {starved} starved, overrun total {overrun_total}ms / max {overrun_max}ms, prebuffer {} B/s, delivery {} B/s vs {} B/s drain",
        if t_prebuffer > t_send { clip.len().min(TTS_PREBUFFER_BYTES) as u64 * 1000 / (t_prebuffer - t_send) } else { 0 },
        if t0.elapsed().as_millis() > t_send { clip.len() as u64 * 1000 / (t0.elapsed().as_millis() - t_send) } else { 0 },
        AUDIO_SAMPLE_RATE as u64 * 2,
    );
    // Dead air between DMA transfers, and how hard the clip is driving the DAC.
    // `gap` is the FIFO-drain window the overrun figure can't see; `peak` at or
    // near 32767 with a lot of samples pinned there means the server is
    // clipping and no amount of buffering will make it clean. Counted after
    // playback rather than per block so measuring doesn't widen the gap it is
    // trying to measure.
    let mut peak = 0i32;
    let mut clipped = 0usize;
    // Sample-to-sample jumps. Speech at 24kHz is band-limited enough that
    // adjacent samples never move far; a step of thousands of LSB in one sample
    // period is a discontinuity, which is what a click *is*. Counting them
    // decides the last open question - whether the clicks are already in the
    // audio the server sends, or are being added after it arrives. `at_edge`
    // counts the ones landing on a DMA block boundary, which would point back
    // at our own staging instead.
    let mut max_step = 0i32;
    let mut steps_4k = 0usize;
    let mut steps_12k = 0usize;
    let mut at_edge = 0usize;
    let mut prev = 0i32;
    let samples_per_block = (tx_buffer.len() & !3) / 2;
    for (i, s) in clip[data_start..].chunks_exact(2).enumerate() {
        let v = i16::from_le_bytes([s[0], s[1]]) as i32;
        peak = peak.max(v.abs());
        clipped += usize::from(v.abs() >= 32_700);
        if i > 0 {
            let step = (v - prev).abs();
            max_step = max_step.max(step);
            if step > 4_000 {
                steps_4k += 1;
                if step > 12_000 {
                    steps_12k += 1;
                }
                if i % samples_per_block < 2 {
                    at_edge += 1;
                }
            }
        }
        prev = v;
    }
    println!(
        "TTS output: gap total {}ms / max {}us over {blocks} writes, {redraws} redraws, peak {peak}/32767, {clipped} samples at rail",
        gap_us_total / 1000,
        gap_us_max,
    );
    println!(
        "TTS pcm: max step {max_step}, {steps_4k} steps >4k, {steps_12k} >12k, {at_edge} of those on a block edge"
    );
    match wav::parse_fmt(&clip) {
        Some((fmt, ch, rate, bits)) => println!(
            "TTS wav fmt: tag {fmt}, {ch} ch, {rate} Hz, {bits}-bit (device expects tag 1, 1 ch, {AUDIO_SAMPLE_RATE} Hz, 16-bit)"
        ),
        None => println!("TTS wav fmt: no fmt chunk found"),
    }
    // I2S_TX_STOP_EN is deliberately cleared (BCLK/WS have to keep running for
    // the mic), which means the TX unit repeats its last sample forever once
    // the FIFO drains. Push a short run of silence so what it holds is zero
    // rather than a DC step into the speaker.
    let tail = 2048.min(tx_buffer.len());
    tx_buffer[..tail].fill(0);
    let _ = i2s_tx.write_dma_async(&mut tx_buffer[..tail]).await;
    pa_enable.set_low();
    // Back to the thinking colour between sentences.
    set_led(led, &UiState::Thinking { tool: None });
    interrupted
}

/// Mean absolute amplitude of a little-endian i16 PCM buffer, used as a cheap
/// stand-in for RMS in the silence detector. Deliberately avoids `sqrt`, which
/// `core` doesn't provide without pulling in `libm`, and tracks speech energy
/// just as well for a threshold comparison.
fn mean_abs_level(pcm: &[u8]) -> u32 {
    let mut sum: u64 = 0;
    let mut count: u64 = 0;
    for pair in pcm.chunks_exact(2) {
        let sample = i16::from_le_bytes([pair[0], pair[1]]);
        sum += sample.unsigned_abs() as u64;
        count += 1;
    }
    if count == 0 { 0 } else { (sum / count) as u32 }
}

fn configure_menu(settings: &Settings) -> UiState {
    UiState::Menu {
        title: "Configure",
        rows: vec![
            MenuRow::action("Wi-Fi Setup"),
            MenuRow::stepper("volume", format!("{}", settings.volume)),
            MenuRow::danger("Erase Settings"),
        ],
    }
}

/// The read-only "what is this device actually configured with" list.
///
/// Secrets are shown only as a fixed run of asterisks. The useful question on
/// a screen anyone in the room can see is "did a value make it onto the
/// device", not "what is it" - and a fixed width doesn't even leak the length.
fn setting_entries(settings: &Settings, from_flash: bool) -> Vec<ui::SettingEntry> {
    let mut entries: Vec<ui::SettingEntry> = Vec::with_capacity(settings::FIELD_ORDER.len() + 2);
    for field in settings::FIELD_ORDER {
        let raw = settings.get(field);
        let (value, placeholder) = if raw.is_empty() {
            ("not set".to_string(), true)
        } else if field.is_secret() {
            ("********".to_string(), false)
        } else {
            (raw.to_string(), false)
        };
        entries.push(ui::SettingEntry { label: field.label().into(), value, placeholder });
    }
    entries.push(ui::SettingEntry {
        label: "Speaker volume".into(),
        value: format!("{}", settings.volume),
        placeholder: false,
    });
    // Answers "did my save actually take?" without needing the serial log.
    entries.push(ui::SettingEntry {
        label: "Source".into(),
        value: if from_flash { "saved on device".into() } else { "not configured".into() },
        placeholder: !from_flash,
    });
    entries
}

/// Returns whether the record is now on flash, so callers can keep the
/// "source" line on the settings card honest.
fn save_settings<F: embedded_storage::nor_flash::NorFlash>(
    store: &Option<settings::Store>,
    flash: &mut F,
    settings: &Settings,
) -> bool {
    let Some(store) = store else {
        println!("settings not saved: no nvs partition");
        return false;
    };
    match store.save(flash, settings) {
        Ok(()) => {
            info!("Settings saved to flash");
            true
        }
        Err(e) => {
            println!("settings save failed: {e}");
            false
        }
    }
}

#[embassy_executor::task]
async fn connection_task(mut controller: WifiController<'static>) {
    // Station phase. Runs until the user asks for the setup portal; the
    // reconnect loop itself never finishes, so `select` is what ends it.
    {
        let station = async {
            loop {
                println!("Connecting to wifi...");
                match controller.connect_async().await {
                    Ok(info) => {
                        println!("Wifi connected: {info:?}");
                        let info = controller.wait_for_disconnect_async().await.ok();
                        println!("Wifi disconnected: {info:?}");
                    }
                    Err(e) => println!("Wifi connect failed: {e:?}"),
                }
                Timer::after(Duration::from_millis(5000)).await;
            }
        };
        embassy_futures::select::select(station, PORTAL_REQUEST.wait()).await;
    }

    // Portal phase. `set_config` stops the radio, switches mode and starts it
    // again on its own, so there is nothing to tear down first.
    info!("Switching radio to setup AP");
    let ap = Config::AccessPoint(
        AccessPointConfig::default()
            .with_ssid(portal::PORTAL_SSID)
            .with_auth_method(AuthenticationMethod::None)
            .with_max_connections(4),
    );
    match controller.set_config(&ap) {
        Ok(()) => {
            println!("Setup AP up: {}", portal::PORTAL_SSID);
            PORTAL_UP.signal(true);
        }
        Err(e) => {
            println!("Failed to start setup AP: {e:?}");
            PORTAL_UP.signal(false);
        }
    }
    // Nothing left to drive: the AP stays up until the reboot that ends the
    // portal session.
    core::future::pending::<()>().await
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface<'static>>) {
    runner.run().await
}

/// Same as [`net_task`] but for the setup AP's stack. embassy-executor tasks
/// are monomorphised into a fixed-size pool per `#[task]`, so the two stacks
/// need two declarations rather than two spawns of one.
#[embassy_executor::task]
async fn ap_net_task(mut runner: Runner<'static, Interface<'static>>) {
    runner.run().await
}

/// How long to wait on the touch interrupt before polling anyway. Purely a
/// safety net: if the INT line were dead or misconfigured the device would
/// otherwise become permanently unresponsive to touch. When INT works this
/// timeout never decides anything.
const TOUCH_INT_FALLBACK_MS: u64 = 500;

/// Debounce window for the BOOT button, and also the minimum press the device
/// will act on. Long enough to reject contact bounce, short enough that a
/// normal press still feels instant.
const BUTTON_DEBOUNCE_MS: u64 = 40;

/// Watches the BOOT button (GPIO0) and raises `NEW_CONVERSATION` on each press.
///
/// The button is active-low with a pull-up, so a press is a falling edge. Two
/// details keep it honest: the signal is raised on press rather than release
/// (the user gets feedback while their finger is still down), and the task then
/// waits for the release before looking for another edge, so a held button
/// clears the conversation once instead of repeating.
///
/// GPIO0 is a strapping pin - held low across a reset it puts the ROM into
/// serial download mode - but that is decided before this code runs, so at
/// runtime it is an ordinary input.
#[embassy_executor::task]
async fn button_task(mut button: Input<'static>) {
    loop {
        button.wait_for_falling_edge().await;
        Timer::after(Duration::from_millis(BUTTON_DEBOUNCE_MS)).await;
        // Bounce, not a press: the line came back up during the settle window.
        if button.is_high() {
            continue;
        }
        println!("boot button pressed: new conversation");
        NEW_CONVERSATION.signal(());
        button.wait_for_rising_edge().await;
        Timer::after(Duration::from_millis(BUTTON_DEBOUNCE_MS)).await;
    }
}

#[embassy_executor::task]
async fn touch_task(mut touch: touch::Cst816d<I2c<'static, Async>>, mut int_pin: Input<'static>) {
    loop {
        // ----- Idle: no I2C traffic, no timer wakeups -----
        // The old version polled I2C every 20ms forever, which is 50 bus
        // transactions and 50 task wakeups a second spent discovering that
        // nobody is touching the screen. Blocking on the controller's own
        // interrupt line costs nothing until a finger actually lands, and
        // removes the up-to-20ms delay before we notice one.
        let woke_on_int = embassy_time::with_timeout(
            Duration::from_millis(TOUCH_INT_FALLBACK_MS),
            int_pin.wait_for_falling_edge(),
        )
        .await
        .is_ok();

        // ----- Touch in progress: poll until the finger lifts -----
        // The chip's IRQ behaviour while a finger is down varies with its
        // config (pulse per update vs. held low), so don't depend on it for
        // tracking - just poll for the duration of the touch, which is short.
        let mut first: Option<touch::TouchPoint> = None;
        let mut last: Option<touch::TouchPoint> = None;
        loop {
            match touch.read().await {
                Ok(Some(p)) => {
                    if first.is_none() {
                        if !woke_on_int {
                            // INT missed a real touch - the fallback is doing
                            // actual work, which means the interrupt path is
                            // broken. Should never print.
                            println!("touch: INT missed a press, caught by poll fallback");
                        }
                        first = Some(p);
                    }
                    last = Some(p);
                }
                Ok(None) => {
                    // Classify on release, from where the finger started and
                    // where it ended. The CST816's own gesture byte is not
                    // used: its vertical labels are inverted for this panel.
                    if let (Some(a), Some(b)) = (first, last) {
                        let event = touch::classify(a, b);
                        println!("touch ({},{}) -> ({},{}) = {event:?}", a.x, a.y, b.x, b.y);
                        TOUCH_EVENTS.send(event).await;
                    }
                    // Finger is up (or this was a spurious wake) - back to
                    // waiting on the interrupt.
                    break;
                }
                Err(e) => {
                    println!("touch read error: {e:?}");
                    break;
                }
            }
            Timer::after(Duration::from_millis(20)).await;
        }
    }
}
