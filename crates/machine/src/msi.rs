//! Delivers device MSI messages as hypervisor interrupts.

use devices::irq::{MsiMessage, MsiSink};
use hv::{Delivery, Interrupt, InterruptController};

/// The MSI address window that reaches local APICs.
const MSI_WINDOW: u64 = 0xfee0_0000;
const MSI_WINDOW_MASK: u64 = 0xfff0_0000;

/// An [`MsiSink`] that injects through the partition's interrupt controller.
pub struct HvMsiSink<I> {
    interrupts: I,
}

impl<I: InterruptController> HvMsiSink<I> {
    pub fn new(interrupts: I) -> Self {
        Self { interrupts }
    }
}

/// What an MSI message asks the local APICs to do, or `None` if it isn't something
/// this platform delivers.
pub fn decode(message: MsiMessage) -> Option<Interrupt> {
    if message.address & MSI_WINDOW_MASK != MSI_WINDOW {
        return None;
    }
    let delivery = match message.delivery_mode() {
        0 => Delivery::Fixed,
        1 => Delivery::LowestPriority,
        4 => Delivery::Nmi,
        5 => Delivery::Init,
        _ => return None,
    };
    Some(Interrupt {
        vector: message.vector(),
        delivery,
        destination: message.destination().into(),
        logical: message.logical(),
        level_triggered: message.level_triggered(),
    })
}

impl<I: InterruptController> MsiSink for HvMsiSink<I> {
    fn send(&self, message: MsiMessage) {
        let Some(interrupt) = decode(message) else {
            tracing::debug!(?message, "dropped an MSI this platform can't deliver");
            return;
        };
        if let Err(e) = self.interrupts.request(&interrupt) {
            tracing::warn!(%e, ?interrupt, "interrupt injection failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_fixed_and_level_interrupts() {
        assert_eq!(
            decode(MsiMessage {
                address: 0xfee0_3000,
                data: 0x41
            }),
            Some(Interrupt {
                vector: 0x41,
                delivery: Delivery::Fixed,
                destination: 3,
                logical: false,
                level_triggered: false,
            })
        );
        let level = decode(MsiMessage {
            address: 0xfee0_0004,
            data: 0xc129,
        })
        .unwrap();
        assert_eq!(level.delivery, Delivery::LowestPriority);
        assert!(level.logical && level.level_triggered);
    }

    #[test]
    fn drops_writes_outside_the_apic_window_and_extint() {
        assert_eq!(
            decode(MsiMessage {
                address: 0x1000,
                data: 0x30
            }),
            None
        );
        assert_eq!(
            decode(MsiMessage {
                address: 0xfee0_0000,
                data: 0x730
            }),
            None,
            "ExtINT"
        );
    }
}
