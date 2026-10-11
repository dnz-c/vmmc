//! Kernel modules for the test initramfs, taken out of the Ubuntu cloud image's own
//! initrd, which is built for the same kernel. Ubuntu builds some drivers the tests need
//! (NVMe) as modules rather than into the kernel.

use std::collections::HashMap;
use std::io::Read;

use ruzstd::decoding::StreamingDecoder;

use crate::cpio;

/// Decompresses one or more zstd frames.
fn unzstd(mut compressed: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    while !compressed.is_empty() {
        let mut decoder =
            StreamingDecoder::new(&mut compressed).map_err(|e| format!("zstd: {e}"))?;
        decoder
            .read_to_end(&mut out)
            .map_err(|e| format!("zstd: {e}"))?;
    }
    Ok(out)
}

/// The modules named in `wanted` (by file stem, like `nvme`) and everything they depend
/// on, uncompressed, in the order to load them: `(file name, contents)`.
pub fn extract(initrd: &[u8], wanted: &[&str]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let files = cpio::files(&unzstd(initrd)?)?;
    let (dep_path, dep_text) = files
        .iter()
        .find(|(path, _)| path.ends_with("/modules.dep"))
        .ok_or("the initrd has no modules.dep")?;
    let module_dir = dep_path.trim_end_matches("modules.dep");
    let dep_text = String::from_utf8_lossy(dep_text);
    let deps: HashMap<&str, Vec<&str>> = dep_text
        .lines()
        .filter_map(|line| {
            let (module, deps) = line.split_once(':')?;
            Some((module.trim(), deps.split_whitespace().collect()))
        })
        .collect();
    let stem = |path: &str| -> String {
        let name = path.rsplit('/').next().unwrap_or(path);
        name.trim_end_matches(".zst")
            .trim_end_matches(".xz")
            .trim_end_matches(".ko")
            .replace('-', "_")
    };

    // Depth first, dependencies before the modules that need them.
    fn visit<'a>(module: &'a str, deps: &HashMap<&'a str, Vec<&'a str>>, order: &mut Vec<&'a str>) {
        if order.contains(&module) {
            return;
        }
        for dep in deps.get(module).into_iter().flatten() {
            visit(dep, deps, order);
        }
        order.push(module);
    }
    // Drivers built into the kernel need no module.
    let builtin: Vec<String> = files
        .iter()
        .filter(|(path, _)| path.ends_with("/modules.builtin"))
        .flat_map(|(_, text)| {
            String::from_utf8_lossy(text)
                .lines()
                .map(&stem)
                .collect::<Vec<_>>()
        })
        .collect();
    let mut order = Vec::new();
    for name in wanted {
        let name = name.replace('-', "_");
        match deps.keys().find(|path| stem(path) == name) {
            Some(module) => visit(module, &deps, &mut order),
            None if builtin.contains(&name) => {}
            None => return Err(format!("no module {name} in the initrd")),
        }
    }

    order
        .into_iter()
        .map(|module| {
            let path = format!("{module_dir}{module}");
            let (_, data) = files
                .iter()
                .find(|(file, _)| *file == path)
                .ok_or_else(|| format!("{path} is listed in modules.dep but missing"))?;
            let data = if module.ends_with(".zst") {
                unzstd(data)?
            } else {
                data.clone()
            };
            Ok((format!("{}.ko", stem(module)), data))
        })
        .collect()
}
