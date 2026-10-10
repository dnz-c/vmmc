use std::sync::Weak;

use hv::{Delivery, Interrupt};
use windows::Win32::System::Hypervisor::{
    WHV_INTERRUPT_CONTROL, WHvX64InterruptDestinationModeLogical,
    WHvX64InterruptDestinationModePhysical, WHvX64InterruptTriggerModeEdge,
    WHvX64InterruptTriggerModeLevel, WHvX64InterruptTypeFixed, WHvX64InterruptTypeInit,
    WHvX64InterruptTypeLowestPriority, WHvX64InterruptTypeNmi, WHvX64InterruptTypeSipi,
};

use crate::api::check;
use crate::partition::PartitionInner;

/// Injects interrupts into a WHP partition from any thread.
#[derive(Debug, Clone)]
pub struct WhpInterrupts {
    partition: Weak<PartitionInner>,
}

impl WhpInterrupts {
    pub(crate) fn new(partition: Weak<PartitionInner>) -> Self {
        Self { partition }
    }
}

impl hv::InterruptController for WhpInterrupts {
    fn request(&self, interrupt: &Interrupt) -> hv::Result<()> {
        let Some(partition) = self.partition.upgrade() else {
            return Ok(());
        };
        let control = interrupt_control(interrupt);
        // SAFETY: `control` is a valid WHV_INTERRUPT_CONTROL of the size passed.
        check("WHvRequestInterrupt", unsafe {
            (partition.api.WHvRequestInterrupt)(
                partition.handle,
                &control,
                size_of::<WHV_INTERRUPT_CONTROL>() as u32,
            )
        })
    }
}

/// Packs an interrupt into WHP's control block. The first 64 bits are bitfields: Type
/// `[7:0]`, DestinationMode `[11:8]`, TriggerMode `[15:12]`.
fn interrupt_control(interrupt: &Interrupt) -> WHV_INTERRUPT_CONTROL {
    let kind = match interrupt.delivery {
        Delivery::Fixed => WHvX64InterruptTypeFixed,
        Delivery::LowestPriority => WHvX64InterruptTypeLowestPriority,
        Delivery::Nmi => WHvX64InterruptTypeNmi,
        Delivery::Init => WHvX64InterruptTypeInit,
        Delivery::Sipi => WHvX64InterruptTypeSipi,
    };
    let mode = if interrupt.logical {
        WHvX64InterruptDestinationModeLogical
    } else {
        WHvX64InterruptDestinationModePhysical
    };
    let trigger = if interrupt.level_triggered {
        WHvX64InterruptTriggerModeLevel
    } else {
        WHvX64InterruptTriggerModeEdge
    };
    WHV_INTERRUPT_CONTROL {
        _bitfield: (kind.0 as u64 & 0xff)
            | ((mode.0 as u64 & 0xf) << 8)
            | ((trigger.0 as u64 & 0xf) << 12),
        Destination: interrupt.destination,
        Vector: interrupt.vector.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_type_destination_mode_and_trigger_mode() {
        let fixed_edge = interrupt_control(&Interrupt {
            vector: 0x30,
            delivery: Delivery::Fixed,
            destination: 3,
            logical: false,
            level_triggered: false,
        });
        assert_eq!(fixed_edge._bitfield, 0);
        assert_eq!((fixed_edge.Destination, fixed_edge.Vector), (3, 0x30));

        let lowest_logical_level = interrupt_control(&Interrupt {
            vector: 0x41,
            delivery: Delivery::LowestPriority,
            destination: 0xff,
            logical: true,
            level_triggered: true,
        });
        assert_eq!(lowest_logical_level._bitfield, 0x1101);

        let sipi = interrupt_control(&Interrupt {
            vector: 0x9a,
            delivery: Delivery::Sipi,
            destination: 1,
            logical: false,
            level_triggered: false,
        });
        assert_eq!(sipi._bitfield, 6);
    }
}
