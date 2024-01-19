//! # I²C Controller-mode code
//!
//! This is for when the RP2040 is actively reading from or writing to other I²C
//! devices on the bus.
//!
//! We implement both the Embedded HAL 1.0 and legacy Embedded HAL 0.2 traits.
//! Currently we only support 7-bit addresses, not 10-bit addresses.

use core::{marker::PhantomData, ops::Deref};
use fugit::HertzU32;

use embedded_hal::i2c;
use embedded_hal_0_2::blocking::i2c as i2c02;

use super::{i2c_reserved_addr, Controller, Error, ValidPinScl, ValidPinSda, I2C};
use crate::{
    pac::{i2c0::RegisterBlock as Block, RESETS},
    resets::SubsystemReset,
};

// ============================================================================
//
// Inherent Methods
//
// ============================================================================

impl<T, Sda, Scl> I2C<T, (Sda, Scl), Controller>
where
    T: SubsystemReset + Deref<Target = Block>,
    Sda: ValidPinSda<T>,
    Scl: ValidPinScl<T>,
{
    /// Configures the I²C peripheral to work in controller mode
    pub fn new_controller(
        i2c: T,
        sda_pin: Sda,
        scl_pin: Scl,
        freq: HertzU32,
        resets: &mut RESETS,
        system_clock: HertzU32,
    ) -> Self {
        let freq = freq.to_Hz();
        assert!(freq <= 1_000_000);
        assert!(freq > 0);

        i2c.reset_bring_down(resets);
        i2c.reset_bring_up(resets);

        i2c.ic_enable.write(|w| w.enable().disabled());

        // select controller mode & speed
        i2c.ic_con.modify(|_, w| {
            w.speed().fast();
            w.master_mode().enabled();
            w.ic_10bitaddr_master().addr_7bits();
            w.ic_10bitaddr_slave().addr_7bits();
            w.ic_slave_disable().slave_disabled();
            w.ic_restart_en().enabled();
            w.tx_empty_ctrl().enabled()
        });

        // Clear FIFO threshold
        i2c.ic_tx_tl.write(|w| unsafe { w.tx_tl().bits(0) });
        i2c.ic_rx_tl.write(|w| unsafe { w.rx_tl().bits(0) });

        let freq_in = system_clock.to_Hz();

        // There are some subtleties to I²C timing which we are completely ignoring here
        // See: https://github.com/raspberrypi/pico-sdk/blob/bfcbefafc5d2a210551a4d9d80b4303d4ae0adf7/src/rp2_common/hardware_i2c/i2c.c#L69
        let period = (freq_in + freq / 2) / freq;
        let lcnt = period * 3 / 5; // spend 3/5 (60%) of the period low
        let hcnt = period - lcnt; // and 2/5 (40%) of the period high

        // Check for out-of-range divisors:
        assert!(hcnt <= 0xffff);
        assert!(lcnt <= 0xffff);
        assert!(hcnt >= 8);
        assert!(lcnt >= 8);

        // Per I²C-bus specification a device in standard or fast mode must
        // internally provide a hold time of at least 300ns for the SDA signal to
        // bridge the undefined region of the falling edge of SCL. A smaller hold
        // time of 120ns is used for fast mode plus.
        let sda_tx_hold_count = if freq < 1000000 {
            // sda_tx_hold_count = freq_in [cycles/s] * 300ns * (1s / 1e9ns)
            // Reduce 300/1e9 to 3/1e7 to avoid numbers that don't fit in uint.
            // Add 1 to avoid division truncation.
            ((freq_in * 3) / 10000000) + 1
        } else {
            // fast mode plus requires a clk_in > 32MHz
            assert!(freq_in >= 32_000_000);

            // sda_tx_hold_count = freq_in [cycles/s] * 120ns * (1s / 1e9ns)
            // Reduce 120/1e9 to 3/25e6 to avoid numbers that don't fit in uint.
            // Add 1 to avoid division truncation.
            ((freq_in * 3) / 25000000) + 1
        };
        assert!(sda_tx_hold_count <= lcnt - 2);

        unsafe {
            i2c.ic_fs_scl_hcnt
                .write(|w| w.ic_fs_scl_hcnt().bits(hcnt as u16));
            i2c.ic_fs_scl_lcnt
                .write(|w| w.ic_fs_scl_lcnt().bits(lcnt as u16));
            i2c.ic_fs_spklen.write(|w| {
                w.ic_fs_spklen()
                    .bits(if lcnt < 16 { 1 } else { (lcnt / 16) as u8 })
            });
            i2c.ic_sda_hold
                .modify(|_r, w| w.ic_sda_tx_hold().bits(sda_tx_hold_count as u16));
        }

        // Enable I²C block
        i2c.ic_enable.write(|w| w.enable().enabled());

        Self {
            i2c,
            pins: (sda_pin, scl_pin),
            mode: PhantomData,
        }
    }
}

impl<T: Deref<Target = Block>, PINS> I2C<T, PINS, Controller> {
    /// Validate user-supplied arguments
    ///
    /// If the arguments are not valid, an Error is returned.
    ///
    /// Checks that:
    ///
    /// * The address is a valid 7-bit I²C address
    /// * The `opt_tx_empty` arg is not `Some(true)`
    /// * The `opt_rx_empty` arg is not `Some(true)`
    fn validate(
        address: u8,
        opt_tx_empty: Option<bool>,
        opt_rx_empty: Option<bool>,
    ) -> Result<(), Error> {
        // validate tx parameters if present
        if opt_tx_empty.unwrap_or(false) {
            return Err(Error::InvalidWriteBufferLength);
        }

        // validate rx parameters if present
        if opt_rx_empty.unwrap_or(false) {
            return Err(Error::InvalidReadBufferLength);
        }

        // validate address
        if address >= 0x80 {
            Err(Error::AddressOutOfRange(address as u16))
        } else if i2c_reserved_addr(address as u16) {
            Err(Error::AddressReserved(address as u16))
        } else {
            Ok(())
        }
    }

    fn setup(&mut self, address: u8) {
        self.i2c.ic_enable.write(|w| w.enable().disabled());
        self.i2c
            .ic_tar
            .write(|w| unsafe { w.ic_tar().bits(address as u16) });
        self.i2c.ic_enable.write(|w| w.enable().enabled());
    }

    fn read_and_clear_abort_reason(&mut self) -> Option<u32> {
        let abort_reason = self.i2c.ic_tx_abrt_source.read().bits();
        if abort_reason != 0 {
            // Note clearing the abort flag also clears the reason, and
            // this instance of flag is clear-on-read! Note also the
            // IC_CLR_TX_ABRT register always reads as 0.
            self.i2c.ic_clr_tx_abrt.read();
            Some(abort_reason)
        } else {
            None
        }
    }

    fn read_internal(
        &mut self,
        buffer: &mut [u8],
        force_restart: bool,
        do_stop: bool,
    ) -> Result<(), Error> {
        let lastindex = buffer.len() - 1;
        for (i, byte) in buffer.iter_mut().enumerate() {
            let first = i == 0;
            let last = i == lastindex;

            // wait until there is space in the FIFO to write the next byte
            while self.tx_fifo_full() {}

            self.i2c.ic_data_cmd.write(|w| {
                if force_restart && first {
                    w.restart().enable();
                } else {
                    w.restart().disable();
                }

                if do_stop && last {
                    w.stop().enable();
                } else {
                    w.stop().disable();
                }

                w.cmd().read()
            });

            while self.i2c.ic_rxflr.read().bits() == 0 {
                if let Some(abort_reason) = self.read_and_clear_abort_reason() {
                    return Err(Error::Abort(abort_reason));
                }
            }

            *byte = self.i2c.ic_data_cmd.read().dat().bits();
        }

        Ok(())
    }

    fn write_internal(&mut self, bytes: &[u8], do_stop: bool) -> Result<(), Error> {
        for (i, byte) in bytes.iter().enumerate() {
            let last = i == bytes.len() - 1;

            self.i2c.ic_data_cmd.write(|w| {
                if do_stop && last {
                    w.stop().enable();
                } else {
                    w.stop().disable();
                }
                unsafe { w.dat().bits(*byte) }
            });

            // Wait until the transmission of the address/data from the internal
            // shift register has completed. For this to function correctly, the
            // TX_EMPTY_CTRL flag in IC_CON must be set. The TX_EMPTY_CTRL flag
            // was set in i2c_init.
            while self.i2c.ic_raw_intr_stat.read().tx_empty().is_inactive() {}

            let abort_reason = self.read_and_clear_abort_reason();

            if abort_reason.is_some() || (do_stop && last) {
                // If the transaction was aborted or if it completed
                // successfully wait until the STOP condition has occured.

                while self.i2c.ic_raw_intr_stat.read().stop_det().is_inactive() {}

                self.i2c.ic_clr_stop_det.read().clr_stop_det();
            }

            // Note the hardware issues a STOP automatically on an abort condition.
            // Note also the hardware clears RX FIFO as well as TX on abort,
            // ecause we set hwparam IC_AVOID_RX_FIFO_FLUSH_ON_TX_ABRT to 0.
            if let Some(abort_reason) = abort_reason {
                return Err(Error::Abort(abort_reason));
            }
        }
        Ok(())
    }

    /// Write to an I²C device on the bus.
    ///
    /// The address is given as a 7-bit value, right-aligned in a `u8`.
    pub fn write(&mut self, address: u8, bytes: &[u8]) -> Result<(), Error> {
        Self::validate(address, Some(bytes.is_empty()), None)?;
        self.setup(address);

        self.write_internal(bytes, true)
    }

    /// Read from an I²C device on the bus.
    ///
    /// The address is given as a 7-bit value, right-aligned in a `u8`.
    pub fn read(&mut self, address: u8, buffer: &mut [u8]) -> Result<(), Error> {
        Self::validate(address, None, Some(buffer.is_empty()))?;
        self.setup(address);

        self.read_internal(buffer, true, true)
    }

    /// Does a write to and then a read from an I²C device on the bus.
    pub fn write_read(&mut self, addr: u8, tx: &[u8], rx: &mut [u8]) -> Result<(), Error> {
        Self::validate(addr, Some(tx.is_empty()), Some(rx.is_empty()))?;
        self.setup(addr);

        self.write_internal(tx, false)?;
        self.read_internal(rx, true, true)
    }

    /// Writes bytes to slave with address `address`
    ///
    /// # I²C Events (contract)
    ///
    /// Same as the `write` method
    pub fn write_iter<B>(&mut self, address: u8, bytes: B) -> Result<(), Error>
    where
        B: IntoIterator<Item = u8>,
    {
        let mut peekable = bytes.into_iter().peekable();
        Self::validate(address, Some(peekable.peek().is_none()), None)?;
        self.setup(address);

        while let Some(tx) = peekable.next() {
            self.write_internal(&[tx], peekable.peek().is_none())?
        }
        Ok(())
    }

    /// Writes bytes to slave with address `address` and then reads enough bytes to fill `buffer` *in a
    /// single transaction*
    ///
    /// # I²C Events (contract)
    ///
    /// Same as the `write_read` method
    pub fn write_iter_read<B>(
        &mut self,
        address: u8,
        bytes: B,
        buffer: &mut [u8],
    ) -> Result<(), Error>
    where
        B: IntoIterator<Item = u8>,
    {
        let mut peekable = bytes.into_iter().peekable();
        Self::validate(address, Some(peekable.peek().is_none()), None)?;
        self.setup(address);

        for tx in peekable {
            self.write_internal(&[tx], false)?
        }
        self.read_internal(buffer, true, true)
    }

    /// Execute the provided operations on the I²C bus (iterator version).
    ///
    /// Transaction contract:
    /// - Before executing the first operation an ST is sent automatically. This is followed by SAD+R/W as appropriate.
    /// - Data from adjacent operations of the same type are sent after each other without an SP or SR.
    /// - Between adjacent operations of a different type an SR and SAD+R/W is sent.
    /// - After executing the last operation an SP is sent automatically.
    /// - If the last operation is a `Read` the master does not send an acknowledge for the last byte.
    ///
    /// - `ST` = start condition
    /// - `SAD+R/W` = slave address followed by bit 1 to indicate reading or 0 to indicate writing
    /// - `SR` = repeated start condition
    /// - `SP` = stop condition
    pub fn transaction_iter<'a, O>(&mut self, address: u8, operations: O) -> Result<(), Error>
    where
        O: IntoIterator<Item = i2c::Operation<'a>>,
    {
        self.setup(address);
        let mut peekable = operations.into_iter().peekable();
        while let Some(operation) = peekable.next() {
            let last = peekable.peek().is_none();
            match operation {
                i2c::Operation::Read(buf) => self.read_internal(buf, false, last)?,
                i2c::Operation::Write(buf) => self.write_internal(buf, last)?,
            }
        }
        Ok(())
    }
}

// ============================================================================
//
// Embedded HAL 0.2
//
// ============================================================================

impl<T: Deref<Target = Block>, PINS> i2c02::Read for I2C<T, PINS, Controller> {
    type Error = Error;

    fn read(&mut self, addr: u8, buffer: &mut [u8]) -> Result<(), Error> {
        // Defer to the inherent implementation
        Self::read(self, addr, buffer)
    }
}
impl<T: Deref<Target = Block>, PINS> i2c02::WriteRead for I2C<T, PINS, Controller> {
    type Error = Error;

    fn write_read(&mut self, addr: u8, tx: &[u8], rx: &mut [u8]) -> Result<(), Error> {
        Self::write_read(self, addr, tx, rx)
    }
}

impl<T: Deref<Target = Block>, PINS> i2c02::Write for I2C<T, PINS, Controller> {
    type Error = Error;

    fn write(&mut self, addr: u8, tx: &[u8]) -> Result<(), Error> {
        // Defer to the inherent implementation
        Self::write(self, addr, tx)
    }
}

impl<T: Deref<Target = Block>, PINS> i2c02::WriteIter for I2C<T, PINS, Controller> {
    type Error = Error;

    fn write<B>(&mut self, address: u8, bytes: B) -> Result<(), Self::Error>
    where
        B: IntoIterator<Item = u8>,
    {
        // Defer to the inherent implementation
        Self::write_iter(self, address, bytes)
    }
}

impl<T: Deref<Target = Block>, PINS> i2c02::WriteIterRead for I2C<T, PINS, Controller> {
    type Error = Error;

    fn write_iter_read<B>(
        &mut self,
        address: u8,
        bytes: B,
        buffer: &mut [u8],
    ) -> Result<(), Self::Error>
    where
        B: IntoIterator<Item = u8>,
    {
        // Defer to the inherent implementation
        Self::write_iter_read(self, address, bytes, buffer)
    }
}

// ============================================================================
//
// Embedded HAL 1.0
//
// ============================================================================

impl<T: Deref<Target = Block>, PINS> i2c::ErrorType for I2C<T, PINS, Controller> {
    type Error = Error;
}

impl<T: Deref<Target = Block>, PINS> i2c::I2c<i2c::SevenBitAddress> for I2C<T, PINS, Controller> {
    fn write(&mut self, addr: i2c::SevenBitAddress, bytes: &[u8]) -> Result<(), Self::Error> {
        Self::write(self, addr, bytes)
    }

    fn read(&mut self, addr: i2c::SevenBitAddress, buffer: &mut [u8]) -> Result<(), Error> {
        Self::read(self, addr, buffer)
    }

    fn transaction(
        &mut self,
        address: i2c::SevenBitAddress,
        operations: &mut [i2c::Operation<'_>],
    ) -> Result<(), Self::Error> {
        self.setup(address);
        for i in 0..operations.len() {
            let last = i == operations.len() - 1;
            match &mut operations[i] {
                i2c::Operation::Read(buf) => self.read_internal(buf, false, last)?,
                i2c::Operation::Write(buf) => self.write_internal(buf, last)?,
            }
        }
        Ok(())
    }
}
