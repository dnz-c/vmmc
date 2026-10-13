//! What a TPM vendor does at the factory: an endorsement key (EK) certificate in NV, so
//! the guest can tell its TPM is a TPM. Windows won't call a TPM ready for attestation
//! without one (and `Get-Tpm` fails).
//!
//! - **The EK** is TCG's default RSA 2048 key (EK Credential Profile, template L-1).
//!   It derives from the endorsement seed, so the guest recreates the same key.
//! - **The certificate** follows the EK Credential Profile: no subject, a critical
//!   alternative name with the TPM's manufacturer, model and version, key
//!   encipherment, and the EK certificate key purpose.
//! - **The issuer** is an ECDSA P-256 key the TPM derives from its platform seed and
//!   signs with, so each TPM has its own. A vmmc CA that guests can check comes later.
//! - **Storage:** NV index 0x01C00002 (TCG's RSA EK certificate index), defined with
//!   platform authorization and locked after writing.
//!
//! Commands go through a closure, so this runs against the TPM before any firmware.

use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

/// TCG's NV index for the RSA 2048 EK certificate.
pub const EK_CERTIFICATE_INDEX: u32 = 0x01c0_0002;

const ST_NO_SESSIONS: u16 = 0x8001;
pub(crate) const ST_SESSIONS: u16 = 0x8002;
const ST_HASHCHECK: u16 = 0x8024;
const RS_PW: u32 = 0x4000_0009;
const RH_NULL: u32 = 0x4000_0007;
pub(crate) const RH_ENDORSEMENT: u32 = 0x4000_000b;
const RH_PLATFORM: u32 = 0x4000_000c;

const CC_NV_DEFINE_SPACE: u32 = 0x12a;
const CC_CREATE_PRIMARY: u32 = 0x131;
const CC_NV_WRITE: u32 = 0x137;
const CC_NV_WRITE_LOCK: u32 = 0x138;
const CC_STARTUP: u32 = 0x144;
const CC_SHUTDOWN: u32 = 0x145;
const CC_SIGN: u32 = 0x15d;
const CC_FLUSH_CONTEXT: u32 = 0x165;
const CC_GET_CAPABILITY: u32 = 0x17a;

const ALG_RSA: u16 = 0x0001;
const ALG_AES: u16 = 0x0006;
const ALG_SHA256: u16 = 0x000b;
const ALG_NULL: u16 = 0x0010;
const ALG_ECDSA: u16 = 0x0018;
const ALG_ECC: u16 = 0x0023;
const ALG_CFB: u16 = 0x0043;
const ECC_NIST_P256: u16 = 0x0003;

const CAP_TPM_PROPERTIES: u32 = 6;
const PT_MANUFACTURER: u32 = 0x105;

/// TPMA_OBJECT bits.
const FIXED_TPM: u32 = 1 << 1;
const FIXED_PARENT: u32 = 1 << 4;
const SENSITIVE_DATA_ORIGIN: u32 = 1 << 5;
const USER_WITH_AUTH: u32 = 1 << 6;
const ADMIN_WITH_POLICY: u32 = 1 << 7;
const RESTRICTED: u32 = 1 << 16;
const DECRYPT: u32 = 1 << 17;
const SIGN: u32 = 1 << 18;

/// The EK template's policy: PolicySecret(TPM_RH_ENDORSEMENT).
const EK_POLICY: [u8; 32] = [
    0x83, 0x71, 0x97, 0x67, 0x44, 0x84, 0xb3, 0xf8, 0x1a, 0x90, 0xcc, 0x8d, 0x46, 0xa5, 0xd7, 0x24,
    0xfd, 0x52, 0xd7, 0x6e, 0x06, 0x52, 0x0b, 0x64, 0xf2, 0xa1, 0xda, 0x1b, 0x33, 0x14, 0x69, 0xaa,
];

/// The EK certificate index's attributes, as the PC Client platform profile has them:
/// written by the platform once (PPWRITE, WRITEDEFINE, PLATFORMCREATE), readable by
/// the platform, the owner and with its own (empty) auth, and no dictionary-attack
/// lockout.
const EK_CERTIFICATE_ATTRIBUTES: u32 =
    1 | (1 << 13) | (1 << 16) | (1 << 17) | (1 << 18) | (1 << 25) | (1 << 30);
/// The largest NV write the reference implementation takes (MAX_NV_BUFFER_SIZE).
const NV_CHUNK: usize = 1024;
/// How often a command the TPM asks to have sent again is sent.
const MAX_TRIES: u32 = 32;

/// A TPM 2.0 command with the standard header, built field by field.
pub(crate) struct Command {
    bytes: Vec<u8>,
}

impl Command {
    pub(crate) fn new(tag: u16, code: u32) -> Self {
        let mut bytes = Vec::with_capacity(512);
        bytes.extend_from_slice(&tag.to_be_bytes());
        bytes.extend_from_slice(&[0; 4]);
        bytes.extend_from_slice(&code.to_be_bytes());
        Self { bytes }
    }

    fn u8(mut self, value: u8) -> Self {
        self.bytes.push(value);
        self
    }

    pub(crate) fn u16(mut self, value: u16) -> Self {
        self.bytes.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub(crate) fn u32(mut self, value: u32) -> Self {
        self.bytes.extend_from_slice(&value.to_be_bytes());
        self
    }

    /// A TPM2B: a 16-bit size, then the bytes.
    fn sized(self, value: &[u8]) -> Self {
        let mut command = self.u16(value.len() as u16);
        command.bytes.extend_from_slice(value);
        command
    }

    /// The authorization area: one password session with an empty password.
    pub(crate) fn password(self) -> Self {
        self.u32(9).u32(RS_PW).u16(0).u8(0).u16(0)
    }

    pub(crate) fn finish(mut self) -> Vec<u8> {
        let size = self.bytes.len() as u32;
        self.bytes[2..6].copy_from_slice(&size.to_be_bytes());
        self.bytes
    }
}

/// Reads a response's fields in order.
struct Response<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Response<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], String> {
        let field = self
            .bytes
            .get(self.at..self.at + len)
            .ok_or("a TPM response ended early")?;
        self.at += len;
        Ok(field)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn sized(&mut self) -> Result<&'a [u8], String> {
        let len = self.u16()?;
        self.take(usize::from(len))
    }
}

/// Runs `command`; returns the response after its header, or the response code.
fn run<'r>(
    tpm: &mut dyn FnMut(&[u8]) -> Vec<u8>,
    what: &str,
    command: Command,
    response: &'r mut Vec<u8>,
) -> Result<Response<'r>, String> {
    let command = command.finish();
    let mut tries = 0;
    let code = loop {
        *response = tpm(&command);
        let code = response.get(6..10).map_or(u32::MAX, |code| {
            u32::from_be_bytes(code.try_into().unwrap())
        });
        tries += 1;
        // TPM_RC_YIELDED, TPM_RC_TESTING and TPM_RC_RETRY ask for the command again.
        if !matches!(code, 0x908 | 0x90a | 0x922) || tries == MAX_TRIES {
            break code;
        }
    };
    if code != 0 {
        return Err(format!("{what} failed (TPM response code {code:#x})"));
    }
    Ok(Response {
        bytes: response,
        at: 10,
    })
}

/// TCG's EK template L-1: RSA 2048, AES-128-CFB, a restricted decryption key that
/// only the endorsement hierarchy's policy can use.
pub(crate) fn ek_template() -> Vec<u8> {
    Command { bytes: Vec::new() }
        .u16(ALG_RSA)
        .u16(ALG_SHA256)
        .u32(
            FIXED_TPM
                | FIXED_PARENT
                | SENSITIVE_DATA_ORIGIN
                | ADMIN_WITH_POLICY
                | RESTRICTED
                | DECRYPT,
        )
        .sized(&EK_POLICY)
        .u16(ALG_AES)
        .u16(128)
        .u16(ALG_CFB)
        .u16(ALG_NULL)
        .u16(2048)
        .u32(0)
        .sized(&[0; 256])
        .bytes
}

/// The issuer: an unrestricted ECDSA P-256 signing key.
fn issuer_template() -> Vec<u8> {
    Command { bytes: Vec::new() }
        .u16(ALG_ECC)
        .u16(ALG_SHA256)
        .u32(FIXED_TPM | FIXED_PARENT | SENSITIVE_DATA_ORIGIN | USER_WITH_AUTH | SIGN)
        .u16(0)
        .u16(ALG_NULL)
        .u16(ALG_ECDSA)
        .u16(ALG_SHA256)
        .u16(ECC_NIST_P256)
        .u16(ALG_NULL)
        .u16(0)
        .u16(0)
        .bytes
}

/// Creates a primary key under `hierarchy`; returns its handle and the public area's
/// `unique` fields (the RSA modulus, or the ECC point's x and y).
pub(crate) fn create_primary(
    tpm: &mut dyn FnMut(&[u8]) -> Vec<u8>,
    hierarchy: u32,
    template: &[u8],
) -> Result<(u32, Vec<Vec<u8>>), String> {
    let command = Command::new(ST_SESSIONS, CC_CREATE_PRIMARY)
        .u32(hierarchy)
        .password()
        .sized(&[0, 0, 0, 0])
        .sized(template)
        .sized(&[])
        .u32(0);
    let mut buffer = Vec::new();
    let mut response = run(tpm, "TPM2_CreatePrimary", command, &mut buffer)?;
    let handle = response.u32()?;
    let _parameter_size = response.u32()?;
    let public = response.sized()?;
    let mut public = Response {
        bytes: public,
        at: 0,
    };
    let kind = public.u16()?;
    let _name_alg = public.u16()?;
    let _attributes = public.u32()?;
    public.sized()?;
    let symmetric = public.u16()?;
    if symmetric != ALG_NULL {
        public.u16()?;
        public.u16()?;
    }
    let scheme = public.u16()?;
    if scheme != ALG_NULL {
        public.u16()?;
    }
    let unique = match kind {
        ALG_RSA => {
            public.u16()?;
            public.u32()?;
            vec![public.sized()?.to_vec()]
        }
        ALG_ECC => {
            public.u16()?;
            if public.u16()? != ALG_NULL {
                public.u16()?;
            }
            vec![public.sized()?.to_vec(), public.sized()?.to_vec()]
        }
        other => return Err(format!("unexpected key type {other:#x}")),
    };
    Ok((handle, unique))
}

fn flush(tpm: &mut dyn FnMut(&[u8]) -> Vec<u8>, handle: u32) -> Result<(), String> {
    let mut buffer = Vec::new();
    run(
        tpm,
        "TPM2_FlushContext",
        Command::new(ST_NO_SESSIONS, CC_FLUSH_CONTEXT).u32(handle),
        &mut buffer,
    )?;
    Ok(())
}

/// The TPM's manufacturer ID (TPM_PT_MANUFACTURER), firmware version and vendor
/// string, as the certificate names them.
fn identity(tpm: &mut dyn FnMut(&[u8]) -> Vec<u8>) -> Result<(u32, String, u32), String> {
    let command = Command::new(ST_NO_SESSIONS, CC_GET_CAPABILITY)
        .u32(CAP_TPM_PROPERTIES)
        .u32(PT_MANUFACTURER)
        .u32(8);
    let mut buffer = Vec::new();
    let mut response = run(tpm, "TPM2_GetCapability", command, &mut buffer)?;
    let _more = response.u8()?;
    let _capability = response.u32()?;
    let count = response.u32()?;
    let mut properties = std::collections::BTreeMap::new();
    for _ in 0..count {
        let property = response.u32()?;
        properties.insert(property, response.u32()?);
    }
    let get = |property| properties.get(&property).copied().unwrap_or(0);
    let model: String = (0x106..=0x109)
        .flat_map(|property| get(property).to_be_bytes())
        .filter(|byte| byte.is_ascii_graphic() || *byte == b' ')
        .map(char::from)
        .collect();
    Ok((get(PT_MANUFACTURER), model.trim().to_owned(), get(0x10b)))
}

/// Writes an EK certificate into a newly manufactured TPM. The TPM must be powered
/// on, before TPM2_Startup; it's left shut down (orderly), ready for the platform
/// reset that starts the guest.
pub fn endorsement_certificate(tpm: &mut dyn FnMut(&[u8]) -> Vec<u8>) -> Result<(), String> {
    let mut buffer = Vec::new();
    run(
        tpm,
        "TPM2_Startup",
        Command::new(ST_NO_SESSIONS, CC_STARTUP).u16(0),
        &mut buffer,
    )?;
    let result = provision(tpm);
    run(
        tpm,
        "TPM2_Shutdown",
        Command::new(ST_NO_SESSIONS, CC_SHUTDOWN).u16(0),
        &mut buffer,
    )?;
    result
}

fn provision(tpm: &mut dyn FnMut(&[u8]) -> Vec<u8>) -> Result<(), String> {
    let mut buffer = Vec::new();
    let (manufacturer, model, firmware) = identity(tpm)?;
    let (ek, ek_unique) = create_primary(tpm, RH_ENDORSEMENT, &ek_template())?;
    flush(tpm, ek)?;
    let modulus = &ek_unique[0];
    let (issuer, issuer_point) = create_primary(tpm, RH_PLATFORM, &issuer_template())?;

    let tbs = der::tbs_certificate(
        modulus,
        &issuer_point[0],
        &issuer_point[1],
        manufacturer,
        &model,
        firmware,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs()),
    );
    let digest = Sha256::digest(&tbs);
    let command = Command::new(ST_SESSIONS, CC_SIGN)
        .u32(issuer)
        .password()
        .sized(&digest)
        .u16(ALG_ECDSA)
        .u16(ALG_SHA256)
        .u16(ST_HASHCHECK)
        .u32(RH_NULL)
        .sized(&[]);
    let signature = {
        let mut response = run(tpm, "TPM2_Sign", command, &mut buffer)?;
        let _parameter_size = response.u32()?;
        let (_algorithm, _hash) = (response.u16()?, response.u16()?);
        (response.sized()?.to_vec(), response.sized()?.to_vec())
    };
    flush(tpm, issuer)?;
    let certificate = der::certificate(&tbs, &signature.0, &signature.1);

    let public = Command { bytes: Vec::new() }
        .u32(EK_CERTIFICATE_INDEX)
        .u16(ALG_SHA256)
        .u32(EK_CERTIFICATE_ATTRIBUTES)
        .u16(0)
        .u16(certificate.len() as u16)
        .bytes;
    let command = Command::new(ST_SESSIONS, CC_NV_DEFINE_SPACE)
        .u32(RH_PLATFORM)
        .password()
        .sized(&[])
        .sized(&public);
    run(tpm, "TPM2_NV_DefineSpace", command, &mut buffer)?;
    for (index, chunk) in certificate.chunks(NV_CHUNK).enumerate() {
        let command = Command::new(ST_SESSIONS, CC_NV_WRITE)
            .u32(RH_PLATFORM)
            .u32(EK_CERTIFICATE_INDEX)
            .password()
            .sized(chunk)
            .u16((index * NV_CHUNK) as u16);
        run(tpm, "TPM2_NV_Write", command, &mut buffer)?;
    }
    let command = Command::new(ST_SESSIONS, CC_NV_WRITE_LOCK)
        .u32(RH_PLATFORM)
        .u32(EK_CERTIFICATE_INDEX)
        .password();
    run(tpm, "TPM2_NV_WriteLock", command, &mut buffer)?;
    Ok(())
}

/// Just enough DER for an X.509 certificate.
mod der {
    const SEQUENCE: u8 = 0x30;
    const SET: u8 = 0x31;
    const INTEGER: u8 = 0x02;
    const BIT_STRING: u8 = 0x03;
    const OCTET_STRING: u8 = 0x04;
    const NULL: u8 = 0x05;
    const OID: u8 = 0x06;
    const UTF8_STRING: u8 = 0x0c;
    const UTC_TIME: u8 = 0x17;
    const GENERALIZED_TIME: u8 = 0x18;
    const BOOLEAN: u8 = 0x01;

    pub fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        let len = content.len();
        if len < 0x80 {
            out.push(len as u8);
        } else {
            let bytes: Vec<u8> = len
                .to_be_bytes()
                .into_iter()
                .skip_while(|byte| *byte == 0)
                .collect();
            out.push(0x80 | bytes.len() as u8);
            out.extend_from_slice(&bytes);
        }
        out.extend_from_slice(content);
        out
    }

    fn sequence(parts: &[Vec<u8>]) -> Vec<u8> {
        tlv(SEQUENCE, &parts.concat())
    }

    /// A non-negative INTEGER from big-endian bytes.
    pub fn unsigned(bytes: &[u8]) -> Vec<u8> {
        let trimmed: Vec<u8> = bytes
            .iter()
            .copied()
            .skip_while(|byte| *byte == 0)
            .collect();
        let mut content = Vec::with_capacity(trimmed.len() + 1);
        if trimmed.first().is_none_or(|byte| byte & 0x80 != 0) {
            content.push(0);
        }
        content.extend_from_slice(&trimmed);
        tlv(INTEGER, &content)
    }

    pub fn oid(arcs: &[u64]) -> Vec<u8> {
        let mut content = vec![(arcs[0] * 40 + arcs[1]) as u8];
        for &arc in &arcs[2..] {
            let mut groups = vec![(arc & 0x7f) as u8];
            let mut rest = arc >> 7;
            while rest != 0 {
                groups.push((rest & 0x7f) as u8 | 0x80);
                rest >>= 7;
            }
            content.extend(groups.iter().rev());
        }
        tlv(OID, &content)
    }

    fn utf8(text: &str) -> Vec<u8> {
        tlv(UTF8_STRING, text.as_bytes())
    }

    fn bit_string(bytes: &[u8]) -> Vec<u8> {
        let mut content = vec![0];
        content.extend_from_slice(bytes);
        tlv(BIT_STRING, &content)
    }

    /// One attribute per RDN, as `(OID, UTF8String)`.
    fn name(attributes: &[(&[u64], &str)]) -> Vec<u8> {
        let rdns: Vec<Vec<u8>> = attributes
            .iter()
            .map(|(attribute, value)| tlv(SET, &sequence(&[oid(attribute), utf8(value)])))
            .collect();
        sequence(&rdns)
    }

    fn extension(id: &[u64], critical: bool, value: &[u8]) -> Vec<u8> {
        let mut parts = vec![oid(id)];
        if critical {
            parts.push(tlv(BOOLEAN, &[0xff]));
        }
        parts.push(tlv(OCTET_STRING, value));
        sequence(&parts)
    }

    /// Seconds since 1970 as (year, month, day, hour, minute, second), in UTC.
    pub fn civil(seconds: u64) -> (u64, u64, u64, u64, u64, u64) {
        let days = seconds / 86_400;
        let rest = seconds % 86_400;
        // Howard Hinnant's days-to-civil.
        let z = days + 719_468;
        let era = z / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era * 400 + u64::from(month <= 2);
        (year, month, day, rest / 3600, rest % 3600 / 60, rest % 60)
    }

    fn time(seconds: u64) -> Vec<u8> {
        let (year, month, day, hour, minute, second) = civil(seconds);
        // RFC 5280: UTCTime through 2049, GeneralizedTime from 2050.
        if year < 2050 {
            let text = format!(
                "{:02}{month:02}{day:02}{hour:02}{minute:02}{second:02}Z",
                year % 100
            );
            tlv(UTC_TIME, text.as_bytes())
        } else {
            let text = format!("{year:04}{month:02}{day:02}{hour:02}{minute:02}{second:02}Z");
            tlv(GENERALIZED_TIME, text.as_bytes())
        }
    }

    const ECDSA_WITH_SHA256: &[u64] = &[1, 2, 840, 10045, 4, 3, 2];

    #[allow(clippy::too_many_arguments)]
    pub fn tbs_certificate(
        modulus: &[u8],
        issuer_x: &[u8],
        issuer_y: &[u8],
        manufacturer: u32,
        model: &str,
        firmware: u32,
        now: u64,
    ) -> Vec<u8> {
        use sha2::{Digest, Sha256};

        // A positive serial number that's the same for the same key.
        let mut serial = Sha256::digest(modulus)[..16].to_vec();
        serial[0] &= 0x7f;
        let mut issuer_key = vec![4];
        issuer_key.extend_from_slice(issuer_x);
        issuer_key.extend_from_slice(issuer_y);
        let issuer_id = &Sha256::digest(&issuer_key)[..8];
        let issuer = name(&[
            (&[2, 5, 4, 10], "vmmc"),
            (
                &[2, 5, 4, 3],
                &format!("vmmc TPM EK issuer {}", hex(issuer_id)),
            ),
        ]);
        let rsa_key = sequence(&[unsigned(modulus), unsigned(&65_537u32.to_be_bytes())]);
        let subject_public_key = sequence(&[
            sequence(&[oid(&[1, 2, 840, 113_549, 1, 1, 1]), tlv(NULL, &[])]),
            bit_string(&rsa_key),
        ]);
        // The TPM's identity, in the TCG attributes (EK Credential Profile 3.2.9).
        let tpm_name = name(&[
            (&[2, 23, 133, 2, 1], &format!("id:{manufacturer:08X}")),
            (
                &[2, 23, 133, 2, 2],
                if model.is_empty() { "vmmc" } else { model },
            ),
            (&[2, 23, 133, 2, 3], &format!("id:{firmware:08X}")),
        ]);
        // GeneralNames { directoryName [4] EXPLICIT Name }.
        let alternative_name = sequence(&[tlv(0xa4, &tpm_name)]);
        // The TPM specification it implements: family "2.0", level 0, revision 1.83.
        let specification = sequence(&[
            oid(&[2, 23, 133, 2, 16]),
            tlv(
                SET,
                &sequence(&[utf8("2.0"), unsigned(&[0]), unsigned(&[183])]),
            ),
        ]);
        let extensions = sequence(&[
            extension(&[2, 5, 29, 19], true, &sequence(&[])),
            // keyEncipherment.
            extension(&[2, 5, 29, 15], true, &tlv(BIT_STRING, &[5, 0x20])),
            extension(&[2, 5, 29, 17], true, &alternative_name),
            extension(&[2, 5, 29, 9], false, &sequence(&[specification])),
            extension(
                &[2, 5, 29, 37],
                false,
                &sequence(&[oid(&[2, 23, 133, 8, 1])]),
            ),
        ]);
        sequence(&[
            tlv(0xa0, &unsigned(&[2])),
            unsigned(&serial),
            sequence(&[oid(ECDSA_WITH_SHA256)]),
            issuer,
            sequence(&[time(now), tlv(GENERALIZED_TIME, b"99991231235959Z")]),
            // No subject: the alternative name, which is critical, identifies the TPM.
            sequence(&[]),
            subject_public_key,
            tlv(0xa3, &extensions),
        ])
    }

    pub fn certificate(tbs: &[u8], r: &[u8], s: &[u8]) -> Vec<u8> {
        let signature = sequence(&[unsigned(r), unsigned(s)]);
        sequence(&[
            tbs.to_vec(),
            sequence(&[oid(ECDSA_WITH_SHA256)]),
            bit_string(&signature),
        ])
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn encodes_the_basics() {
            assert_eq!(tlv(0x04, &[1, 2]), [0x04, 2, 1, 2]);
            assert_eq!(&tlv(0x04, &[0; 200])[..3], &[0x04, 0x81, 200]);
            assert_eq!(&tlv(0x04, &[0; 300])[..4], &[0x04, 0x82, 0x01, 0x2c]);
            assert_eq!(unsigned(&[0x80]), [0x02, 2, 0, 0x80], "kept positive");
            assert_eq!(unsigned(&[0, 0, 5]), [0x02, 1, 5], "minimal");
            assert_eq!(unsigned(&[0]), [0x02, 1, 0]);
            assert_eq!(
                oid(&[1, 2, 840, 113_549, 1, 1, 1]),
                [
                    0x06, 9, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01
                ]
            );
            assert_eq!(
                oid(&[2, 23, 133, 8, 1]),
                [0x06, 5, 0x67, 0x81, 0x05, 0x08, 0x01]
            );
        }

        #[test]
        fn dates_are_civil_utc() {
            assert_eq!(civil(0), (1970, 1, 1, 0, 0, 0));
            // 2026-10-12 21:30:05 UTC.
            assert_eq!(civil(1_791_840_605), (2026, 10, 12, 21, 30, 5));
            assert_eq!(civil(951_782_400), (2000, 2, 29, 0, 0, 0), "leap day");
        }

        #[test]
        fn the_certificate_carries_the_key_and_the_tpm() {
            let modulus = [0xc5u8; 256];
            let tbs = tbs_certificate(&modulus, &[1; 32], &[2; 32], 0x5859_5a20, "", 0, 0);
            let certificate = certificate(&tbs, &[0x81; 32], &[0x01; 32]);
            assert_eq!(certificate[0], SEQUENCE);
            let contains = |needle: &[u8]| certificate.windows(needle.len()).any(|w| w == needle);
            // The modulus, as a positive INTEGER.
            let mut integer = vec![0x02, 0x82, 0x01, 0x01, 0x00];
            integer.extend_from_slice(&modulus);
            assert!(contains(&integer));
            assert!(contains(b"id:58595A20"), "manufacturer");
            assert!(contains(b"vmmc"), "model");
            assert!(
                contains(&oid(&[2, 23, 133, 8, 1])),
                "EK certificate purpose"
            );
        }
    }
}
