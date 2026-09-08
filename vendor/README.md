# vendor/

Third-party source vendored into the tree because the upstream release does not
build for `xtensa-esp32s3-none-elf`. Each entry records what was changed, so a
future version bump is a re-vendor plus the same patch rather than an
archaeology exercise.

## rmp3

`rmp3 0.3.1` from crates.io (CC0-1.0, github.com/notviri/rmp3), which bundles
minimp3 (CC0-1.0, github.com/lieff/minimp3). Used by `speak()` in
`src/bin/main.rs` to decode the mp3 the TTS endpoint returns.

**Why it is vendored.** The crate takes its C integer types from `libc`, and
`libc` has no bindings for a bare-metal xtensa target — on
`xtensa-esp32s3-none-elf` it compiles to an empty crate, so every
`libc::c_int` / `libc::c_uchar` in `src/ffi.rs` fails to resolve. There is no
feature or fallback to turn on; the target simply is not in libc's table.

**The patch**, in full:

- `libc::c_int` → `core::ffi::c_int` and `libc::c_uchar` → `core::ffi::c_uchar`
  (11 and 2 sites, in `src/ffi.rs` and `src/lib.rs`). These are the same types —
  `libc` re-exports them from `core::ffi` on the targets it does support — so
  this is a sourcing change, not a semantic one.
- The now-unused `libc` dependency dropped from `Cargo.toml`.

Nothing in `ffi/minimp3.c`, `ffi/minimp3/minimp3.h` or `build.rs` is touched.
The C side needed no changes at all: `cc` picks up the ESP toolchain's
`xtensa-esp32s3-elf-gcc` from `~/export-esp.sh` and produces `libminimp3.a`
for the target as-is.

**Features.** Pulled in with `default-features = false` from the root
`Cargo.toml`. That drops `simd`, which is x86 SSE2 / ARM NEON only and would be
compiled out on xtensa regardless. `std` is *not* in rmp3's default set, and
`float` stays off so `rmp3::Sample` remains `i16` — the format the I2S path
already wants.
