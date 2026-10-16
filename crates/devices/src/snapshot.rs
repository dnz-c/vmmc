//! Saving and restoring device state, for snapshots and suspend.
//!
//! A device saves what the guest can see of it, as a serde struct encoded with
//! postcard, and restores it onto a device the machine built with the same
//! configuration: what the configuration gives (disk images, wiring, host handles) isn't
//! saved. A state's layout belongs to the vmmc version that wrote it; the machine's
//! state file carries a format version, which goes up whenever a device's state changes.

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Saved state that doesn't fit the device it's restored onto.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct StateError(pub String);

/// A device whose state can be saved and restored.
pub trait Snapshot: Send {
    /// The device's state.
    fn save(&self) -> Vec<u8>;

    /// Puts back what [`Snapshot::save`] saved, on a device built with the same
    /// configuration, before the guest runs.
    fn restore(&mut self, state: &[u8]) -> Result<(), StateError>;

    /// Waits until the device has nothing in flight (a command half done, data on its
    /// way into guest memory) and takes on no new work from the host until
    /// [`Snapshot::resume`]. The machine calls it with the vCPUs paused, before saving.
    fn quiesce(&mut self) {}

    /// Takes on work again after [`Snapshot::quiesce`].
    fn resume(&mut self) {}
}

/// `state`, encoded.
pub fn encode<T: Serialize>(state: &T) -> Vec<u8> {
    postcard::to_allocvec(state).expect("device state is plain data")
}

/// A state [`encode`] encoded.
pub fn decode<T: DeserializeOwned>(state: &[u8]) -> Result<T, StateError> {
    postcard::from_bytes(state)
        .map_err(|e| StateError(format!("the saved state doesn't fit this device: {e}")))
}

#[cfg(test)]
pub(crate) mod testing {
    use super::Snapshot;

    /// Saves `device`, restores the state onto `fresh`, and checks it saves the same.
    pub fn round_trip(device: &impl Snapshot, fresh: &mut impl Snapshot) {
        let state = device.save();
        fresh.restore(&state).expect("restores");
        assert_eq!(fresh.save(), state, "saves the state it was restored from");
    }
}
