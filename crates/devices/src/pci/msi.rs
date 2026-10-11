//! The plain MSI capability: one message, 64-bit address, no per-vector masking.

use super::ConfigSpace;
use crate::irq::MsiMessage;

pub const CAPABILITY_ID: u8 = 0x05;

const CONTROL_ENABLE: u16 = 1 << 0;
const CONTROL_64BIT: u16 = 1 << 7;

/// Adds the capability (disabled). Returns its offset.
pub fn add_capability(config: &mut ConfigSpace) -> u16 {
    // Control, address low, address high, data, padding.
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&CONTROL_64BIT.to_le_bytes());
    body.extend_from_slice(&[0; 10]);
    let offset = config.add_capability(CAPABILITY_ID, &body);
    config.set_wmask_u16(offset + 2, CONTROL_ENABLE);
    config.set_wmask_u32(offset + 4, !0b11);
    config.set_wmask_u32(offset + 8, u32::MAX);
    config.set_wmask_u16(offset + 12, u16::MAX);
    offset
}

/// The message the capability at `offset` sends, if MSI is enabled.
pub fn message(config: &ConfigSpace, offset: u16) -> Option<MsiMessage> {
    if config.u16_at(offset + 2) & CONTROL_ENABLE == 0 {
        return None;
    }
    Some(MsiMessage {
        address: u64::from(config.u32_at(offset + 4))
            | (u64::from(config.u32_at(offset + 8)) << 32),
        data: u32::from(config.u16_at(offset + 12)),
    })
}

/// Whether a config write at `offset` with `len` bytes touched the capability.
pub fn touched(capability: u16, offset: u16, len: usize) -> bool {
    let end = usize::from(offset) + len;
    usize::from(capability) < end && usize::from(offset) < usize::from(capability) + 14
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pci::Identity;

    #[test]
    fn programs_and_enables_a_message() {
        let mut config = ConfigSpace::new(Identity {
            vendor_id: 0x8086,
            device_id: 0x2922,
            revision: 0,
            class: 0x01_06_01,
            subsystem_vendor_id: 0,
            subsystem_id: 0,
        });
        let offset = add_capability(&mut config);
        assert_eq!(config.u8_at(offset), CAPABILITY_ID);
        assert!(message(&config, offset).is_none(), "off at reset");
        config.write(offset + 4, &0xfee0_1004u32.to_le_bytes());
        config.write(offset + 12, &0x4041u16.to_le_bytes());
        config.write(offset + 2, &0xffffu16.to_le_bytes());
        let message = message(&config, offset).unwrap();
        assert_eq!(message.address, 0xfee0_1004);
        assert_eq!(message.data, 0x4041);
        assert_eq!(
            config.u16_at(offset + 2),
            CONTROL_64BIT | CONTROL_ENABLE,
            "one message only"
        );
        assert!(touched(offset, offset + 12, 2));
        assert!(!touched(offset, offset + 14, 2));
    }
}
