//! 16550A UART, from `vm-superio`, with its interrupt wired to an IOAPIC pin.

use std::io::Write;

use vm_core::BusDevice;
use vm_superio::Trigger;

use crate::irq::IrqLine;

/// COM1's I/O base and IRQ.
pub const COM1_BASE: u16 = 0x3f8;
pub const COM1_IRQ: u8 = 4;
pub const SERIAL_PORTS: u16 = 8;

/// Pulses the UART's (edge-triggered) ISA interrupt line.
pub struct IrqTrigger(IrqLine);

impl Trigger for IrqTrigger {
    type E = std::convert::Infallible;

    fn trigger(&self) -> Result<(), Self::E> {
        self.0.pulse();
        Ok(())
    }
}

pub struct Serial {
    uart: vm_superio::Serial<IrqTrigger, vm_superio::serial::NoEvents, Box<dyn Write + Send>>,
}

impl Serial {
    /// A UART that writes the guest's output to `out` and interrupts on `irq`.
    pub fn new(irq: IrqLine, out: Box<dyn Write + Send>) -> Self {
        Self {
            uart: vm_superio::Serial::new(IrqTrigger(irq), out),
        }
    }

    /// Queues host input for the guest. Returns how many bytes fit; the rest should be
    /// offered again once the guest has read some.
    pub fn enqueue_input(&mut self, input: &[u8]) -> usize {
        match self.uart.enqueue_raw_bytes(input) {
            Ok(count) => count,
            Err(vm_superio::serial::Error::FullFifo) => 0,
            Err(e) => {
                tracing::warn!(%e, "serial input dropped");
                0
            }
        }
    }
}

impl BusDevice for Serial {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        data.fill(self.uart.read(offset as u8));
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        if let Err(e) = self.uart.write(offset as u8, data[0]) {
            tracing::warn!(%e, "serial output failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::irq::testing::RecordingSink;
    use crate::legacy::ioapic::IoApic;

    /// A writer the test can inspect afterwards.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    const IER: u64 = 1;
    const LSR: u64 = 5;
    const IER_RDA: u8 = 1;
    const LSR_DATA_READY: u8 = 1;

    fn serial() -> (Serial, Captured, Arc<RecordingSink>) {
        let sink = Arc::new(RecordingSink::default());
        let ioapic = Arc::new(Mutex::new(IoApic::new(0, sink.clone())));
        // Route COM1's pin edge-triggered to vector 0x24.
        {
            let mut ioapic = ioapic.lock().unwrap();
            ioapic.write(0, &(0x10u32 + 2 * u32::from(COM1_IRQ)).to_le_bytes());
            ioapic.write(0x10, &0x24u32.to_le_bytes());
        }
        let out = Captured::default();
        let serial = Serial::new(IrqLine::new(ioapic, COM1_IRQ), Box::new(out.clone()));
        (serial, out, sink)
    }

    #[test]
    fn output_reaches_the_writer() {
        let (mut serial, out, _) = serial();
        for byte in b"hi" {
            serial.write(0, &[*byte]);
        }
        assert_eq!(*out.0.lock().unwrap(), b"hi");
    }

    #[test]
    fn input_raises_the_interrupt_and_is_readable() {
        let (mut serial, _, sink) = serial();
        serial.write(IER, &[IER_RDA]);
        assert_eq!(serial.enqueue_input(b"ok"), 2);
        let fired = sink.take();
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].vector(), 0x24);

        let mut data = [0u8];
        serial.read(LSR, &mut data);
        assert_eq!(data[0] & LSR_DATA_READY, LSR_DATA_READY);
        serial.read(0, &mut data);
        assert_eq!(data[0], b'o');
    }

    #[test]
    fn input_beyond_the_fifo_is_pushed_back() {
        let (mut serial, _, _) = serial();
        let input = [b'x'; 100];
        let accepted = serial.enqueue_input(&input);
        assert_eq!(accepted, 64, "the 16550A FIFO holds 64 bytes in vm-superio");
        assert_eq!(
            serial.enqueue_input(&input),
            0,
            "full until the guest reads"
        );
    }
}
