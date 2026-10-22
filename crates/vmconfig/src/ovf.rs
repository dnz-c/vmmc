//! OVF packages (DMTF DSP0243), the usual way to move a VM between hypervisors: an
//! `.ovf` (XML: the virtual hardware and the disks), the disks as stream-optimized VMDKs,
//! and an `.mf` with their SHA-256 (or SHA-1) digests; side by side, or tarred into one
//! `.ova`.
//!
//! - **Export** writes a VM as its disks are now (it must be off): its CPUs, memory,
//!   UEFI settings, disks (on NVMe or SATA), DVD drives and NICs, as VMware writes them.
//! - **Import** makes a new VM from OVF 1.x or 2.x: the first virtual system's CPUs,
//!   memory, disks (each converted to a monolithic sparse VMDK in the VM's folder), DVD
//!   drives and NICs (e1000e), its firmware from VMware's settings, every file checked
//!   against the manifest. Disks on IDE or SCSI go on SATA. A BIOS VM is refused:
//!   vmmc's firmware is UEFI.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::Digest;

use crate::import::{Imported, os_from_guest};
use crate::{
    Bus, Cdrom, Disk, EXTENSION, MAX_CPUS, Nic, NicModel, Vm, VmConfig, format_mac, random_uuid,
    tar,
};

const STREAM_OPTIMIZED: &str =
    "http://www.vmware.com/interfaces/specifications/vmdk.html#streamOptimized";

/// The CIM operating system ID and VMware's guest type for a VM's OS.
fn os_ids(os: &str) -> (u32, &'static str) {
    match os {
        "windows11" => (1, "windows11_64Guest"),
        "windows10" => (1, "windows9_64Guest"),
        "ubuntu" => (94, "ubuntu64Guest"),
        "linux" => (101, "otherLinux64Guest"),
        _ => (1, "other64Guest"),
    }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// A hardware item, its `rasd` fields in the schema's (alphabetical) order.
fn item(mut fields: Vec<(&str, String)>) -> String {
    fields.sort_by_key(|(name, _)| *name);
    let mut out = String::from("      <Item>\n");
    for (name, value) in fields {
        out += &format!("        <rasd:{name}>{}</rasd:{name}>\n", escape(&value));
    }
    out + "      </Item>\n"
}

/// The `.ovf` for `vm`, whose disks went into `disks`: each its file name, capacity and
/// file size, in the config's order.
fn envelope(vm: &Vm, disks: &[(String, u64, u64)]) -> String {
    let config = &vm.config;
    let name = escape(&config.name);
    let mut references = String::new();
    let mut disk_section = String::new();
    for (index, (file, capacity, size)) in disks.iter().enumerate() {
        let n = index + 1;
        references += &format!(
            "    <File ovf:href=\"{}\" ovf:id=\"file{n}\" ovf:size=\"{size}\"/>\n",
            escape(file)
        );
        disk_section += &format!(
            "    <Disk ovf:capacity=\"{capacity}\" ovf:capacityAllocationUnits=\"byte\" ovf:diskId=\"vmdisk{n}\" ovf:fileRef=\"file{n}\" ovf:format=\"{STREAM_OPTIMIZED}\"/>\n"
        );
    }
    let mut items = item(vec![
        ("AllocationUnits", "hertz * 10^6".into()),
        ("Description", "Number of Virtual CPUs".into()),
        ("ElementName", format!("{} virtual CPU(s)", config.cpus)),
        ("InstanceID", "1".into()),
        ("ResourceType", "3".into()),
        ("VirtualQuantity", config.cpus.to_string()),
    ]);
    items += &item(vec![
        ("AllocationUnits", "byte * 2^20".into()),
        ("Description", "Memory Size".into()),
        ("ElementName", format!("{}MB of memory", config.memory_mib)),
        ("InstanceID", "2".into()),
        ("ResourceType", "4".into()),
        ("VirtualQuantity", config.memory_mib.to_string()),
    ]);
    // A controller for NVMe disks, and one for SATA disks and DVD drives.
    let nvme = config.disks.iter().any(|disk| disk.bus == Bus::Nvme);
    let sata = config.disks.iter().any(|disk| disk.bus != Bus::Nvme) || !config.cdroms.is_empty();
    let mut next_id = 3;
    let mut controller = |subtype: &str, label: &str, items: &mut String| {
        let id = next_id;
        next_id += 1;
        *items += &item(vec![
            ("Description", format!("{label} Controller")),
            ("ElementName", format!("{label} controller 0")),
            ("InstanceID", id.to_string()),
            ("ResourceSubType", subtype.into()),
            ("ResourceType", "20".into()),
        ]);
        id
    };
    let nvme_id = nvme.then(|| controller("vmware.nvme.controller", "NVMe", &mut items));
    let sata_id = sata.then(|| controller("vmware.sata.ahci", "SATA", &mut items));
    let (mut on_nvme, mut on_sata) = (0, 0);
    for (index, disk) in config.disks.iter().enumerate() {
        let (parent, address) = match (disk.bus, nvme_id, sata_id) {
            (Bus::Nvme, Some(id), _) => (id, &mut on_nvme),
            (_, _, Some(id)) => (id, &mut on_sata),
            _ => unreachable!("a controller for each bus in use"),
        };
        items += &item(vec![
            ("AddressOnParent", address.to_string()),
            ("ElementName", format!("Hard disk {}", index + 1)),
            ("HostResource", format!("ovf:/disk/vmdisk{}", index + 1)),
            ("InstanceID", next_id.to_string()),
            ("Parent", parent.to_string()),
            ("ResourceType", "17".into()),
        ]);
        *address += 1;
        next_id += 1;
    }
    for (index, _) in config.cdroms.iter().enumerate() {
        items += &item(vec![
            ("AddressOnParent", on_sata.to_string()),
            ("AutomaticAllocation", "false".into()),
            ("ElementName", format!("CD/DVD drive {}", index + 1)),
            ("InstanceID", next_id.to_string()),
            ("Parent", sata_id.unwrap_or_default().to_string()),
            ("ResourceSubType", "vmware.cdrom.remotepassthrough".into()),
            ("ResourceType", "15".into()),
        ]);
        on_sata += 1;
        next_id += 1;
    }
    for (index, _) in config.nics.iter().enumerate() {
        items += &item(vec![
            ("AddressOnParent", index.to_string()),
            ("AutomaticAllocation", "true".into()),
            ("Connection", "nat".into()),
            ("ElementName", format!("Network adapter {}", index + 1)),
            ("InstanceID", next_id.to_string()),
            ("ResourceSubType", "E1000e".into()),
            ("ResourceType", "10".into()),
        ]);
        next_id += 1;
    }
    let (os_id, os_type) = os_ids(&config.os);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!--Made by vmmc-->
<Envelope xmlns="http://schemas.dmtf.org/ovf/envelope/1" xmlns:cim="http://schemas.dmtf.org/wbem/wscim/1/common" xmlns:ovf="http://schemas.dmtf.org/ovf/envelope/1" xmlns:rasd="http://schemas.dmtf.org/wbem/wscim/1/cim-schema/2/CIM_ResourceAllocationSettingData" xmlns:vmw="http://www.vmware.com/schema/ovf" xmlns:vssd="http://schemas.dmtf.org/wbem/wscim/1/cim-schema/2/CIM_VirtualSystemSettingData" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <References>
{references}  </References>
  <DiskSection>
    <Info>Virtual disk information</Info>
{disk_section}  </DiskSection>
  <NetworkSection>
    <Info>The list of logical networks</Info>
    <Network ovf:name="nat">
      <Description>The nat network</Description>
    </Network>
  </NetworkSection>
  <VirtualSystem ovf:id="{name}">
    <Info>A virtual machine</Info>
    <Name>{name}</Name>
    <OperatingSystemSection ovf:id="{os_id}" vmw:osType="{os_type}">
      <Info>The kind of installed guest operating system</Info>
    </OperatingSystemSection>
    <VirtualHardwareSection>
      <Info>Virtual hardware requirements</Info>
      <System>
        <vssd:ElementName>Virtual Hardware Family</vssd:ElementName>
        <vssd:InstanceID>0</vssd:InstanceID>
        <vssd:VirtualSystemIdentifier>{name}</vssd:VirtualSystemIdentifier>
        <vssd:VirtualSystemType>vmx-19</vssd:VirtualSystemType>
      </System>
{items}      <vmw:Config ovf:required="false" vmw:key="firmware" vmw:value="efi"/>
      <vmw:Config ovf:required="false" vmw:key="uefi.secureBoot.enabled" vmw:value="{secure_boot}"/>
    </VirtualHardwareSection>
  </VirtualSystem>
</Envelope>
"#,
        secure_boot = config.firmware.secure_boot,
    )
}

/// Feeds what `reader` has to `hasher`, and returns the digest.
fn feed<D: Digest>(mut reader: impl Read, mut hasher: D) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            return Ok(hasher.finalize().to_vec());
        }
        hasher.update(&buf[..n]);
    }
}

fn sha256_hex(path: &Path) -> Result<String, String> {
    fs::File::open(path)
        .and_then(|file| feed(file, sha2::Sha256::new()))
        .map(|digest| hex(&digest))
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Writes `vm`, as its disks are now (it must be off), as an OVF package at `to`: an
/// `.ova`, or an `.ovf` with its disks and manifest beside it. `progress` hears the disk
/// bytes written and their total.
pub fn export(vm: &Vm, to: &Path, mut progress: impl FnMut(u64, u64)) -> Result<(), String> {
    let extension = to
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase());
    let ova = match extension.as_deref() {
        Some("ova") => true,
        Some("ovf") => false,
        _ => {
            return Err(format!(
                "{}: an OVF package is an .ova or .ovf file",
                to.display()
            ));
        }
    };
    if to.exists() {
        return Err(format!("{} is there already", to.display()));
    }
    let stem = to
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| vm.config.name.clone());
    let dir = to.parent().unwrap_or(Path::new("."));
    // An .ova's files are made in a folder beside it, then tarred.
    let work = if ova {
        dir.join(format!(".{stem}.ova-parts"))
    } else {
        dir.to_owned()
    };
    fs::create_dir_all(&work).map_err(|e| format!("can't create {}: {e}", work.display()))?;
    let mut made: Vec<PathBuf> = Vec::new();
    let written = (|| {
        let opened: Vec<(String, Arc<dyn disk::BlockBackend>)> = vm
            .config
            .disks
            .iter()
            .enumerate()
            .map(|(index, disk)| {
                let path = vm.resolve(&disk.path);
                disk::open(&path, true)
                    .map(|opened| (format!("{stem}-disk{}.vmdk", index + 1), opened))
                    .map_err(|e| format!("{}: {e}", path.display()))
            })
            .collect::<Result<_, _>>()?;
        let total: u64 = opened.iter().map(|(_, disk)| disk.len()).sum();
        let mut disks = Vec::new();
        let mut before = 0;
        for (name, opened) in &opened {
            let path = work.join(name);
            disk::write_stream_optimized(opened.as_ref(), &path, |done| {
                progress(before + done, total)
            })
            .map_err(|e| format!("can't write {}: {e}", path.display()))?;
            made.push(path.clone());
            before += opened.len();
            let size = fs::metadata(&path).map_err(|e| e.to_string())?.len();
            disks.push((name.clone(), opened.len(), size));
        }
        let ovf = work.join(format!("{stem}.ovf"));
        if !ova && ovf.exists() {
            return Err(format!("{} is there already", ovf.display()));
        }
        fs::write(&ovf, envelope(vm, &disks))
            .map_err(|e| format!("can't write {}: {e}", ovf.display()))?;
        made.push(ovf.clone());
        let mut manifest = String::new();
        for path in std::iter::once(&ovf).chain(made.iter().filter(|path| **path != ovf)) {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            manifest += &format!("SHA256({name})= {}\n", sha256_hex(path)?);
        }
        let mf = work.join(format!("{stem}.mf"));
        fs::write(&mf, manifest).map_err(|e| format!("can't write {}: {e}", mf.display()))?;
        made.push(mf.clone());
        if ova {
            // The .ovf first, as the standard wants, then the manifest and the disks.
            let mut files = vec![ovf.clone(), mf];
            files.extend(
                made.iter()
                    .filter(|path| path.extension().is_some_and(|e| e == "vmdk"))
                    .cloned(),
            );
            let named: Vec<(String, &Path)> = files
                .iter()
                .map(|path| {
                    (
                        path.file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned(),
                        path.as_path(),
                    )
                })
                .collect();
            let named: Vec<(&str, &Path)> = named
                .iter()
                .map(|(name, path)| (name.as_str(), *path))
                .collect();
            tar::write(to, &named).map_err(|e| format!("can't write {}: {e}", to.display()))?;
        }
        Ok(())
    })();
    if ova {
        let _ = fs::remove_dir_all(&work);
    } else if written.is_err() {
        for path in &made {
            let _ = fs::remove_file(path);
        }
    }
    if written.is_err() && ova {
        let _ = fs::remove_file(to);
    }
    written
}

/// Where one of a package's files is.
enum Source {
    File(PathBuf),
    Tar(PathBuf, tar::Entry),
}

impl Source {
    /// Feeds the file to `hasher`.
    fn digest<D: Digest>(&self, hasher: D) -> Result<Vec<u8>, String> {
        match self {
            Source::File(path) => fs::File::open(path).and_then(|file| feed(file, hasher)),
            Source::Tar(archive, entry) => fs::File::open(archive).and_then(|mut file| {
                use std::io::Seek;
                file.seek(std::io::SeekFrom::Start(entry.offset))?;
                feed(file.take(entry.size), hasher)
            }),
        }
        .map_err(|e| e.to_string())
    }

    fn read_text(&self) -> Result<String, String> {
        let bytes = match self {
            Source::File(path) => fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?,
            Source::Tar(archive, entry) => {
                if entry.size > 64 << 20 {
                    return Err(format!("{} is too big for an OVF descriptor", entry.name));
                }
                let mut file = fs::File::open(archive).map_err(|e| e.to_string())?;
                use std::io::Seek;
                file.seek(std::io::SeekFrom::Start(entry.offset))
                    .map_err(|e| e.to_string())?;
                let mut bytes = vec![0u8; entry.size as usize];
                file.read_exact(&mut bytes).map_err(|e| e.to_string())?;
                bytes
            }
        };
        String::from_utf8(
            bytes
                .strip_prefix(b"\xef\xbb\xbf")
                .map(<[u8]>::to_vec)
                .unwrap_or(bytes),
        )
        .map_err(|_| "the OVF descriptor isn't UTF-8".to_owned())
    }
}

/// The package's files by name: the `.ova`'s entries, or the `.ovf` and the files its
/// folder has. Returns them and the descriptor's name.
fn package(path: &Path) -> Result<(HashMap<String, Source>, String), String> {
    let extension = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase());
    match extension.as_deref() {
        Some("ova") => {
            let entries = tar::entries(path).map_err(|e| format!("{}: {e}", path.display()))?;
            let ovf = entries
                .iter()
                .find(|entry| entry.name.to_ascii_lowercase().ends_with(".ovf"))
                .map(|entry| entry.name.clone())
                .ok_or("the .ova has no .ovf in it")?;
            let files = entries
                .into_iter()
                .map(|entry| (entry.name.clone(), Source::Tar(path.to_owned(), entry)))
                .collect();
            Ok((files, ovf))
        }
        Some("ovf") => {
            let dir = path.parent().unwrap_or(Path::new("."));
            let mut files = HashMap::new();
            for entry in fs::read_dir(dir)
                .map_err(|e| format!("{}: {e}", dir.display()))?
                .flatten()
            {
                let name = entry.file_name().to_string_lossy().into_owned();
                files.insert(name, Source::File(entry.path()));
            }
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            Ok((files, name))
        }
        _ => Err(format!(
            "{}: an OVF package is an .ova or .ovf file",
            path.display()
        )),
    }
}

/// Checks the package's files against its manifest (`NAME.mf`), if it has one.
fn check_manifest(files: &HashMap<String, Source>, ovf: &str) -> Result<(), String> {
    let mf = format!("{}.mf", ovf.rsplit_once('.').map_or(ovf, |(stem, _)| stem));
    let Some(manifest) = files.get(&mf) else {
        return Ok(());
    };
    for line in manifest.read_text()?.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parsed = line.split_once('(').and_then(|(algorithm, rest)| {
            let (name, digest) = rest.split_once(")")?;
            Some((
                algorithm.trim(),
                name,
                digest.trim().trim_start_matches('=').trim(),
            ))
        });
        let Some((algorithm, name, expected)) = parsed else {
            return Err(format!("the manifest's line {line:?} isn't a digest"));
        };
        let file = files
            .get(name)
            .ok_or_else(|| format!("the manifest lists {name}, which the package hasn't"))?;
        let digest = match algorithm.to_ascii_uppercase().as_str() {
            "SHA256" => file.digest(sha2::Sha256::new())?,
            "SHA1" => file.digest(sha1::Sha1::new())?,
            "SHA512" => file.digest(sha2::Sha512::new())?,
            other => {
                return Err(format!(
                    "the manifest uses {other}, which vmmc doesn't check"
                ));
            }
        };
        if !hex(&digest).eq_ignore_ascii_case(expected) {
            return Err(format!(
                "{name} doesn't match its digest in the manifest: the package is damaged"
            ));
        }
    }
    Ok(())
}

/// An attribute by its local name, whatever its namespace.
fn attribute<'a>(node: roxmltree::Node<'a, '_>, name: &str) -> Option<&'a str> {
    node.attributes()
        .find(|a| a.name() == name)
        .map(|a| a.value())
}

fn child<'a, 'input>(
    node: roxmltree::Node<'a, 'input>,
    name: &str,
) -> Option<roxmltree::Node<'a, 'input>> {
    node.children()
        .find(|n| n.is_element() && n.tag_name().name() == name)
}

/// Bytes per unit in OVF's units: "byte * 2^20", "MegaBytes", "byte".
fn unit_bytes(units: &str) -> Result<u64, String> {
    let compact: String = units.chars().filter(|c| !c.is_whitespace()).collect();
    let lower = compact.to_ascii_lowercase();
    let named = [
        ("kilobytes", 10),
        ("megabytes", 20),
        ("gigabytes", 30),
        ("terabytes", 40),
    ];
    if let Some((_, shift)) = named.iter().find(|(name, _)| lower == *name) {
        return Ok(1 << shift);
    }
    let mut factor = 1u64;
    for part in lower.split('*') {
        factor = factor
            .checked_mul(match part {
                "byte" | "bytes" => 1,
                power if power.contains('^') => {
                    let (base, exponent) = power.split_once('^').unwrap();
                    let base: u64 = base.parse().map_err(|_| format!("unit {units:?}"))?;
                    let exponent: u32 = exponent.parse().map_err(|_| format!("unit {units:?}"))?;
                    base.checked_pow(exponent)
                        .ok_or(format!("unit {units:?}"))?
                }
                number => number.parse().map_err(|_| format!("unit {units:?}"))?,
            })
            .ok_or(format!("unit {units:?}"))?;
    }
    Ok(factor)
}

/// A hardware item's fields by local name.
type Item = HashMap<String, String>;

/// Makes a new VM, in a folder in `parent_dir`, from the OVF package at `path` (an
/// `.ova`, or an `.ovf` with its files beside it), named as the package names it.
/// `progress` hears the disk bytes converted and their total.
pub fn import(
    path: &Path,
    parent_dir: &Path,
    mut progress: impl FnMut(u64, u64),
) -> Result<Imported, String> {
    let (files, ovf_name) = package(path)?;
    check_manifest(&files, &ovf_name)?;
    let text = files[&ovf_name].read_text()?;
    let document = roxmltree::Document::parse(&text).map_err(|e| format!("{ovf_name}: {e}"))?;
    let root = document.root_element();
    if root.tag_name().name() != "Envelope" {
        return Err(format!("{ovf_name} isn't an OVF descriptor"));
    }
    let mut warnings = Vec::new();
    // References and disks.
    let references: HashMap<&str, &str> = child(root, "References")
        .into_iter()
        .flat_map(|references| {
            references
                .children()
                .filter(|n| n.tag_name().name() == "File")
        })
        .filter_map(|file| Some((attribute(file, "id")?, attribute(file, "href")?)))
        .collect();
    let mut disk_files: HashMap<&str, (Option<&str>, u64)> = HashMap::new();
    for disk in root.descendants().filter(|n| {
        n.is_element() && n.tag_name().name() == "Disk" && attribute(*n, "diskId").is_some()
    }) {
        let id = attribute(disk, "diskId").unwrap();
        let units = attribute(disk, "capacityAllocationUnits").map_or(Ok(1), unit_bytes)?;
        let capacity: u64 = attribute(disk, "capacity")
            .ok_or(format!("disk {id} has no capacity"))?
            .parse()
            .map_err(|_| format!("disk {id}'s capacity isn't a number"))?;
        let file = attribute(disk, "fileRef")
            .map(|reference| {
                references.get(reference).copied().ok_or(format!(
                    "disk {id}'s file {reference} isn't in the references"
                ))
            })
            .transpose()?;
        disk_files.insert(id, (file, capacity.saturating_mul(units)));
    }
    let system = root
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name() == "VirtualSystem")
        .ok_or(format!("{ovf_name} has no virtual system"))?;
    let name = child(system, "Name")
        .and_then(|n| n.text())
        .or_else(|| attribute(system, "id"))
        .unwrap_or("Imported")
        .trim()
        .to_owned();
    let os = child(system, "OperatingSystemSection")
        .map(|section| {
            let guest = attribute(section, "osType")
                .map(str::to_owned)
                .or_else(|| {
                    child(section, "Description")
                        .and_then(|d| d.text())
                        .map(str::to_owned)
                })
                .unwrap_or_default();
            os_from_guest(&guest.replace(' ', ""))
        })
        .unwrap_or("other");
    let hardware = child(system, "VirtualHardwareSection")
        .ok_or(format!("{ovf_name} has no virtual hardware"))?;
    let items: Vec<Item> = hardware
        .children()
        .filter(|n| {
            matches!(
                n.tag_name().name(),
                "Item" | "StorageItem" | "EthernetPortItem"
            )
        })
        .map(|item| {
            item.children()
                .filter(|n| n.is_element())
                .map(|field| {
                    (
                        field.tag_name().name().to_owned(),
                        field.text().unwrap_or_default().trim().to_owned(),
                    )
                })
                .collect()
        })
        .collect();
    let settings: HashMap<&str, &str> = hardware
        .children()
        .filter(|n| n.tag_name().name() == "Config")
        .filter_map(|config| Some((attribute(config, "key")?, attribute(config, "value")?)))
        .collect();
    match settings.get("firmware").copied() {
        Some("efi") => {}
        Some(other) => {
            return Err(format!(
                "{name} boots with {other} firmware: vmmc's is UEFI, which a guest installed for BIOS can't boot from"
            ));
        }
        None => warnings.push(
            "the package doesn't say which firmware the VM had: vmmc's is UEFI, which a guest installed for BIOS can't boot from".into(),
        ),
    }
    let kind = |item: &Item| item.get("ResourceType").cloned().unwrap_or_default();
    let quantity = |resource: &str| {
        items
            .iter()
            .find(|item| kind(item) == resource)
            .and_then(|item| {
                Some((
                    item.get("VirtualQuantity")?.parse::<u64>().ok()?,
                    item.get("AllocationUnits").cloned(),
                ))
            })
    };
    let cpus = quantity("3").map_or(1, |(n, _)| n.clamp(1, u64::from(MAX_CPUS)) as u8);
    let memory_mib = match quantity("4") {
        Some((n, units)) => {
            let bytes = n.saturating_mul(units.as_deref().map_or(Ok(1 << 20), unit_bytes)?);
            (bytes >> 20).max(32)
        }
        None => 1024,
    };

    // The VM's folder, its name made unique.
    let folder_name: String = name
        .chars()
        .map(|c| if "\\/:*?\"<>|".contains(c) { '_' } else { c })
        .collect();
    let mut dir = parent_dir.join(&folder_name);
    let mut number = 2;
    while dir.exists() {
        dir = parent_dir.join(format!("{folder_name} {number}"));
        number += 1;
    }
    let vm_name = dir
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    let made = (|| {
        let mut config = VmConfig::new(&vm_name, os, cpus, memory_mib);
        config.firmware.secure_boot = settings.get("uefi.secureBoot.enabled") == Some(&"true");
        config.nics.clear();
        let controllers: HashMap<String, &Item> = items
            .iter()
            .filter_map(|item| Some((item.get("InstanceID")?.clone(), item)))
            .collect();
        let disks: Vec<&Item> = items.iter().filter(|item| kind(item) == "17").collect();
        let total: u64 = disks
            .iter()
            .filter_map(|item| {
                disk_files
                    .get(disk_id(item)?)
                    .map(|(_, capacity)| *capacity)
            })
            .sum();
        let mut before = 0;
        for (index, item) in disks.iter().enumerate() {
            let id = disk_id(item).ok_or(format!("disk {} names no disk", index + 1))?;
            let (file, capacity) = *disk_files
                .get(id)
                .ok_or(format!("the disk {id} isn't in the disk section"))?;
            let bus = match item
                .get("Parent")
                .and_then(|parent| controllers.get(parent))
            {
                Some(controller) => {
                    match (kind(controller).as_str(), controller.get("ResourceSubType")) {
                        ("20", Some(subtype)) if subtype.to_ascii_lowercase().contains("nvme") => {
                            Bus::Nvme
                        }
                        ("5", _) | ("6", _) => {
                            warnings.push(format!(
                                "disk {} was on IDE or SCSI, and is on SATA now",
                                index + 1
                            ));
                            Bus::Sata
                        }
                        _ => Bus::Sata,
                    }
                }
                None => Bus::Sata,
            };
            let disk_name = if index == 0 {
                format!("{vm_name}.vmdk")
            } else {
                format!("{vm_name}-{}.vmdk", index + 1)
            };
            let to = dir.join(&disk_name);
            match file {
                Some(file) => {
                    if !file.to_ascii_lowercase().ends_with(".vmdk") {
                        return Err(format!("{file} isn't a VMDK, which is all vmmc imports"));
                    }
                    let source = files
                        .get(file)
                        .ok_or(format!("the package hasn't {file}"))?;
                    // An .ova's disk comes out of it first.
                    let (from, temporary) = match source {
                        Source::File(path) => (path.clone(), false),
                        Source::Tar(archive, entry) => {
                            let out = dir.join(format!(".import-{}.vmdk", index + 1));
                            tar::extract(archive, entry, &out)
                                .map_err(|e| format!("can't take {file} out: {e}"))?;
                            (out, true)
                        }
                    };
                    let converted = (|| {
                        let source = disk::open(&from, true).map_err(|e| format!("{file}: {e}"))?;
                        let len = source.len().max(capacity).next_multiple_of(512);
                        disk::create_sparse(&to, len)
                            .map_err(|e| format!("can't make {}: {e}", to.display()))?;
                        let target = disk::open(&to, false).map_err(|e| e.to_string())?;
                        disk::copy(source.as_ref(), target.as_ref(), |done| {
                            progress(before + done, total)
                        })
                        .map_err(|e| format!("can't convert {file}: {e}"))
                    })();
                    if temporary {
                        let _ = fs::remove_file(&from);
                    }
                    converted?;
                }
                None => disk::create_sparse(&to, capacity.max(1 << 20).next_multiple_of(512))
                    .map_err(|e| format!("can't make {}: {e}", to.display()))?,
            }
            before += capacity;
            config.disks.push(Disk {
                path: disk_name,
                bus,
                read_only: false,
            });
        }
        for _ in items
            .iter()
            .filter(|item| matches!(kind(item).as_str(), "15" | "16"))
        {
            config.cdroms.push(Cdrom {
                path: None,
                install_media: false,
            });
        }
        let uuid = random_uuid();
        for (index, item) in items.iter().filter(|item| kind(item) == "10").enumerate() {
            let subtype = item.get("ResourceSubType").cloned().unwrap_or_default();
            if !subtype.to_ascii_lowercase().starts_with("e1000") {
                warnings.push(format!(
                    "network adapter {} ({subtype}) is an e1000e now",
                    index + 1
                ));
            }
            config.nics.push(Nic {
                model: NicModel::E1000e,
                mac: format_mac(&[0x52, 0x54, 0x00, uuid[13], uuid[14], uuid[15] ^ index as u8]),
            });
        }
        config.validate()?;
        let vm = Vm {
            path: dir.join(format!("{vm_name}.{EXTENSION}")),
            config,
        };
        vm.save().map_err(|e| e.to_string())?;
        Ok(vm)
    })();
    match made {
        Ok(vm) => Ok(Imported { vm, warnings }),
        Err(e) => {
            let _ = fs::remove_dir_all(&dir);
            Err(e)
        }
    }
}

/// The disk ID a disk item names: `ovf:/disk/ID`.
fn disk_id(item: &Item) -> Option<&str> {
    let resource = item.get("HostResource")?;
    resource
        .strip_prefix("ovf:/disk/")
        .or_else(|| resource.strip_prefix("/disk/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vm(dir: &Path) -> Vm {
        let folder = dir.join("Source");
        fs::create_dir_all(&folder).unwrap();
        let mut config = VmConfig::new("Source & <Co>", "ubuntu", 2, 3072);
        config.firmware.secure_boot = false;
        config.disks = vec![
            Disk {
                path: "Source.vmdk".into(),
                bus: Bus::Nvme,
                read_only: false,
            },
            Disk {
                path: "data.vmdk".into(),
                bus: Bus::Sata,
                read_only: false,
            },
        ];
        config.cdroms = vec![Cdrom {
            path: None,
            install_media: false,
        }];
        for (name, data, at) in [
            ("Source.vmdk", b"system", 0u64),
            ("data.vmdk", b"data..", 5 << 20),
        ] {
            let path = folder.join(name);
            disk::create_sparse(&path, 8 << 20).unwrap();
            disk::open(&path, false)
                .unwrap()
                .write_at(data, at)
                .unwrap();
        }
        let vm = Vm {
            path: folder.join("Source.vmmc"),
            config,
        };
        vm.save().unwrap();
        vm
    }

    fn read(vm: &Vm, disk: usize, at: u64) -> Vec<u8> {
        let opened = disk::open(&vm.resolve(&vm.config.disks[disk].path), true).unwrap();
        let mut data = vec![0u8; 6];
        opened.read_at(&mut data, at).unwrap();
        data
    }

    #[test]
    fn an_ova_round_trip_keeps_the_hardware_and_the_disks() {
        let root = tempfile::tempdir().unwrap();
        let source = vm(root.path());
        let ova = root.path().join("Source.ova");
        let mut last = (0, 0);
        export(&source, &ova, |done, total| last = (done, total)).unwrap();
        assert_eq!(last, (16 << 20, 16 << 20));
        assert!(
            !root.path().join(".Source.ova-parts").exists(),
            "the parts are gone"
        );
        let names: Vec<String> = tar::entries(&ova)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(
            names,
            [
                "Source.ovf",
                "Source.mf",
                "Source-disk1.vmdk",
                "Source-disk2.vmdk"
            ]
        );

        let into = root.path().join("imported");
        fs::create_dir(&into).unwrap();
        let imported = import(&ova, &into, |_, _| {}).unwrap();
        assert_eq!(imported.warnings, Vec::<String>::new());
        let config = &imported.vm.config;
        // Named as its folder is: what Windows refuses in a file name goes.
        assert_eq!(config.name, "Source & _Co_");
        assert_eq!(
            (config.cpus, config.memory_mib, config.os.as_str()),
            (2, 3072, "ubuntu")
        );
        assert!(!config.firmware.secure_boot);
        assert_eq!(
            config.disks.iter().map(|d| d.bus).collect::<Vec<_>>(),
            [Bus::Nvme, Bus::Sata]
        );
        assert_eq!((config.cdroms.len(), config.nics.len()), (1, 1));
        assert_ne!(config.nics[0].mac, source.config.nics[0].mac);
        assert_eq!(read(&imported.vm, 0, 0), b"system");
        assert_eq!(read(&imported.vm, 1, 5 << 20), b"data..");
        // Imported again: a folder of its own.
        let again = import(&ova, &into, |_, _| {}).unwrap();
        assert_eq!(again.vm.config.name, "Source & _Co_ 2");
    }

    #[test]
    fn an_ovf_with_a_damaged_disk_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let source = vm(root.path());
        let out = root.path().join("out");
        fs::create_dir(&out).unwrap();
        let ovf = out.join("Source.ovf");
        export(&source, &ovf, |_, _| {}).unwrap();
        assert!(out.join("Source.mf").is_file());
        let into = root.path().join("imported");
        fs::create_dir(&into).unwrap();
        import(&ovf, &into, |_, _| {}).unwrap();
        let disk = out.join("Source-disk2.vmdk");
        let mut bytes = fs::read(&disk).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        fs::write(&disk, bytes).unwrap();
        let error = import(&ovf, &into, |_, _| {}).err().unwrap();
        assert!(error.contains("doesn't match its digest"), "{error}");
    }

    #[test]
    fn units_come_in_several_spellings() {
        assert_eq!(unit_bytes("byte * 2^20").unwrap(), 1 << 20);
        assert_eq!(unit_bytes("byte*2^30").unwrap(), 1 << 30);
        assert_eq!(unit_bytes("MegaBytes").unwrap(), 1 << 20);
        assert_eq!(unit_bytes("byte").unwrap(), 1);
        assert!(unit_bytes("furlongs").is_err());
    }
}
