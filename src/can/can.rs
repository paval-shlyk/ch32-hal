use super::enums::*;
use super::filter::{BitMode, FilterMode};
use super::{CanFilter, CanFrame};
use crate::can::registers::Registers;
use crate::can::util;
use crate::{into_ref, pac, peripherals, Peripheral, PeripheralRef, RccPeripheral, RemapPeripheral};

pub struct Can<'d, T: Instance> {
    _peri: PeripheralRef<'d, T>,
    fifo: CanFifo,
    last_mailbox_used: usize,
}

#[derive(Debug)]
pub enum CanInitError {
    InvalidTimings,
}

impl<'d, T: Instance> Can<'d, T> {
    /// Assumes AFIO & PORTB clocks have been enabled by HAL.
    ///
    /// CAN_RX is mapped to PB8, and CAN_TX is mapped to PB9.
    pub fn new<const REMAP: u8>(
        peri: impl Peripheral<P = T> + 'd,
        rx: impl Peripheral<P = impl RxPin<T, REMAP>> + 'd,
        tx: impl Peripheral<P = impl TxPin<T, REMAP>> + 'd,
        fifo: CanFifo,
        mode: CanMode,
        bitrate: u32,
    ) -> Result<Self, CanInitError> {
        into_ref!(peri, rx, tx);

        let this = Self {
            _peri: peri,
            fifo,
            last_mailbox_used: usize::MAX,
        };
        T::enable_and_reset(); // Enable CAN peripheral

        rx.set_mode_cnf(
            pac::gpio::vals::Mode::INPUT,
            pac::gpio::vals::Cnf::PULL_IN__AF_PUSH_PULL_OUT,
        );

        tx.set_mode_cnf(
            pac::gpio::vals::Mode::OUTPUT_50MHZ,
            pac::gpio::vals::Cnf::PULL_IN__AF_PUSH_PULL_OUT,
        );
        T::set_remap(REMAP);

        // //here should remap functionality be added
        // T::remap(0b10);

        Registers(T::regs()).enter_init_mode(); // CAN enter initialization mode

        // Configure bit timing parameters and CAN operating mode
        let Some(bit_timings) = util::calc_can_timings(T::frequency().0, bitrate) else {
            return Err(CanInitError::InvalidTimings);
        };

        Registers(T::regs()).set_bit_timing_and_mode(bit_timings, mode);

        Registers(T::regs()).leave_init_mode(); // Exit CAN initialization mode

        Ok(this)
    }

    /// Each filter bank consists of 2 32-bit registers CAN_FxR0 and CAN_FxR1
    pub fn add_filter<BIT: BitMode, MODE: FilterMode>(&self, filter: CanFilter<BIT, MODE>) {
        let can = T::regs();
        let fifo = &self.fifo;

        can.fctlr().modify(|w| w.set_finit(true)); // Enable filter init mode

        can.fscfgr()
            .modify(|w| w.set_fsc(filter.bank, filter.bit_mode.val_bool())); // Set filter scale config (32bit or 16bit mode)

        can.fr(filter.fr_id_value_reg())
            .write_value(crate::pac::can::regs::Fr(filter.id_value)); // Set filter's id value to match/mask

        can.fr(filter.fr_id_mask_reg())
            .write_value(crate::pac::can::regs::Fr(filter.id_mask)); // Set filter's id bits to mask

        can.fmcfgr().modify(|w| w.set_fbm(filter.bank, filter.mode.val_bool())); // Set new filter's operating mode

        can.fafifor()
            .modify(|w| w.set_ffa(filter.bank, fifo.val_bool())); // Associate CAN's FIFO to new filter

        can.fwr().modify(|w| w.set_fact(filter.bank, true)); // Activate new filter

        can.fctlr().modify(|w| w.set_finit(false)); // Exit filter init mode
    }

    /// Puts a frame in the transmit buffer to be sent on the bus.
    ///
    /// If the transmit buffer is full, this function will try to replace a pending
    /// lower priority frame and return the frame that was replaced.
    /// Returns `Err(WouldBlock)` if the transmit buffer is full and no frame can be
    /// replaced.
    pub fn transmit(&self, frame: &CanFrame) -> nb::Result<Option<CanFrame>, CanError> {
        let mailbox_num = match Registers(T::regs()).find_free_mailbox() {
            Some(n) => n,
            None => return Err(nb::Error::WouldBlock),
        };

        Registers(T::regs()).write_frame_mailbox(mailbox_num, frame);

        // Success in readying packet for transmit. No packets can be replaced in the
        // transmit buffer so return None in accordance with embedded-can.
        Ok(None)
    }

    /// Retrieves status of the last frame transmission
    pub fn transmit_status(&self) -> TxStatus {
        if self.last_mailbox_used > 2 {
            return TxStatus::OtherError;
        }

        Registers(T::regs()).transmit_status(self.last_mailbox_used)
    }

    /// Try to read the next message from the queue.
    /// If there are no messages, an error is returned.
    pub fn try_recv(&self) -> nb::Result<CanFrame, CanError> {
        let can = &T::regs();
        let fifo = self.fifo.val();

        //check pending messages
        if can.rfifo(self.fifo.val()).read().fmp() == 0 {
            return Err(nb::Error::WouldBlock);
        }

        let dlc = can.rxmdtr(fifo).read().dlc() as usize;
        let raw_id = can.rxmir(fifo).read().stid();

        let id = embedded_can::StandardId::new(raw_id).unwrap();

        let frame_data_unordered: u64 = ((can.rxmdhr(fifo).read().0 as u64) << 32) | can.rxmdlr(fifo).read().0 as u64;

        let frame = CanFrame::new_from_data_registers(id, frame_data_unordered, dlc);

        can.rfifo(fifo).write(|w| {
            //set the data was read
            w.set_rfom(true);
        });

        Ok(frame)
    }
}

/// These trait methods are only usable within the embedded_can context.
/// Under normal use of the [Can] instance,
impl<'d, T> embedded_can::nb::Can for Can<'d, T>
where
    T: Instance,
{
    type Frame = CanFrame;
    type Error = CanError;

    /// Puts a frame in the transmit buffer to be sent on the bus.
    ///
    /// If the transmit buffer is full, this function will try to replace a pending
    /// lower priority frame and return the frame that was replaced.
    /// Returns `Err(WouldBlock)` if the transmit buffer is full and no frame can be
    /// replaced.
    fn transmit(&mut self, frame: &Self::Frame) -> nb::Result<Option<Self::Frame>, Self::Error> {
        Can::transmit(self, frame)
    }

    /// Returns a received frame if available.
    fn receive(&mut self) -> nb::Result<Self::Frame, Self::Error> {
        Can::try_recv(self)
    }
}

pub trait SealedInstance: RccPeripheral + RemapPeripheral {
    fn regs() -> pac::can::Can;
    // Either `0b00`, `0b10` or `b11` on CAN1. `0` or `1` on CAN2.
    // fn remap(rm: u8) -> ();
}

pub trait Instance: SealedInstance + 'static {}

pin_trait!(RxPin, Instance);
pin_trait!(TxPin, Instance);

foreach_peripheral!(
    (can, $inst:ident) => {
        impl SealedInstance for peripherals::$inst {
            // FIXME: CH32L1 supports CANFD, and is compatible with the CAN peripheral.
            fn regs() -> crate::pac::can::Can {
                #[cfg(ch32l1)]
                return unsafe { crate::pac::can::Can::from_ptr(crate::pac::$inst.as_ptr()) };
                #[cfg(not(ch32l1))]
                return crate::pac::$inst;
            }
        }

        impl Instance for peripherals::$inst {
           // type Interrupt = crate::_generated::peripheral_interrupts::$inst::GLOBAL;
        }
    };
);
