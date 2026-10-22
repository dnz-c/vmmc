//! Windows' own file and folder pickers.

use std::path::PathBuf;

/// Asks for a file matching `pattern` (like `*.iso`), described as `kind`.
#[cfg(windows)]
pub fn pick_file(title: &str, kind: &str, pattern: &str) -> Option<PathBuf> {
    show(title, Some((kind, pattern)))
}

/// Asks for a folder.
#[cfg(windows)]
pub fn pick_folder(title: &str) -> Option<PathBuf> {
    show(title, None)
}

#[cfg(windows)]
fn show(title: &str, filter: Option<(&str, &str)>) -> Option<PathBuf> {
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
        CoTaskMemFree,
    };
    use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
    use windows::Win32::UI::Shell::{
        FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH,
    };
    use windows::core::{HSTRING, PCWSTR};

    // SAFETY: COM calls on the UI thread; every string outlives the calls that use it,
    // and the returned path is freed with CoTaskMemFree.
    unsafe {
        // Already initialized (by the window) is fine.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let dialog: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        dialog.SetTitle(&HSTRING::from(title)).ok()?;
        let names: Vec<HSTRING>;
        match filter {
            Some((kind, pattern)) => {
                names = vec![
                    HSTRING::from(kind),
                    HSTRING::from(pattern),
                    HSTRING::from("All files"),
                    HSTRING::from("*.*"),
                ];
                let specs = [
                    COMDLG_FILTERSPEC {
                        pszName: PCWSTR(names[0].as_ptr()),
                        pszSpec: PCWSTR(names[1].as_ptr()),
                    },
                    COMDLG_FILTERSPEC {
                        pszName: PCWSTR(names[2].as_ptr()),
                        pszSpec: PCWSTR(names[3].as_ptr()),
                    },
                ];
                dialog.SetFileTypes(&specs).ok()?;
            }
            None => {
                let options = dialog.GetOptions().ok()?;
                dialog.SetOptions(options | FOS_PICKFOLDERS).ok()?;
            }
        }
        // An error here is the user cancelling.
        dialog.Show(None).ok()?;
        let item = dialog.GetResult().ok()?;
        let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = name.to_string().ok();
        CoTaskMemFree(Some(name.0.cast()));
        path.map(PathBuf::from)
    }
}

/// Asks where to save a file of `kind` (`pattern`, like `*.ova`), suggesting `name`.
#[cfg(windows)]
pub fn pick_save(title: &str, kind: &str, pattern: &str, name: &str) -> Option<PathBuf> {
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
        CoTaskMemFree,
    };
    use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
    use windows::Win32::UI::Shell::{FileSaveDialog, IFileSaveDialog, SIGDN_FILESYSPATH};
    use windows::core::{HSTRING, PCWSTR};

    // SAFETY: COM calls on the UI thread; every string outlives the calls that use it,
    // and the returned path is freed with CoTaskMemFree.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let dialog: IFileSaveDialog =
            CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        dialog.SetTitle(&HSTRING::from(title)).ok()?;
        let (kind, pattern) = (HSTRING::from(kind), HSTRING::from(pattern));
        let specs = [COMDLG_FILTERSPEC {
            pszName: PCWSTR(kind.as_ptr()),
            pszSpec: PCWSTR(pattern.as_ptr()),
        }];
        dialog.SetFileTypes(&specs).ok()?;
        let extension = pattern.to_string().trim_start_matches("*.").to_owned();
        dialog.SetDefaultExtension(&HSTRING::from(extension)).ok()?;
        dialog.SetFileName(&HSTRING::from(name)).ok()?;
        // An error here is the user cancelling.
        dialog.Show(None).ok()?;
        let item = dialog.GetResult().ok()?;
        let path = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let text = path.to_string().ok();
        CoTaskMemFree(Some(path.0.cast()));
        text.map(PathBuf::from)
    }
}

#[cfg(not(windows))]
pub fn pick_file(_: &str, _: &str, _: &str) -> Option<PathBuf> {
    None
}

#[cfg(not(windows))]
pub fn pick_save(_: &str, _: &str, _: &str, _: &str) -> Option<PathBuf> {
    None
}

#[cfg(not(windows))]
pub fn pick_folder(_: &str) -> Option<PathBuf> {
    None
}
