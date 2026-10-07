//! `CanBus` impl over the ESP32 TWAI controller (esp-idf-hal).
//!
//! BENCH: exact `esp_idf_hal::can` call signatures (Timing variant name, tick
//! conversion) must be confirmed against the installed esp-idf-hal version on
//! first build. Logic/shape is final.

use std::time::Duration;

use collector::{BusError, CanBus, CanFrame};
use embedded_can::{ExtendedId, Frame as _, Id, StandardId};
use esp_idf_hal::can::config::{Config, Timing};
use esp_idf_hal::can::{CanDriver, Frame, CAN};
use esp_idf_hal::delay::TickType;
use esp_idf_hal::gpio::{InputPin, OutputPin};
use esp_idf_hal::peripheral::Peripheral;

pub struct TwaiCanBus<'d> {
    driver: CanDriver<'d>,
    listen_only: bool,
}

impl<'d> TwaiCanBus<'d> {
    /// 500 kbit/s TWAI. Rx = GPIO4, Tx = GPIO5 (per the MrDIY CAN shield).
    pub fn new(
        can: impl Peripheral<P = CAN> + 'd,
        tx: impl Peripheral<P = impl OutputPin> + 'd,
        rx: impl Peripheral<P = impl InputPin> + 'd,
    ) -> anyhow::Result<Self> {
        let config = Config::new().timing(Timing::B500K);
        let mut driver = CanDriver::new(can, tx, rx, &config)?;
        driver.start()?;
        Ok(Self { driver, listen_only: true })
    }
}

impl<'d> CanBus for TwaiCanBus<'d> {
    fn send(&mut self, frame: &CanFrame) -> Result<(), BusError> {
        if self.listen_only {
            return Err(BusError::ListenOnly);
        }
        let id: Id = if frame.extended {
            ExtendedId::new(frame.id)
                .map(Id::from)
                .ok_or_else(|| BusError::Driver("invalid extended id".into()))?
        } else {
            StandardId::new(frame.id as u16)
                .map(Id::from)
                .ok_or_else(|| BusError::Driver("invalid standard id".into()))?
        };
        let f = Frame::new(id, &frame.data)
            .ok_or_else(|| BusError::Driver("frame payload too long".into()))?;
        let ticks = TickType::new_millis(100).ticks();
        self.driver
            .transmit(&f, ticks)
            .map_err(|e| BusError::Driver(e.to_string()))
    }

    fn recv(&mut self, timeout: Duration) -> Option<CanFrame> {
        let ticks = TickType::new_millis(timeout.as_millis() as u64).ticks();
        let f = self.driver.receive(ticks).ok()?;
        let (id, extended) = match f.id() {
            Id::Standard(s) => (s.as_raw() as u32, false),
            Id::Extended(e) => (e.as_raw(), true),
        };
        Some(CanFrame { id, extended, data: f.data().to_vec() })
    }

    fn set_listen_only(&mut self, listen_only: bool) {
        self.listen_only = listen_only;
    }
}
