// Runtime-editable configuration, persisted to flash.
//
// Nothing here is baked in at build time. A freshly flashed device has *no*
// network, no endpoints and no model names; it boots straight into the
// on-device setup portal, and everything it is told there is written to flash
// and read back on the next boot. That means one firmware image works on any
// network against any endpoint, and no credential is ever compiled into the
// binary.
//
// Storage lives in the ESP-IDF-style `nvs` data partition that espflash's
// default partition table already provides (0x9000, 24 KB). We do not use the
// ESP-IDF NVS *format* - there is no ESP-IDF here to interoperate with, and a
// single fixed-layout record is far less code than a key/value store. It is
// just a convenient 24 KB of flash that nothing else on this device claims.
//
// Record layout, written at the start of the partition:
//
//   0..4    magic b"E32S"
//   4       format version
//   5       speaker volume 0..=100, as a percentage of MAX_CODEC_VOLUME
//   6..8    payload length, u16 LE
//   8..12   CRC-32 of the payload, u32 LE
//   12..    payload: FIELD_ORDER.len() x (u16 LE length + UTF-8 bytes)
//
// Anything that fails to validate reads as "unconfigured" rather than bricking
// the device, which also makes "reset settings" nothing more than erasing the
// sector.

extern crate alloc;
use alloc::{string::String, string::ToString};

use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};

const MAGIC: [u8; 4] = *b"E32S";
/// Bumped to 2 when the compiled-in defaults were removed and the two host
/// fields became full base URLs carrying a scheme and an API prefix. A v1
/// record decodes as unconfigured, which sends the device to the setup portal
/// - the right thing, since a v1 base URL is a bare host and would build a
/// request URL reqwless can't even parse.
const VERSION: u8 = 2;
const HEADER_LEN: usize = 12;
/// One flash sector holds the whole record; keep the payload comfortably under
/// it so a save is always a single erase+write.
const MAX_RECORD: usize = 4096;
/// The stored volume is a user-facing 0..=100 percentage, not the codec's own
/// scale. 82% of the ceiling below is the codec's 70, which is where this sat
/// before the rescale: audible on this speaker, and the point below which it
/// starts to disappear.
pub const DEFAULT_VOLUME: u8 = 82;

/// Codec volume that the top of the user-facing scale maps to.
///
/// The ES8311 takes 0..=100 and the settings row used to hand it that number
/// straight through, but the top of its range is unusable on this speaker and
/// amplifier - anything past 85 is painfully loud rather than louder, so the
/// last fifth of the stepper was travel nobody could use. Mapping 100% onto 85
/// spends the whole stepper on the range that actually differs.
pub const MAX_CODEC_VOLUME: u8 = 85;

/// The OpenAI-compatible operation paths this firmware calls. They are the
/// only part of a request URL that is fixed: everything to their left is the
/// user-configured base URL, so the same firmware talks to a server rooted at
/// `https://host/v1` and one rooted at `https://host/api/v1` without knowing
/// anything about either.
///
/// They double as the suffixes `Settings::set` strips, so pasting a complete
/// endpoint URL into the setup form still leaves a usable base.
pub const OP_CHAT_COMPLETIONS: &str = "/chat/completions";
pub const OP_TRANSCRIPTIONS: &str = "/audio/transcriptions";
pub const OP_SPEECH: &str = "/audio/speech";
const OPERATION_PATHS: [&str; 3] = [OP_CHAT_COMPLETIONS, OP_TRANSCRIPTIONS, OP_SPEECH];

/// The editable string fields, in serialization order. Adding a field means
/// bumping `VERSION` (older records then fail validation and read as
/// unconfigured, which is the correct behaviour).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    WifiSsid,
    WifiPassword,
    AgentBaseUrl,
    AgentApiKey,
    AgentModel,
    SpeechBaseUrl,
    SpeechApiKey,
    SpeechSttModel,
    SpeechTtsModel,
}

/// Also the order the setup form renders in, so it reads network first, then
/// agent, then speech.
pub const FIELD_ORDER: [Field; 9] = [
    Field::WifiSsid,
    Field::WifiPassword,
    Field::AgentBaseUrl,
    Field::AgentApiKey,
    Field::AgentModel,
    Field::SpeechBaseUrl,
    Field::SpeechApiKey,
    Field::SpeechSttModel,
    Field::SpeechTtsModel,
];

impl Field {
    /// Stable identifier used as the HTML form field name.
    pub const fn key(self) -> &'static str {
        match self {
            Field::WifiSsid => "wifi_ssid",
            Field::WifiPassword => "wifi_password",
            Field::AgentBaseUrl => "agent_base_url",
            Field::AgentApiKey => "agent_api_key",
            Field::AgentModel => "agent_model",
            Field::SpeechBaseUrl => "speech_base_url",
            Field::SpeechApiKey => "speech_api_key",
            Field::SpeechSttModel => "speech_stt_model",
            Field::SpeechTtsModel => "speech_tts_model",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Field::WifiSsid => "Wi-Fi network (SSID)",
            Field::WifiPassword => "Wi-Fi password",
            Field::AgentBaseUrl => "Agent base URL",
            Field::AgentApiKey => "Agent API key",
            Field::AgentModel => "Agent model",
            Field::SpeechBaseUrl => "Speech base URL",
            Field::SpeechApiKey => "Speech API key",
            Field::SpeechSttModel => "Transcription model",
            Field::SpeechTtsModel => "Voice model",
        }
    }

    /// Example value shown as the form's placeholder. With nothing prefilled on
    /// a fresh device, the shape of the value is the only clue the user gets -
    /// that a base URL includes the API prefix is not guessable from the label.
    pub const fn hint(self) -> &'static str {
        match self {
            Field::WifiSsid => "network name",
            Field::WifiPassword => "blank for an open network",
            Field::AgentBaseUrl => "https://host.example.net/v1",
            Field::AgentApiKey => "blank if the endpoint needs no key",
            Field::AgentModel => "model id",
            Field::SpeechBaseUrl => "https://host.example.net/v1",
            Field::SpeechApiKey => "blank if the endpoint needs no key",
            Field::SpeechSttModel => "transcription model id",
            Field::SpeechTtsModel => "voice model id",
        }
    }

    /// Secrets are never echoed back into the setup form.
    pub const fn is_secret(self) -> bool {
        matches!(self, Field::WifiPassword | Field::AgentApiKey | Field::SpeechApiKey)
    }

    /// Base URLs are complete `scheme://host[:port][/prefix]` values with no
    /// trailing slash - everything a request URL needs except the operation.
    ///
    /// The scheme is the user's to state, not ours to assume: `http` and
    /// `https` are both reachable from this firmware and picking one silently
    /// would either downgrade a secure endpoint or break a plaintext one. The
    /// prefix is part of the value for the same reason - one OpenAI-compatible
    /// server sits at `/v1`, another behind `/api/v1`. The firmware only ever
    /// appends the standard operation paths (`/chat/completions`,
    /// `/audio/transcriptions`, `/audio/speech`).
    pub const fn is_base_url(self) -> bool {
        matches!(self, Field::AgentBaseUrl | Field::SpeechBaseUrl)
    }

    /// Whether `value` is usable for this field.
    ///
    /// Blank fails only for required fields; a base URL additionally has to
    /// carry an explicit scheme, since `endpoint()` concatenates rather than
    /// guessing and a bare host would produce a URL reqwless can't parse.
    pub fn accepts(self, value: &str) -> bool {
        if value.is_empty() {
            return !self.is_required();
        }
        if self.is_base_url() {
            return value.starts_with("https://") || value.starts_with("http://");
        }
        true
    }

    /// Whether the firmware can do anything at all without this field.
    ///
    /// The two API keys and the Wi-Fi password are deliberately optional: an
    /// endpoint that needs no auth and an open network are both legitimate
    /// configurations, and demanding a value would make them unconfigurable.
    pub const fn is_required(self) -> bool {
        !matches!(
            self,
            Field::WifiPassword | Field::AgentApiKey | Field::SpeechApiKey
        )
    }
}

#[derive(Clone)]
pub struct Settings {
    fields: [String; FIELD_ORDER.len()],
    /// User-facing 0..=100. Pass `codec_volume()` to the ES8311, never this.
    pub volume: u8,
}

impl Settings {
    /// `volume` mapped onto the codec's usable range. See `MAX_CODEC_VOLUME`.
    pub fn codec_volume(&self) -> u8 {
        (self.volume.min(100) as u16 * MAX_CODEC_VOLUME as u16 / 100) as u8
    }

    /// A blank device: no network, no endpoints, no models.
    ///
    /// This is what a first boot and an erased record both produce, and it is
    /// deliberately unusable - `first_invalid()` reports a blank required
    /// field, so `main` sends it to the setup portal instead of trying to run
    /// on nothing. The volume is the one exception, because there is no
    /// sensible "unset" speaker level.
    pub fn unconfigured() -> Self {
        Self {
            fields: [const { String::new() }; FIELD_ORDER.len()],
            volume: DEFAULT_VOLUME,
        }
    }

    /// The first field that isn't usable, in form order - which is also the
    /// first one the user should be sent back to fix.
    pub fn first_invalid(&self) -> Option<Field> {
        FIELD_ORDER.into_iter().find(|f| !f.accepts(self.get(*f)))
    }

    fn index(field: Field) -> usize {
        FIELD_ORDER.iter().position(|f| *f == field).expect("field is in FIELD_ORDER")
    }

    pub fn get(&self, field: Field) -> &str {
        &self.fields[Self::index(field)]
    }

    pub fn set(&mut self, field: Field, value: impl Into<String>) {
        let mut value: String = value.into();
        if field.is_base_url() {
            // Be forgiving about the tail end of what gets pasted in - people
            // copy a whole endpoint URL out of a docs page, operation path and
            // all, and that path would otherwise end up doubled. The scheme is
            // deliberately left exactly as typed: `accepts()` rejects a value
            // without one rather than inventing it.
            let cleaned = {
                let mut v = value.trim().trim_end_matches('/');
                for op in OPERATION_PATHS {
                    v = v.strip_suffix(op).unwrap_or(v);
                }
                v.trim_end_matches('/').to_string()
            };
            value = cleaned;
        }
        self.fields[Self::index(field)] = value;
    }

    /// Full request URL for one operation on one of the two services.
    ///
    /// `op` is a standard OpenAI-compatible path such as `/chat/completions`;
    /// everything before it is whatever the user configured, scheme and prefix
    /// included.
    pub fn endpoint(&self, base: Field, op: &str) -> String {
        alloc::format!("{}{}", self.get(base), op)
    }

    /// `Authorization` header value for one of the two services. Still well
    /// formed when the key is blank, which is what an endpoint that needs no
    /// auth sees - and what it ignores.
    pub fn bearer(&self, key: Field) -> String {
        alloc::format!("Bearer {}", self.get(key))
    }

    pub fn wifi_ssid(&self) -> &str {
        self.get(Field::WifiSsid)
    }
    pub fn wifi_password(&self) -> &str {
        self.get(Field::WifiPassword)
    }

    /// Serializes into `out` (which must be internal RAM - see `SCRATCH`) and
    /// returns the used length, already padded to a 4-byte flash word.
    fn encode_into(&self, out: &mut [u8]) -> Option<usize> {
        let mut cursor = HEADER_LEN;
        for field in FIELD_ORDER {
            let bytes = self.get(field).as_bytes();
            let len = u16::try_from(bytes.len()).ok()?;
            out.get_mut(cursor..cursor + 2)?.copy_from_slice(&len.to_le_bytes());
            cursor += 2;
            out.get_mut(cursor..cursor + bytes.len())?.copy_from_slice(bytes);
            cursor += bytes.len();
        }
        let payload_len = u16::try_from(cursor - HEADER_LEN).ok()?;
        let crc = crc32(&out[HEADER_LEN..cursor]);

        out[..4].copy_from_slice(&MAGIC);
        out[4] = VERSION;
        out[5] = self.volume;
        out[6..8].copy_from_slice(&payload_len.to_le_bytes());
        out[8..12].copy_from_slice(&crc.to_le_bytes());

        // Flash writes must be a whole number of 4-byte words.
        let padded = cursor.next_multiple_of(4);
        out.get_mut(cursor..padded)?.fill(0);
        Some(padded)
    }

    fn decode(record: &[u8]) -> Option<Self> {
        if record.len() < HEADER_LEN || record[..4] != MAGIC || record[4] != VERSION {
            return None;
        }
        let volume = record[5].min(100);
        let payload_len = u16::from_le_bytes([record[6], record[7]]) as usize;
        let expected_crc = u32::from_le_bytes([record[8], record[9], record[10], record[11]]);
        let payload = record.get(HEADER_LEN..HEADER_LEN + payload_len)?;
        if crc32(payload) != expected_crc {
            return None;
        }

        let mut settings = Self::unconfigured();
        settings.volume = volume;
        let mut cursor = 0usize;
        for field in FIELD_ORDER {
            let len_bytes = payload.get(cursor..cursor + 2)?;
            let len = u16::from_le_bytes([len_bytes[0], len_bytes[1]]) as usize;
            cursor += 2;
            let bytes = payload.get(cursor..cursor + len)?;
            cursor += len;
            settings.fields[Self::index(field)] = core::str::from_utf8(bytes).ok()?.to_string();
        }
        Some(settings)
    }
}

/// Bitwise CRC-32 (IEEE). No table, because a 1 KB lookup table would cost
/// more than the handful of microseconds this saves on a record we touch
/// twice per boot at most.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Staging buffer for every flash read and write.
///
/// This must live in internal RAM. `esp-storage` disables the instruction
/// cache for the duration of a flash operation, and on the ESP32-S3 PSRAM is
/// reached *through* that same cache - handing it a PSRAM buffer (which is
/// what any `Vec` here would be, since the global allocator is the PSRAM one)
/// would fault or silently read garbage. A plain `static` lands in `.bss`,
/// i.e. internal DRAM, which is exactly what's needed.
///
/// It is also why this is a `static` rather than a local: 4 KB is far too much
/// to put on `main()`'s stack, which has overflowed before on this firmware.
static mut SCRATCH: [u8; MAX_RECORD] = [0; MAX_RECORD];

/// Buffer for the partition table read, for the same internal-RAM reason.
static mut TABLE_SCRATCH: [u8; 1024] = [0; 1024];

/// Where in flash the settings record lives, resolved once from the partition
/// table so a partition-table change can't silently corrupt something else.
#[derive(Clone, Copy)]
pub struct Store {
    offset: u32,
    size: u32,
}

impl Store {
    /// Locates the `nvs` data partition. Returns `None` if the partition table
    /// can't be read or has no NVS entry, in which case the caller should run
    /// on compiled-in defaults with persistence disabled.
    ///
    /// Call once, from `main()`, before anything else can touch the scratch
    /// buffers.
    pub fn locate<F: embedded_storage::Storage>(flash: &mut F) -> Option<Self> {
        use esp_bootloader_esp_idf::partitions;

        // SAFETY: all settings I/O happens on `main()`'s own task, one call at
        // a time, and nothing else in the firmware references these buffers.
        let table_buf = unsafe { &mut *core::ptr::addr_of_mut!(TABLE_SCRATCH) };
        let table = partitions::read_partition_table(flash, table_buf).ok()?;
        let entry = table
            .find_partition(partitions::PartitionType::Data(
                partitions::DataPartitionSubType::Nvs,
            ))
            .ok()??;
        Some(Self { offset: entry.offset(), size: entry.len() })
    }

    pub fn load<F: ReadNorFlash>(&self, flash: &mut F) -> Option<Settings> {
        // SAFETY: see `locate`.
        let buf = unsafe { &mut *core::ptr::addr_of_mut!(SCRATCH) };
        let len = MAX_RECORD.min(self.size as usize);
        flash.read(self.offset, &mut buf[..len]).ok()?;
        Settings::decode(&buf[..len])
    }

    pub fn save<F: NorFlash>(&self, flash: &mut F, settings: &Settings) -> Result<(), &'static str> {
        // SAFETY: see `locate`.
        let buf = unsafe { &mut *core::ptr::addr_of_mut!(SCRATCH) };
        let len = settings.encode_into(buf).ok_or("settings too large to store")?;
        let sector = F::ERASE_SIZE;
        let erase_len = len.next_multiple_of(sector) as u32;
        if erase_len > self.size {
            return Err("settings record exceeds nvs partition");
        }
        flash
            .erase(self.offset, self.offset + erase_len)
            .map_err(|_| "flash erase failed")?;
        flash.write(self.offset, &buf[..len]).map_err(|_| "flash write failed")?;
        Ok(())
    }

    /// Wipes the stored record. The next boot then finds nothing, reads as
    /// unconfigured, and goes to the setup portal.
    pub fn clear<F: NorFlash>(&self, flash: &mut F) -> Result<(), &'static str> {
        let sector = F::ERASE_SIZE as u32;
        flash
            .erase(self.offset, self.offset + sector)
            .map_err(|_| "flash erase failed")
    }
}
