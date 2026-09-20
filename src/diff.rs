// Dirty-rect display flushing.
//
// Every state change used to repaint the whole 240x240 panel: `ui::render`
// cleared the framebuffer, redrew everything, then pushed all 115,200 bytes
// over SPI. At 40 MHz that is ~29ms of transfer per frame before counting
// the redraw, and it happened even when the new frame was pixel-identical
// to the old one - which is why the UI felt laggy.
//
// `flush_changed` replaces that blind flush. It compares the freshly drawn
// frame against a copy of what the panel currently shows:
//
//   * Few pixels changed (spinner notch, ring tick, a label) - only the
//     changed horizontal runs are sent, each as its own small address
//     window. A handful of KB instead of 115KB.
//   * Nothing changed - nothing is sent at all.
//   * Most of the screen changed (a state transition) - one full-screen
//     transfer, the same wire time the old code always paid. Run-per-row
//     flushing would add per-row window-command overhead on top of the
//     data, so past a threshold the single big window wins.
//
// The compare is over u32 words (two Rgb565 pixels each), which halves the
// scan work and can only ever over-report by one pixel at a run's edge -
// sending a pixel that did not change is harmless, skipping one that did
// is not, so the granularity errs safe.
//
// The previous-frame copy is the second half of the caller's `ui_frame`
// buffer (see `main`), allocated once at boot on the same PSRAM heap as the
// draw half. Both are plain CPU-owned memory - the SPI driver copies out of
// them synchronously inside `send_data_slice`, so the PSRAM-vs-DMA concern
// that applies to the radio's packet buffers does not arise here. The
// buffers start zeroed; the first real frame always differs (every state
// draws nonblack content), so the first flush paints.

use lcd_async::{interface::Interface, models::GC9A01, Display};

use crate::pins::{DISPLAY_HEIGHT, DISPLAY_WIDTH};

const W: usize = DISPLAY_WIDTH as usize;
const H: usize = DISPLAY_HEIGHT as usize;

/// Bytes in one full frame: 240 * 240 * 2 (Rgb565).
pub const FRAME_BYTES: usize = W * H * 2;

/// Changed-byte count at which flushing runs loses to one full transfer.
/// A full-screen repaint sends 115KB either way; run windows would add
/// per-row command overhead on top, so a mostly-changed frame goes out as
/// a single window. Tuned to a third of the screen: below it, the run
/// path's data savings dwarf its command overhead; above it they do not.
const FULL_FLUSH_THRESHOLD: usize = FRAME_BYTES / 3;

/// Sends only the pixels where `frame` differs from `prev`, then updates
/// `prev` to match, so the caller never has to think about the pairing.
///
/// `prev` is the panel's current contents; `frame` is the freshly rendered
/// image. Both must be `FRAME_BYTES` long - callers that pass anything else
/// get the old behavior (one full-screen transfer) rather than a panic, so
/// a missed allocation-site update fails visibly but safely.
pub async fn flush_changed<DI, RST>(
    display: &mut Display<DI, GC9A01, RST>,
    prev: &mut [u8],
    frame: &mut [u8],
) where
    DI: Interface<Word = u8>,
    RST: embedded_hal::digital::OutputPin,
{
    if prev.len() != FRAME_BYTES || frame.len() != FRAME_BYTES {
        // Unknown buffer layout: fall back to a full-window repaint, the
        // exact behavior this module replaced.
        display
            .show_raw_data(0, 0, W as u16, H as u16, frame)
            .await
            .ok();
        return;
    }

    // One row is 240 px * 2 B = 480 B = 120 u32 words.
    let row_words = W * 2 / 4;
    debug_assert_eq!(row_words * 4, W * 2);

    // Pass 1: count changed bytes to pick a strategy. Costs ~a millisecond
    // of PSRAM reads and saves tens of milliseconds on transitions.
    let mut changed_bytes = 0usize;
    for y in 0..H {
        let row_off = y * W * 2;
        let frow: &[u8] = &frame[row_off..row_off + W * 2];
        let prow: &[u8] = &prev[row_off..row_off + W * 2];
        for wi in 0..row_words {
            if frow[wi * 4..wi * 4 + 4] != prow[wi * 4..wi * 4 + 4] {
                changed_bytes += 4;
            }
        }
    }

    if changed_bytes == 0 {
        return; // Panel already shows this frame; nothing to send.
    }

    if changed_bytes >= FULL_FLUSH_THRESHOLD {
        // Mostly-changed frame: one window, one transfer, no per-row
        // command overhead.
        display
            .show_raw_data(0, 0, W as u16, H as u16, frame)
            .await
            .ok();
        prev.copy_from_slice(frame);
        return;
    }

    // Pass 2: emit the changed runs. One run per maximal stretch of changed
    // words on a row.
    for y in 0..H {
        let row_off = y * W * 2;
        let frow: &[u8] = &frame[row_off..row_off + W * 2];
        let prow: &[u8] = &prev[row_off..row_off + W * 2];

        // Word index where the current changed run started, if one is open.
        let mut run_start: Option<usize> = None;
        for wi in 0..row_words {
            let differs = frow[wi * 4..wi * 4 + 4] != prow[wi * 4..wi * 4 + 4];
            match (differs, run_start) {
                // Change begins (or continues).
                (true, _) if run_start.is_none() => run_start = Some(wi),
                // Matching word closes an open run.
                (false, Some(start)) => {
                    emit_run(display, frame, y, start, wi).await;
                    run_start = None;
                }
                _ => {}
            }
        }
        // A run that reaches the row's right edge closes here.
        if let Some(start) = run_start {
            emit_run(display, frame, y, start, row_words).await;
        }
    }

    // The panel now shows `frame`; make `prev` say so for next time.
    prev.copy_from_slice(frame);
}

/// Sends one horizontal run of changed pixels: word indices `start..end` of
/// row `y`, i.e. pixels `start*2 .. end*2`.
async fn emit_run<DI, RST>(
    display: &mut Display<DI, GC9A01, RST>,
    frame: &[u8],
    y: usize,
    start: usize,
    end: usize,
) where
    DI: Interface<Word = u8>,
    RST: embedded_hal::digital::OutputPin,
{
    let x0 = start * 2;
    let width = (end - start) * 2;
    let byte0 = y * W * 2 + x0 * 2;
    let byte1 = byte0 + width * 2;
    display
        .show_raw_data(x0 as u16, y as u16, width as u16, 1, &frame[byte0..byte1])
        .await
        .ok();
}
