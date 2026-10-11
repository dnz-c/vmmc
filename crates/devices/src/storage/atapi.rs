//! An ATAPI DVD-ROM drive: the SCSI/MMC packet commands that firmware, Windows and Linux
//! send to an optical drive, answered from a read-only image of 2048-byte sectors.
//!
//! Commands are plain functions of the CDB and the drive's state, so the AHCI port only
//! moves bytes. Errors are reported the SCSI way: a CHECK CONDITION with sense data that
//! the driver then fetches with REQUEST SENSE.

use std::sync::Arc;

use disk::BlockBackend;

pub const SECTOR_SIZE: u64 = 2048;
/// Discs larger than this report the DVD-ROM profile rather than CD-ROM.
const CD_MAX_SECTORS: u64 = 405_000;

// Sense keys.
pub const NOT_READY: u8 = 0x02;
pub const MEDIUM_ERROR: u8 = 0x03;
pub const ILLEGAL_REQUEST: u8 = 0x05;
pub const UNIT_ATTENTION: u8 = 0x06;

/// Sense key, additional sense code and qualifier.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sense {
    pub key: u8,
    pub asc: u8,
    pub ascq: u8,
}

impl Sense {
    const NONE: Self = Self {
        key: 0,
        asc: 0,
        ascq: 0,
    };
    const INVALID_OPCODE: Self = Self {
        key: ILLEGAL_REQUEST,
        asc: 0x20,
        ascq: 0,
    };
    const INVALID_FIELD: Self = Self {
        key: ILLEGAL_REQUEST,
        asc: 0x24,
        ascq: 0,
    };
    const LBA_OUT_OF_RANGE: Self = Self {
        key: ILLEGAL_REQUEST,
        asc: 0x21,
        ascq: 0,
    };
    const NO_MEDIUM: Self = Self {
        key: NOT_READY,
        asc: 0x3a,
        ascq: 0,
    };
    const READ_ERROR: Self = Self {
        key: MEDIUM_ERROR,
        asc: 0x11,
        ascq: 0,
    };
    const MEDIUM_CHANGED: Self = Self {
        key: UNIT_ATTENTION,
        asc: 0x28,
        ascq: 0,
    };
}

/// What a command produced.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Success, with this data for the host (possibly none).
    Data(Vec<u8>),
    /// CHECK CONDITION; the sense data is kept for REQUEST SENSE.
    Error(Sense),
}

/// An optical drive and its medium.
pub struct Cdrom {
    medium: Option<Arc<dyn BlockBackend>>,
    sense: Sense,
    /// A medium was inserted and the host hasn't been told yet.
    media_event: bool,
    /// The medium changed: the next command fails with a unit attention.
    unit_attention: bool,
    serial: String,
}

impl Cdrom {
    pub fn new(medium: Option<Arc<dyn BlockBackend>>, serial: &str) -> Self {
        Self {
            media_event: medium.is_some(),
            unit_attention: false,
            medium,
            sense: Sense::NONE,
            serial: serial.to_owned(),
        }
    }

    pub fn serial(&self) -> &str {
        &self.serial
    }

    fn sectors(&self) -> u64 {
        self.medium
            .as_ref()
            .map_or(0, |medium| medium.len() / SECTOR_SIZE)
    }

    fn is_dvd(&self) -> bool {
        self.sectors() > CD_MAX_SECTORS
    }

    /// Runs the packet command in `cdb`. `allocation` caps the data returned.
    pub fn command(&mut self, cdb: &[u8; 16]) -> Outcome {
        const TEST_UNIT_READY: u8 = 0x00;
        const REQUEST_SENSE: u8 = 0x03;
        const INQUIRY: u8 = 0x12;
        const START_STOP_UNIT: u8 = 0x1b;
        const PREVENT_ALLOW_REMOVAL: u8 = 0x1e;
        const READ_CAPACITY: u8 = 0x25;
        const READ_10: u8 = 0x28;
        const SEEK_10: u8 = 0x2b;
        const VERIFY_10: u8 = 0x2f;
        const SYNCHRONIZE_CACHE: u8 = 0x35;
        const READ_TOC: u8 = 0x43;
        const GET_CONFIGURATION: u8 = 0x46;
        const GET_EVENT_STATUS: u8 = 0x4a;
        const READ_DISC_INFORMATION: u8 = 0x51;
        const READ_TRACK_INFORMATION: u8 = 0x52;
        const MODE_SELECT_10: u8 = 0x55;
        const MODE_SENSE_10: u8 = 0x5a;
        const READ_12: u8 = 0xa8;
        const SET_CD_SPEED: u8 = 0xbb;
        const MECHANISM_STATUS: u8 = 0xbd;

        let opcode = cdb[0];
        // REQUEST SENSE reports, and so clears, the last error; anything else starts
        // afresh.
        if opcode == REQUEST_SENSE {
            let mut sense = std::mem::take(&mut self.sense);
            if sense == Sense::NONE && std::mem::take(&mut self.unit_attention) {
                sense = Sense::MEDIUM_CHANGED;
            }
            let mut data = vec![0u8; 18];
            data[0] = 0x70; // current error, fixed format
            data[2] = sense.key;
            data[7] = 10; // additional length
            data[12] = sense.asc;
            data[13] = sense.ascq;
            return Outcome::Data(truncate(data, usize::from(cdb[4])));
        }
        self.sense = Sense::NONE;
        // After a medium change, the first command (other than ones that only describe
        // the drive) reports it.
        tracing::trace!(opcode, "ATAPI command");
        if self.unit_attention && !matches!(opcode, INQUIRY | GET_CONFIGURATION | GET_EVENT_STATUS)
        {
            self.unit_attention = false;
            self.sense = Sense::MEDIUM_CHANGED;
            return Outcome::Error(Sense::MEDIUM_CHANGED);
        }
        let needs_medium = matches!(
            opcode,
            TEST_UNIT_READY
                | READ_CAPACITY
                | READ_10
                | READ_12
                | SEEK_10
                | VERIFY_10
                | READ_TOC
                | READ_DISC_INFORMATION
                | READ_TRACK_INFORMATION
        );
        let result = if needs_medium && self.medium.is_none() {
            Err(Sense::NO_MEDIUM)
        } else {
            match opcode {
                TEST_UNIT_READY
                | START_STOP_UNIT
                | PREVENT_ALLOW_REMOVAL
                | SEEK_10
                | VERIFY_10
                | SYNCHRONIZE_CACHE
                | MODE_SELECT_10
                | SET_CD_SPEED => Ok(Vec::new()),
                INQUIRY => self.inquiry(cdb),
                READ_CAPACITY => {
                    let last = self.sectors().saturating_sub(1).min(u64::from(u32::MAX));
                    let mut data = (last as u32).to_be_bytes().to_vec();
                    data.extend_from_slice(&(SECTOR_SIZE as u32).to_be_bytes());
                    Ok(data)
                }
                READ_10 => {
                    let lba = u32::from_be_bytes(cdb[2..6].try_into().unwrap());
                    let count = u16::from_be_bytes([cdb[7], cdb[8]]);
                    self.read(lba.into(), count.into())
                }
                READ_12 => {
                    let lba = u32::from_be_bytes(cdb[2..6].try_into().unwrap());
                    let count = u32::from_be_bytes(cdb[6..10].try_into().unwrap());
                    self.read(lba.into(), count.into())
                }
                READ_TOC => self.read_toc(cdb),
                GET_CONFIGURATION => Ok(self.configuration(cdb)),
                GET_EVENT_STATUS => self.event_status(cdb),
                READ_DISC_INFORMATION => Ok(self.disc_information()),
                READ_TRACK_INFORMATION => Ok(self.track_information()),
                MODE_SENSE_10 => self.mode_sense(cdb),
                // No changer: an 8-byte header with no slots.
                MECHANISM_STATUS => Ok(vec![0; 8]),
                _ => {
                    tracing::debug!(opcode, "ATAPI: unsupported command");
                    Err(Sense::INVALID_OPCODE)
                }
            }
        };
        match result {
            Ok(data) => Outcome::Data(truncate(data, allocation_length(cdb))),
            Err(sense) => {
                self.sense = sense;
                Outcome::Error(sense)
            }
        }
    }

    /// Takes the medium out (`None`) or puts one in. The host sees a unit attention.
    pub fn change_medium(&mut self, medium: Option<Arc<dyn BlockBackend>>) {
        self.media_event = medium.is_some();
        self.medium = medium;
        self.unit_attention = true;
    }

    fn inquiry(&self, cdb: &[u8; 16]) -> Result<Vec<u8>, Sense> {
        // Vital product data pages aren't offered.
        if cdb[1] & 1 != 0 {
            return Err(Sense::INVALID_FIELD);
        }
        let mut data = vec![0u8; 36];
        data[0] = 0x05; // CD/DVD device
        data[1] = 0x80; // removable
        data[2] = 0x05; // SPC-3
        data[3] = 0x32; // ATAPI-style response data format 2
        data[4] = 31; // additional length
        data[8..16].copy_from_slice(b"VMMC    ");
        data[16..32].copy_from_slice(b"vmmc DVD-ROM    ");
        data[32..36].copy_from_slice(b"1.0 ");
        Ok(data)
    }

    fn read(&self, lba: u64, count: u64) -> Result<Vec<u8>, Sense> {
        if lba + count > self.sectors() {
            return Err(Sense::LBA_OUT_OF_RANGE);
        }
        // The AHCI port caps transfers by the guest's PRDT; bound the buffer too.
        if count * SECTOR_SIZE > 32 << 20 {
            return Err(Sense::INVALID_FIELD);
        }
        let medium = self.medium.as_ref().ok_or(Sense::NO_MEDIUM)?;
        let mut data = vec![0u8; (count * SECTOR_SIZE) as usize];
        medium.read_at(&mut data, lba * SECTOR_SIZE).map_err(|e| {
            tracing::warn!(%e, "ATAPI: reading the medium failed");
            Sense::READ_ERROR
        })?;
        Ok(data)
    }

    fn read_toc(&self, cdb: &[u8; 16]) -> Result<Vec<u8>, Sense> {
        let msf = cdb[1] & 0x02 != 0;
        // The format lives in byte 2 (MMC), or in byte 9's top bits for old drivers.
        let format = match cdb[2] & 0x0f {
            0 => cdb[9] >> 6,
            format => format,
        };
        let address = |lba: u64| -> [u8; 4] {
            if msf {
                let frames = lba + 150;
                [
                    0,
                    (frames / (60 * 75)) as u8,
                    ((frames / 75) % 60) as u8,
                    (frames % 75) as u8,
                ]
            } else {
                (lba as u32).to_be_bytes()
            }
        };
        let mut data = vec![0, 0, 1, 1];
        match format {
            // TOC: track 1 (data) and the lead-out.
            0 => {
                let start = cdb[6];
                if start > 1 && start != 0xaa {
                    return Err(Sense::INVALID_FIELD);
                }
                if start <= 1 {
                    data.extend_from_slice(&[0, 0x14, 1, 0]);
                    data.extend_from_slice(&address(0));
                }
                data.extend_from_slice(&[0, 0x14, 0xaa, 0]);
                data.extend_from_slice(&address(self.sectors()));
            }
            // Session information: one session, starting at track 1.
            1 => {
                data.extend_from_slice(&[0, 0x14, 1, 0]);
                data.extend_from_slice(&address(0));
            }
            _ => return Err(Sense::INVALID_FIELD),
        }
        let len = (data.len() - 2) as u16;
        data[0..2].copy_from_slice(&len.to_be_bytes());
        Ok(data)
    }

    fn configuration(&self, cdb: &[u8; 16]) -> Vec<u8> {
        let request_type = cdb[1] & 0b11;
        let starting = u16::from_be_bytes([cdb[2], cdb[3]]);
        let has_medium = self.medium.is_some();
        let current_profile: u16 = match (has_medium, self.is_dvd()) {
            (false, _) => 0,
            (true, true) => 0x0010,
            (true, false) => 0x0008,
        };
        // (feature code, version, persistent, current, body)
        let profiles = {
            let mut body = Vec::new();
            for profile in [0x0010u16, 0x0008] {
                body.extend_from_slice(&profile.to_be_bytes());
                body.push(u8::from(profile == current_profile));
                body.push(0);
            }
            body
        };
        let random_readable = {
            let mut body = (SECTOR_SIZE as u32).to_be_bytes().to_vec();
            body.extend_from_slice(&(if self.is_dvd() { 16u16 } else { 1u16 }).to_be_bytes());
            body.extend_from_slice(&[0, 0]);
            body
        };
        let features: [(u16, u8, bool, bool, Vec<u8>); 7] = [
            (0x0000, 0, true, true, profiles),
            // Core: ATAPI interface, device busy events.
            (0x0001, 2, true, true, vec![0, 0, 0, 2, 1, 0, 0, 0]),
            // Morphing: operational change events.
            (0x0002, 1, true, true, vec![0x02, 0, 0, 0]),
            // Removable medium: tray, eject, lock.
            (0x0003, 0, true, true, vec![0x29, 0, 0, 0]),
            (0x0010, 0, false, has_medium, random_readable),
            (
                0x001e,
                2,
                false,
                has_medium && !self.is_dvd(),
                vec![0, 0, 0, 0],
            ),
            (
                0x001f,
                1,
                false,
                has_medium && self.is_dvd(),
                vec![0, 0, 0, 0],
            ),
        ];
        let mut data = vec![0u8; 8];
        data[6..8].copy_from_slice(&current_profile.to_be_bytes());
        for (code, version, persistent, current, body) in features {
            let wanted = match request_type {
                0 => code >= starting,
                1 => code >= starting && current,
                2 => code == starting,
                _ => false,
            };
            if !wanted {
                continue;
            }
            data.extend_from_slice(&code.to_be_bytes());
            data.push((version << 2) | (u8::from(persistent) << 1) | u8::from(current));
            data.push(body.len() as u8);
            data.extend_from_slice(&body);
        }
        let len = (data.len() - 4) as u32;
        data[0..4].copy_from_slice(&len.to_be_bytes());
        data
    }

    fn event_status(&mut self, cdb: &[u8; 16]) -> Result<Vec<u8>, Sense> {
        const MEDIA_CLASS: u8 = 1 << 4;
        // Only polled operation is supported.
        if cdb[1] & 1 == 0 {
            return Err(Sense::INVALID_FIELD);
        }
        if cdb[4] & MEDIA_CLASS == 0 {
            // No event available, and media events are what's supported.
            return Ok(vec![0, 2, 0x80, MEDIA_CLASS]);
        }
        let event = if std::mem::take(&mut self.media_event) {
            2 // new media
        } else {
            0 // no change
        };
        let present = u8::from(self.medium.is_some()) << 1;
        Ok(vec![0, 6, 4, MEDIA_CLASS, event, present, 0, 0])
    }

    fn disc_information(&self) -> Vec<u8> {
        let mut data = vec![0u8; 34];
        data[0..2].copy_from_slice(&32u16.to_be_bytes());
        data[2] = 0x0e; // last session complete, disc complete
        data[3] = 1; // first track
        data[4] = 1; // sessions
        data[5] = 1; // first track in last session
        data[6] = 1; // last track in last session
        data
    }

    fn track_information(&self) -> Vec<u8> {
        let mut data = vec![0u8; 36];
        data[0..2].copy_from_slice(&34u16.to_be_bytes());
        data[2] = 1; // track
        data[3] = 1; // session
        data[5] = 0x04; // data track, recorded uninterrupted
        data[6] = 0x01; // data mode 1
        data[24..28].copy_from_slice(&(self.sectors() as u32).to_be_bytes());
        data
    }

    fn mode_sense(&self, cdb: &[u8; 16]) -> Result<Vec<u8>, Sense> {
        const CAPABILITIES: u8 = 0x2a;
        const ALL_PAGES: u8 = 0x3f;
        let page = cdb[2] & 0x3f;
        if page != CAPABILITIES && page != ALL_PAGES {
            return Err(Sense::INVALID_FIELD);
        }
        // Header, then the CD/DVD capabilities and mechanical status page.
        let mut data = vec![0u8; 8];
        let mut capabilities = vec![0u8; 22];
        capabilities[0] = CAPABILITIES;
        capabilities[1] = 20;
        capabilities[2] = 0x3b; // reads CD-R, CD-RW, method 2, DVD-ROM, DVD-R
        capabilities[4] = 0x71; // audio play, composite, ports, multi-session
        capabilities[5] = 0xff; // CD-DA, accurate stream, and the rest
        capabilities[6] = 0x29; // tray, eject, lock
        capabilities[8..10].copy_from_slice(&5_540u16.to_be_bytes()); // 32x, KB/s
        capabilities[10..12].copy_from_slice(&256u16.to_be_bytes()); // volume levels
        capabilities[12..14].copy_from_slice(&256u16.to_be_bytes()); // buffer, KB
        capabilities[14..16].copy_from_slice(&5_540u16.to_be_bytes());
        data.extend_from_slice(&capabilities);
        let len = (data.len() - 2) as u16;
        data[0..2].copy_from_slice(&len.to_be_bytes());
        Ok(data)
    }
}

/// The CDB's allocation length: how much the host has room for.
fn allocation_length(cdb: &[u8; 16]) -> usize {
    match cdb[0] {
        // 6-byte CDBs: one byte; 0 means 256 for INQUIRY.
        0x12 => match cdb[4] {
            0 => 256,
            n => n.into(),
        },
        // READ(10) and READ(12) are sized by the data itself.
        0x28 | 0xa8 => usize::MAX,
        0x25 => 8,
        // 10- and 12-byte CDBs with a 16-bit length in bytes 7-8.
        0x43 | 0x46 | 0x4a | 0x51 | 0x52 | 0x5a => u16::from_be_bytes([cdb[7], cdb[8]]).into(),
        0xbd => u16::from_be_bytes([cdb[8], cdb[9]]).into(),
        _ => usize::MAX,
    }
}

fn truncate(mut data: Vec<u8>, len: usize) -> Vec<u8> {
    data.truncate(len);
    data
}

#[cfg(test)]
mod tests {
    use disk::MemDisk;

    use super::*;

    fn cdrom(sectors: usize) -> Cdrom {
        let medium = MemDisk::new(sectors * SECTOR_SIZE as usize, true);
        medium
            .write_at(b"VMMC-ISO", 16 * SECTOR_SIZE)
            .expect("writable in tests");
        Cdrom::new(Some(Arc::new(medium)), "VMMCCD0")
    }

    fn cdb(bytes: &[u8]) -> [u8; 16] {
        let mut cdb = [0u8; 16];
        cdb[..bytes.len()].copy_from_slice(bytes);
        cdb
    }

    fn data(outcome: Outcome) -> Vec<u8> {
        match outcome {
            Outcome::Data(data) => data,
            Outcome::Error(sense) => panic!("check condition: {sense:?}"),
        }
    }

    #[test]
    fn identifies_as_a_removable_dvd_drive() {
        let mut cd = cdrom(32);
        let inquiry = data(cd.command(&cdb(&[0x12, 0, 0, 0, 36])));
        assert_eq!(inquiry.len(), 36);
        assert_eq!(inquiry[0], 0x05);
        assert_eq!(inquiry[1], 0x80);
        assert_eq!(&inquiry[16..28], b"vmmc DVD-ROM");
        assert_eq!(
            data(cd.command(&cdb(&[0x12, 0, 0, 0, 5]))).len(),
            5,
            "truncated"
        );
    }

    #[test]
    fn capacity_and_reads_use_2048_byte_sectors() {
        let mut cd = cdrom(32);
        let capacity = data(cd.command(&cdb(&[0x25])));
        assert_eq!(capacity, [0, 0, 0, 31, 0, 0, 8, 0]);
        let sector = data(cd.command(&cdb(&[0x28, 0, 0, 0, 0, 16, 0, 0, 1])));
        assert_eq!(sector.len(), 2048);
        assert_eq!(&sector[..8], b"VMMC-ISO");
        let twelve = data(cd.command(&cdb(&[0xa8, 0, 0, 0, 0, 16, 0, 0, 0, 2])));
        assert_eq!(twelve.len(), 4096);
    }

    #[test]
    fn errors_leave_sense_data_for_request_sense() {
        let mut cd = cdrom(32);
        assert_eq!(
            cd.command(&cdb(&[0x28, 0, 0, 0, 0, 31, 0, 0, 2])),
            Outcome::Error(Sense::LBA_OUT_OF_RANGE)
        );
        let sense = data(cd.command(&cdb(&[0x03, 0, 0, 0, 18])));
        assert_eq!(
            (sense[0], sense[2], sense[12]),
            (0x70, ILLEGAL_REQUEST, 0x21)
        );
        let cleared = data(cd.command(&cdb(&[0x03, 0, 0, 0, 18])));
        assert_eq!(cleared[2], 0, "sense is reported once");
        assert_eq!(
            cd.command(&cdb(&[0xff])),
            Outcome::Error(Sense::INVALID_OPCODE)
        );
    }

    #[test]
    fn an_empty_drive_is_not_ready() {
        let mut cd = Cdrom::new(None, "VMMCCD0");
        assert_eq!(cd.command(&cdb(&[0x00])), Outcome::Error(Sense::NO_MEDIUM));
        assert_eq!(data(cd.command(&cdb(&[0x12, 0, 0, 0, 36])))[0], 0x05);
        let events = data(cd.command(&cdb(&[0x4a, 1, 0, 0, 0x10, 0, 0, 0, 8])));
        assert_eq!(events[5] & 2, 0, "no medium present");
    }

    #[test]
    fn toc_lists_one_data_track_and_the_lead_out() {
        let mut cd = cdrom(300);
        let toc = data(cd.command(&cdb(&[0x43, 0, 0, 0, 0, 0, 0, 0, 100])));
        assert_eq!(&toc[0..4], &[0, 18, 1, 1]);
        assert_eq!(&toc[4..12], &[0, 0x14, 1, 0, 0, 0, 0, 0]);
        assert_eq!(&toc[12..20], &[0, 0x14, 0xaa, 0, 0, 0, 1, 44]);
        // MSF: 300 + 150 frames is 0:06:00.
        let msf = data(cd.command(&cdb(&[0x43, 2, 0, 0, 0, 0, 0, 0, 100])));
        assert_eq!(&msf[16..20], &[0, 0, 6, 0]);
        let sessions = data(cd.command(&cdb(&[0x43, 0, 1, 0, 0, 0, 0, 0, 12])));
        assert_eq!(sessions.len(), 12);
    }

    #[test]
    fn configuration_reports_the_profile_for_the_disc_size() {
        let mut cd = cdrom(32);
        let all = data(cd.command(&cdb(&[0x46, 0, 0, 0, 0, 0, 0, 1, 0])));
        assert_eq!(u16::from_be_bytes([all[6], all[7]]), 0x0008, "CD-ROM");
        assert_eq!(
            u32::from_be_bytes(all[0..4].try_into().unwrap()) as usize,
            all.len() - 4
        );
        // Just the core feature.
        let core = data(cd.command(&cdb(&[0x46, 2, 0, 1, 0, 0, 0, 1, 0])));
        assert_eq!(core.len(), 8 + 4 + 8);
        assert_eq!(&core[8..10], &[0, 1]);

        let mut dvd = cdrom(CD_MAX_SECTORS as usize + 1);
        let all = data(dvd.command(&cdb(&[0x46, 1, 0, 0, 0, 0, 0, 1, 0])));
        assert_eq!(u16::from_be_bytes([all[6], all[7]]), 0x0010, "DVD-ROM");
    }

    #[test]
    fn media_events_report_new_media_once() {
        let mut cd = cdrom(32);
        let first = data(cd.command(&cdb(&[0x4a, 1, 0, 0, 0x10, 0, 0, 0, 8])));
        assert_eq!((first[4], first[5]), (2, 2), "new media, present");
        let second = data(cd.command(&cdb(&[0x4a, 1, 0, 0, 0x10, 0, 0, 0, 8])));
        assert_eq!(second[4], 0, "no change");
        assert_eq!(
            cd.command(&cdb(&[0x4a, 0, 0, 0, 0x10, 0, 0, 0, 8])),
            Outcome::Error(Sense::INVALID_FIELD),
            "asynchronous mode isn't supported"
        );
    }

    #[test]
    fn mode_sense_offers_the_capabilities_page() {
        let mut cd = cdrom(32);
        let page = data(cd.command(&cdb(&[0x5a, 0, 0x2a, 0, 0, 0, 0, 0, 255])));
        assert_eq!(
            u16::from_be_bytes([page[0], page[1]]) as usize,
            page.len() - 2
        );
        assert_eq!(page[8], 0x2a);
        assert_eq!(
            cd.command(&cdb(&[0x5a, 0, 0x01, 0, 0, 0, 0, 0, 255])),
            Outcome::Error(Sense::INVALID_FIELD)
        );
    }

    #[test]
    fn changing_the_medium_raises_a_unit_attention() {
        let mut cd = cdrom(32);
        cd.change_medium(None);
        assert_eq!(
            data(cd.command(&cdb(&[0x12, 0, 0, 0, 36])))[0],
            5,
            "INQUIRY still works"
        );
        assert_eq!(
            cd.command(&cdb(&[0x00])),
            Outcome::Error(Sense::MEDIUM_CHANGED)
        );
        let sense = data(cd.command(&cdb(&[0x03, 0, 0, 0, 18])));
        assert_eq!((sense[2], sense[12]), (UNIT_ATTENTION, 0x28));
        assert_eq!(cd.command(&cdb(&[0x00])), Outcome::Error(Sense::NO_MEDIUM));
    }
}
