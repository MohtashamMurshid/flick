//! Recycle only. The shell must never offer or silently perform a permanent
//! delete when a volume, policy, or size limit prevents recycling.
use std::{
    ffi::OsString,
    os::windows::ffi::OsStrExt,
    path::{Component, Path, Prefix},
    sync::{Arc, Mutex},
};
use windows::{
    Win32::{
        Foundation::E_ABORT,
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
            CoUninitialize,
        },
        UI::Shell::{
            FILEOPERATION_FLAGS, FOF_NO_CONNECTED_ELEMENTS, FOF_NO_UI, FOF_NORECURSION,
            FOFX_EARLYFAILURE, FOFX_RECYCLEONDELETE, FileOperation, IFileOperation,
            IFileOperationProgressSink, IFileOperationProgressSink_Impl, IShellItem,
            SHCreateItemFromParsingName, TSF_DELETE_RECYCLE_IF_POSSIBLE,
        },
    },
    core::{Error, HRESULT, PCWSTR, Ref, Result, implement},
};

const FLAGS: FILEOPERATION_FLAGS = FILEOPERATION_FLAGS(
    FOF_NO_UI.0
        | FOFX_RECYCLEONDELETE.0
        | FOFX_EARLYFAILURE.0
        | FOF_NO_CONNECTED_ELEMENTS.0
        | FOF_NORECURSION.0,
);
const CANNOT_RECYCLE: &str = "Windows could not confirm recycling this file. Flick did not request a permanent-delete fallback. Check the original folder and Recycle Bin.";
const DELETED_WITHOUT_RECYCLE: &str =
    "Windows reported that this file was permanently deleted instead of moved to the Recycle Bin.";

struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

pub(super) fn recycle(path: &Path) -> std::result::Result<(), String> {
    // This worker has no COM initialization elsewhere. Fail rather than use an
    // incompatible apartment. S_OK and S_FALSE both require CoUninitialize.
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
        .ok()
        .map_err(|e| e.to_string())?;
    let _apartment = Apartment;
    let path = path.canonicalize().map_err(|e| e.to_string())?;
    let parsing_name = shell_path(&path)?;
    let outcome = Arc::new(Mutex::new(None));
    let sink: IFileOperationProgressSink = RecycleOnly {
        outcome: outcome.clone(),
    }
    .into();
    unsafe {
        let operation: IFileOperation =
            CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| e.to_string())?;
        // FOFX_RECYCLEONDELETE explicitly requests the Recycle Bin (Windows8+).
        // Deliberately omit FOF_WANTNUKEWARNING: no permanent-delete prompt.
        // https://learn.microsoft.com/windows/win32/api/shobjidl_core/nf-shobjidl_core-ifileoperation-setoperationflags
        operation
            .SetOperationFlags(FLAGS)
            .map_err(|e| e.to_string())?;
        let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(parsing_name.as_ptr()), None)
            .map_err(|e| e.to_string())?;
        operation
            .DeleteItem(&item, &sink)
            .map_err(|e| e.to_string())?;
        let performed = operation.PerformOperations();
        // A successful PerformOperations does not imply every item succeeded.
        let aborted = operation
            .GetAnyOperationsAborted()
            .map_err(|e| e.to_string())?
            .as_bool();
        let result = outcome.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(Err(error)) = result {
            return Err(error);
        }
        performed.map_err(|e| e.to_string())?;
        if aborted || result.is_none() {
            return Err(CANNOT_RECYCLE.into());
        }
    }
    Ok(())
}

fn shell_path(path: &Path) -> std::result::Result<Vec<u16>, String> {
    // canonicalize() emits verbatim paths, which the shell does not accept.
    // Convert only disk/UNC prefixes; reject device namespaces.
    let mut components = path.components();
    let mut output = OsString::new();
    match components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::VerbatimDisk(letter) | Prefix::Disk(letter) => {
                output.push(format!("{}:", letter as char))
            }
            Prefix::VerbatimUNC(host, share) | Prefix::UNC(host, share) => {
                output.push(r"\\");
                output.push(host);
                output.push(r"\");
                output.push(share);
            }
            _ => return Err("This Windows path cannot be recycled safely.".into()),
        },
        _ => return Err("An absolute Windows file path is required.".into()),
    }
    for component in components {
        match component {
            Component::RootDir => output.push(r"\"),
            Component::Normal(name) => {
                if !output.as_encoded_bytes().ends_with(b"\\") {
                    output.push(r"\");
                }
                output.push(name);
            }
            _ => return Err("This Windows path cannot be recycled safely.".into()),
        }
    }
    Ok(output.encode_wide().chain(Some(0)).collect())
}

#[implement(IFileOperationProgressSink)]
struct RecycleOnly {
    outcome: Arc<Mutex<Option<std::result::Result<(), String>>>>,
}

#[allow(non_snake_case)]
impl IFileOperationProgressSink_Impl for RecycleOnly_Impl {
    fn StartOperations(&self) -> Result<()> {
        Ok(())
    }
    fn FinishOperations(&self, result: HRESULT) -> Result<()> {
        if let Err(error) = result.ok() {
            *self.outcome.lock().unwrap_or_else(|e| e.into_inner()) = Some(Err(error.to_string()));
        }
        Ok(())
    }
    fn PreDeleteItem(&self, flags: u32, _: Ref<'_, IShellItem>) -> Result<()> {
        // Windows reports the actual per-item transfer flags here. Refuse any
        // item that the shell has changed from recycling to a plain delete.
        // https://learn.microsoft.com/windows/win32/api/shobjidl_core/nf-shobjidl_core-ifileoperationprogresssink-predeleteitem
        if flags & TSF_DELETE_RECYCLE_IF_POSSIBLE.0 as u32 == 0 {
            *self.outcome.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(Err(CANNOT_RECYCLE.into()));
            return Err(Error::from_hresult(E_ABORT));
        }
        Ok(())
    }
    fn PostDeleteItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        result: HRESULT,
        recycled: Ref<'_, IShellItem>,
    ) -> Result<()> {
        // The returned item must identify the Recycle Bin destination.
        // https://learn.microsoft.com/windows/win32/api/shobjidl_core/nf-shobjidl_core-ifileoperationprogresssink-postdeleteitem
        let result = result.ok().map_err(|e| e.to_string()).and_then(|()| {
            if recycled.is_some() {
                Ok(())
            } else {
                Err(DELETED_WITHOUT_RECYCLE.into())
            }
        });
        *self.outcome.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
        Ok(())
    }
    fn PreRenameItem(&self, _: u32, _: Ref<'_, IShellItem>, _: &PCWSTR) -> Result<()> {
        Err(Error::from_hresult(E_ABORT))
    }
    fn PostRenameItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
        _: HRESULT,
        _: Ref<'_, IShellItem>,
    ) -> Result<()> {
        Ok(())
    }
    fn PreMoveItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
    ) -> Result<()> {
        Err(Error::from_hresult(E_ABORT))
    }
    fn PostMoveItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
        _: HRESULT,
        _: Ref<'_, IShellItem>,
    ) -> Result<()> {
        Ok(())
    }
    fn PreCopyItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
    ) -> Result<()> {
        Err(Error::from_hresult(E_ABORT))
    }
    fn PostCopyItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
        _: HRESULT,
        _: Ref<'_, IShellItem>,
    ) -> Result<()> {
        Ok(())
    }
    fn PreNewItem(&self, _: u32, _: Ref<'_, IShellItem>, _: &PCWSTR) -> Result<()> {
        Err(Error::from_hresult(E_ABORT))
    }
    fn PostNewItem(
        &self,
        _: u32,
        _: Ref<'_, IShellItem>,
        _: &PCWSTR,
        _: &PCWSTR,
        _: u32,
        _: HRESULT,
        _: Ref<'_, IShellItem>,
    ) -> Result<()> {
        Ok(())
    }
    fn UpdateProgress(&self, _: u32, _: u32) -> Result<()> {
        Ok(())
    }
    fn ResetTimer(&self) -> Result<()> {
        Ok(())
    }
    fn PauseTimer(&self) -> Result<()> {
        Ok(())
    }
    fn ResumeTimer(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Shell::FOF_WANTNUKEWARNING;

    #[test]
    fn flags_request_recycle_and_never_offer_permanent_delete() {
        assert_ne!(FLAGS.0 & FOFX_RECYCLEONDELETE.0, 0);
        assert_eq!(FLAGS.0 & FOF_WANTNUKEWARNING.0, 0);
        assert_ne!(FLAGS.0 & FOF_NO_CONNECTED_ELEMENTS.0, 0);
    }

    #[test]
    fn shell_paths_preserve_disk_unc_and_non_ascii_names() {
        for (source, expected) in [
            (r"\\?\C:\generated\日本語.mp4", r"C:\generated\日本語.mp4"),
            (
                r"\\?\UNC\server\share\generated.mp4",
                r"\\server\share\generated.mp4",
            ),
        ] {
            let wide = shell_path(Path::new(source)).unwrap();
            assert_eq!(wide.last(), Some(&0));
            assert_eq!(
                String::from_utf16(&wide[..wide.len() - 1]).unwrap(),
                expected
            );
        }
        assert!(shell_path(Path::new(r"\\.\PhysicalDrive0")).is_err());
    }

    #[test]
    fn sink_refuses_plain_delete_before_any_file_operation() {
        let outcome = Arc::new(Mutex::new(None));
        let sink: IFileOperationProgressSink = RecycleOnly {
            outcome: outcome.clone(),
        }
        .into();
        assert!(unsafe { sink.PreDeleteItem(0, None) }.is_err());
        assert!(outcome.lock().unwrap().as_ref().unwrap().is_err());
        assert!(
            unsafe { sink.PreDeleteItem(TSF_DELETE_RECYCLE_IF_POSSIBLE.0 as u32, None) }.is_ok()
        );
    }
}
