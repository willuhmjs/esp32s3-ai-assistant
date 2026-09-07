// CST816D capacitive touch controller driver.
// Protocol extracted from this board's factory firmware (xiaozhi-esp32,
// main/boards/spotpear/sp-esp32-s3-1.28-box/sp-esp32-s3-1.28-box.cc):
// chip ID at reg 0xA3, touch point data is 6 bytes starting at reg 0x02.

use embedded_hal_async::i2c::I2c;

pub const CST816D_ADDR: u8 = 0x15;
const REG_CHIP_ID: u8 = 0xA3;
// 0x01 = GestureID, 0x02 = FingerNum, 0x03..0x06 = X/Y hi/lo. Reading from
// 0x01 instead of 0x02 costs one extra byte and gets the chip's own
// swipe/click gesture recognition for free.
const REG_GESTURE: u8 = 0x01;

pub struct Cst816d<I2C> {
    i2c: I2C,
    addr: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TouchPoint {
    pub x: u16,
    pub y: u16,
    pub gesture: Gesture,
}

/// Gestures the CST816 recognises on-chip, reported in register 0x01.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gesture {
    None,
    SlideDown,
    SlideUp,
    SlideLeft,
    SlideRight,
    SingleClick,
    DoubleClick,
    LongPress,
    Unknown(u8),
}

/// What the main loop actually consumes. Swipe direction is derived from the
/// first and last coordinates of a touch rather than from the CST816's own
/// `Gesture` byte: the chip's vertical labels are inverted relative to this
/// panel's display orientation (a physical downward swipe reports `SlideUp`),
/// and deriving it ourselves keeps the mapping honest and debuggable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchEvent {
    Tap { x: u16, y: u16 },
    SwipeLeft,
    SwipeRight,
    SwipeUp,
    SwipeDown,
}

/// Minimum travel (pixels) before a touch counts as a swipe instead of a tap.
pub const SWIPE_MIN_PX: i32 = 40;

/// Classifies a touch from where it started and where it ended.
pub fn classify(first: TouchPoint, last: TouchPoint) -> TouchEvent {
    let dx = last.x as i32 - first.x as i32;
    let dy = last.y as i32 - first.y as i32;
    if dx.abs() >= SWIPE_MIN_PX && dx.abs() >= dy.abs() {
        return if dx < 0 { TouchEvent::SwipeLeft } else { TouchEvent::SwipeRight };
    }
    if dy.abs() >= SWIPE_MIN_PX {
        return if dy < 0 { TouchEvent::SwipeUp } else { TouchEvent::SwipeDown };
    }
    TouchEvent::Tap { x: last.x, y: last.y }
}

impl From<u8> for Gesture {
    fn from(v: u8) -> Self {
        match v {
            0x00 => Self::None,
            0x01 => Self::SlideDown,
            0x02 => Self::SlideUp,
            0x03 => Self::SlideLeft,
            0x04 => Self::SlideRight,
            0x05 => Self::SingleClick,
            0x0B => Self::DoubleClick,
            0x0C => Self::LongPress,
            other => Self::Unknown(other),
        }
    }
}

impl<I2C, E> Cst816d<I2C>
where
    I2C: I2c<Error = E>,
{
    pub async fn new(mut i2c: I2C, addr: u8) -> Result<Self, E> {
        let mut chip_id = [0u8; 1];
        i2c.write_read(addr, &[REG_CHIP_ID], &mut chip_id).await?;
        log::info!("CST816D chip id: 0x{:02X}", chip_id[0]);
        Ok(Self { i2c, addr })
    }

    /// Returns `Some(point)` if a finger is currently touching, `None` otherwise.
    pub async fn read(&mut self) -> Result<Option<TouchPoint>, E> {
        let mut buf = [0u8; 7];
        self.i2c
            .write_read(self.addr, &[REG_GESTURE], &mut buf)
            .await?;

        let mut num = buf[1] & 0x01;
        if buf[1] == 0xFF {
            num = 0;
        }
        if num == 0 {
            return Ok(None);
        }

        let x = (((buf[2] & 0x0F) as u16) << 8) | buf[3] as u16;
        let y = (((buf[4] & 0x0F) as u16) << 8) | buf[5] as u16;
        Ok(Some(TouchPoint { x, y, gesture: buf[0].into() }))
    }
}
