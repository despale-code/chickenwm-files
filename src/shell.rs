//! Windows shell integration.
//!
//! File operations go through `SHFileOperationW`, so they get the Recycle Bin, Windows' own
//! conflict ("Replace or Skip") and progress dialogs, and Explorer-compatible undo. The file
//! clipboard is the system one (`CF_HDROP` + `Preferred DropEffect`), so copy/paste works
//! between chickenwm-files, Explorer and other apps. Also: OLE drag-out to other apps, the native
//! shell context menu ("Show more options"), Open with, Properties and shortcuts.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use windows::core::{implement, Interface, HRESULT, HSTRING, PCSTR, PCWSTR};
use windows::Win32::Foundation::{BOOL, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, IBindCtx, IDataObject, IPersistFile, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{
    DoDragDrop, IDropSource, IDropSource_Impl, CF_HDROP, DROPEFFECT, DROPEFFECT_COPY,
    DROPEFFECT_LINK, DROPEFFECT_MOVE,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MK_RBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    DefSubclassProc, DragQueryFileW, ILFindLastID, ILFree, IContextMenu, IContextMenu2,
    IContextMenu3, IShellFolder, IShellLinkW, RemoveWindowSubclass, SHBindToParent,
    SHFileOperationW, SHMultiFileProperties, SHObjectProperties, SHOpenWithDialog,
    SHParseDisplayName, SetWindowSubclass, ShellLink, CMF_NORMAL,
    CMINVOKECOMMANDINFO, DROPFILES, FOF_ALLOWUNDO, FOF_NOCONFIRMMKDIR, FOF_RENAMEONCOLLISION,
    FOF_WANTNUKEWARNING, FO_COPY, FO_DELETE, FO_MOVE, HDROP, OAIF_ALLOW_REGISTRATION, OAIF_EXEC,
    OAIF_REGISTER_EXT, OPENASINFO, SHOP_FILEPATH,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, DestroyMenu, GetCursorPos, TrackPopupMenuEx, SW_SHOWNORMAL, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, WM_DRAWITEM, WM_INITMENUPOPUP, WM_MEASUREITEM, WM_MENUCHAR,
};

const DRAGDROP_S_DROP: HRESULT = HRESULT(0x0004_0100);
const DRAGDROP_S_CANCEL: HRESULT = HRESULT(0x0004_0101);
const DRAGDROP_S_USEDEFAULTCURSORS: HRESULT = HRESULT(0x0004_0102);

pub fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Double-NUL-terminated path list, as `SHFileOperationW` and `DROPFILES` want it.
fn wide_list(paths: &[PathBuf]) -> Vec<u16> {
    let mut out = Vec::new();
    for p in paths {
        out.extend(wide(p.as_os_str()));
    }
    out.push(0);
    out
}

/// Worker threads that touch COM-based shell APIs call this first.
pub fn init_com_thread() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOp {
    Copy,
    Move,
    /// To the Recycle Bin (Windows warns if an item is too large to recycle).
    Recycle,
    /// Permanently, after Windows' own confirmation.
    Delete,
}

/// Runs a shell file operation. Blocks until the user has dealt with any prompts, so call it
/// from a worker thread. Returns false if it failed or the user cancelled.
pub fn file_op(op: FileOp, sources: &[PathBuf], dest: Option<&Path>) -> bool {
    if sources.is_empty() {
        return true;
    }
    let from = wide_list(sources);
    let to = dest.map(|d| wide_list(&[d.to_path_buf()]));
    let same_dir = dest.is_some_and(|d| sources.iter().all(|s| s.parent() == Some(d)));
    let (func, flags) = match op {
        FileOp::Copy => (
            FO_COPY,
            FOF_ALLOWUNDO.0 | FOF_NOCONFIRMMKDIR.0 | if same_dir { FOF_RENAMEONCOLLISION.0 } else { 0 },
        ),
        FileOp::Move => (FO_MOVE, FOF_ALLOWUNDO.0 | FOF_NOCONFIRMMKDIR.0),
        FileOp::Recycle => (FO_DELETE, FOF_ALLOWUNDO.0 | FOF_WANTNUKEWARNING.0),
        FileOp::Delete => (FO_DELETE, 0),
    };
    let mut op_struct = windows::Win32::UI::Shell::SHFILEOPSTRUCTW {
        hwnd: HWND::default(),
        wFunc: func,
        pFrom: PCWSTR(from.as_ptr()),
        pTo: to.as_ref().map_or(PCWSTR::null(), |t| PCWSTR(t.as_ptr())),
        fFlags: flags as u16,
        ..Default::default()
    };
    let rc = unsafe { SHFileOperationW(&mut op_struct) };
    rc == 0 && !op_struct.fAnyOperationsAborted.as_bool()
}

fn open_clipboard() -> bool {
    for _ in 0..10 {
        if unsafe { OpenClipboard(HWND::default()) }.is_ok() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    false
}

fn preferred_drop_effect_format() -> u32 {
    unsafe { RegisterClipboardFormatW(windows::core::w!("Preferred DropEffect")) }
}

fn global_from_bytes(bytes: &[u8]) -> Option<HGLOBAL> {
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).ok()?;
        let ptr = GlobalLock(h) as *mut u8;
        if ptr.is_null() {
            return None;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
        let _ = GlobalUnlock(h);
        Some(h)
    }
}

/// Puts files on the system clipboard exactly like Explorer's Copy / Cut.
pub fn clipboard_set_files(paths: &[PathBuf], cut: bool) -> bool {
    if paths.is_empty() || !open_clipboard() {
        return false;
    }
    let header = DROPFILES {
        pFiles: std::mem::size_of::<DROPFILES>() as u32,
        fWide: BOOL(1),
        ..Default::default()
    };
    let list = wide_list(paths);
    let mut bytes = Vec::with_capacity(std::mem::size_of::<DROPFILES>() + list.len() * 2);
    bytes.extend_from_slice(unsafe {
        std::slice::from_raw_parts(&header as *const _ as *const u8, std::mem::size_of::<DROPFILES>())
    });
    for u in list {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    let effect = if cut { DROPEFFECT_MOVE.0 } else { DROPEFFECT_COPY.0 | DROPEFFECT_LINK.0 };
    let ok = unsafe {
        let _ = EmptyClipboard();
        let files_ok = global_from_bytes(&bytes)
            .is_some_and(|h| SetClipboardData(CF_HDROP.0 as u32, HANDLE(h.0)).is_ok());
        if let Some(h) = global_from_bytes(&effect.to_le_bytes()) {
            let _ = SetClipboardData(preferred_drop_effect_format(), HANDLE(h.0));
        }
        files_ok
    };
    unsafe {
        let _ = CloseClipboard();
    }
    ok
}

/// Files currently on the system clipboard and whether they were cut (vs copied).
pub fn clipboard_get_files() -> Option<(Vec<PathBuf>, bool)> {
    unsafe {
        if IsClipboardFormatAvailable(CF_HDROP.0 as u32).is_err() || !open_clipboard() {
            return None;
        }
    }
    let result = unsafe {
        (|| {
            let h = GetClipboardData(CF_HDROP.0 as u32).ok()?;
            let hdrop = HDROP(h.0);
            let count = DragQueryFileW(hdrop, u32::MAX, None);
            let mut files = Vec::with_capacity(count as usize);
            for i in 0..count {
                let len = DragQueryFileW(hdrop, i, None) as usize;
                let mut buf = vec![0u16; len + 1];
                let got = DragQueryFileW(hdrop, i, Some(&mut buf)) as usize;
                files.push(PathBuf::from(String::from_utf16_lossy(&buf[..got])));
            }
            let mut cut = false;
            if let Ok(h) = GetClipboardData(preferred_drop_effect_format()) {
                let ptr = GlobalLock(HGLOBAL(h.0)) as *const u32;
                if !ptr.is_null() {
                    cut = (*ptr & DROPEFFECT_MOVE.0) != 0;
                    let _ = GlobalUnlock(HGLOBAL(h.0));
                }
            }
            Some((files, cut))
        })()
    };
    unsafe {
        let _ = CloseClipboard();
    }
    result
}

/// After pasting cut files, Explorer clears the clipboard so they can't be "moved" twice.
pub fn clipboard_clear() {
    if open_clipboard() {
        unsafe {
            let _ = EmptyClipboard();
            let _ = CloseClipboard();
        }
    }
}

/// Builds a shell UI object (`IContextMenu`, `IDataObject`) for items that share one folder.
fn ui_object<T: Interface>(hwnd: HWND, paths: &[PathBuf]) -> Option<T> {
    let mut pidls: Vec<*mut ITEMIDLIST> = Vec::new();
    let result = (|| unsafe {
        for p in paths {
            let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
            SHParseDisplayName(&HSTRING::from(p.as_os_str()), None::<&IBindCtx>, &mut pidl, 0, None).ok()?;
            pidls.push(pidl);
        }
        let first = *pidls.first()?;
        let folder: IShellFolder = SHBindToParent(first, None).ok()?;
        let children: Vec<*const ITEMIDLIST> =
            pidls.iter().map(|&p| ILFindLastID(p) as *const ITEMIDLIST).collect();
        folder.GetUIObjectOf::<_, T>(hwnd, &children, None).ok()
    })();
    for p in pidls {
        unsafe { ILFree(Some(p)) };
    }
    result
}

#[implement(IDropSource)]
struct DropSource {
    button: MODIFIERKEYS_FLAGS,
}

impl IDropSource_Impl for DropSource_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        if escape.as_bool() {
            DRAGDROP_S_CANCEL
        } else if (keys.0 & self.button.0) == 0 {
            DRAGDROP_S_DROP
        } else {
            HRESULT(0)
        }
    }

    fn GiveFeedback(&self, _effect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// Hands a drag that left the window to Windows, so the files can be dropped into Explorer,
/// the desktop, a mail client, etc. Blocks until dropped. Returns true if something was dropped.
pub fn drag_out(hwnd: HWND, paths: &[PathBuf], right_button: bool) -> bool {
    let Some(data) = ui_object::<IDataObject>(hwnd, paths) else {
        return false;
    };
    let source: IDropSource = DropSource {
        button: if right_button { MK_RBUTTON } else { MK_LBUTTON },
    }
    .into();
    unsafe {
        let _ = ReleaseCapture();
        let mut effect = DROPEFFECT(0);
        DoDragDrop(&data, &source, DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK, &mut effect)
            == DRAGDROP_S_DROP
    }
}

thread_local! {
    /// The native menu being shown; its owner-drawn submenus ("Send to", "Open with", "New")
    /// are filled in by forwarding menu messages to it.
    static ACTIVE_MENU: RefCell<Option<IContextMenu>> = const { RefCell::new(None) };
}

unsafe extern "system" fn menu_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if matches!(msg, WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR) {
        let handled = ACTIVE_MENU.with(|m| {
            let menu = m.borrow();
            let menu = menu.as_ref()?;
            if let Ok(cm3) = menu.cast::<IContextMenu3>() {
                let mut result = LRESULT(0);
                unsafe { cm3.HandleMenuMsg2(msg, wparam, lparam, Some(&mut result)) }.ok()?;
                return Some(result);
            }
            let cm2 = menu.cast::<IContextMenu2>().ok()?;
            unsafe { cm2.HandleMenuMsg(msg, wparam, lparam) }.ok()?;
            Some(LRESULT(0))
        });
        if let Some(result) = handled {
            return result;
        }
    }
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

/// The full Windows context menu for the items (7-Zip, Git, Send to, ...), at the cursor.
/// Returns true if a command was run.
pub fn native_context_menu(hwnd: HWND, paths: &[PathBuf]) -> bool {
    let Some(menu) = ui_object::<IContextMenu>(hwnd, paths) else {
        return false;
    };
    unsafe {
        let Ok(hmenu) = CreatePopupMenu() else {
            return false;
        };
        if menu.QueryContextMenu(hmenu, 0, 1, 0x7FFF, CMF_NORMAL).is_err() {
            let _ = DestroyMenu(hmenu);
            return false;
        }
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        ACTIVE_MENU.with(|m| *m.borrow_mut() = Some(menu.clone()));
        let subclassed = SetWindowSubclass(hwnd, Some(menu_subclass_proc), 0x4859_5052, 0).as_bool();
        let cmd = TrackPopupMenuEx(hmenu, (TPM_RETURNCMD | TPM_RIGHTBUTTON).0, pt.x, pt.y, hwnd, None);
        if subclassed {
            let _ = RemoveWindowSubclass(hwnd, Some(menu_subclass_proc), 0x4859_5052);
        }
        ACTIVE_MENU.with(|m| *m.borrow_mut() = None);
        let mut ran = false;
        if cmd.0 > 0 {
            let info = CMINVOKECOMMANDINFO {
                cbSize: std::mem::size_of::<CMINVOKECOMMANDINFO>() as u32,
                hwnd,
                // MAKEINTRESOURCE(cmd - idCmdFirst)
                lpVerb: PCSTR((cmd.0 - 1) as usize as *const u8),
                nShow: SW_SHOWNORMAL.0,
                ..Default::default()
            };
            ran = menu.InvokeCommand(&info).is_ok();
        }
        let _ = DestroyMenu(hmenu);
        ran
    }
}

pub fn shell_open(_hwnd: HWND, path: &Path, verb: Option<&str>) {
    chickenwm_core::elevation::shell_open_unelevated(path, None, verb);
}

pub fn open_with(hwnd: HWND, path: &Path) {
    let file = HSTRING::from(path.as_os_str());
    let info = OPENASINFO {
        pcszFile: PCWSTR(file.as_ptr()),
        pcszClass: PCWSTR::null(),
        oaifInFlags: OAIF_ALLOW_REGISTRATION | OAIF_REGISTER_EXT | OAIF_EXEC,
    };
    unsafe {
        let _ = SHOpenWithDialog(hwnd, &info);
    }
}

pub fn properties(hwnd: HWND, paths: &[PathBuf]) {
    match paths {
        [] => {}
        [one] => unsafe {
            let _ = SHObjectProperties(hwnd, SHOP_FILEPATH, &HSTRING::from(one.as_os_str()), PCWSTR::null());
        },
        many => {
            if let Some(data) = ui_object::<IDataObject>(hwnd, many) {
                unsafe {
                    let _ = SHMultiFileProperties(&data, 0);
                }
            }
        }
    }
}

/// Creates `link` (a `.lnk` path) pointing at `target`.
pub fn create_shortcut(target: &Path, link: &Path) -> windows::core::Result<()> {
    unsafe {
        let shell_link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        shell_link.SetPath(&HSTRING::from(target.as_os_str()))?;
        if let Some(dir) = target.parent() {
            shell_link.SetWorkingDirectory(&HSTRING::from(dir.as_os_str()))?;
        }
        shell_link.cast::<IPersistFile>()?.Save(&HSTRING::from(link.as_os_str()), true)
    }
}

/// `(free, total)` bytes of the volume containing `path`.
pub fn disk_space(path: &Path) -> Option<(u64, u64)> {
    let (mut free, mut total) = (0u64, 0u64);
    unsafe {
        GetDiskFreeSpaceExW(&HSTRING::from(path.as_os_str()), Some(&mut free), Some(&mut total), None).ok()?;
    }
    Some((free, total))
}

/// Volume label + kind for the sidebar ("Local Disk", "USB Drive", ...).
pub fn drive_label(root: &Path) -> String {
    use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumeInformationW};
    let root_w = HSTRING::from(root.as_os_str());
    let mut name = [0u16; 128];
    let label = unsafe {
        GetVolumeInformationW(&root_w, Some(&mut name), None, None, None, None)
            .ok()
            .map(|_| {
                let len = name.iter().position(|&c| c == 0).unwrap_or(0);
                String::from_utf16_lossy(&name[..len])
            })
            .filter(|s| !s.is_empty())
    };
    label.unwrap_or_else(|| {
        match unsafe { GetDriveTypeW(&root_w) } {
            2 => "USB Drive",
            4 => "Network Drive",
            5 => "CD Drive",
            _ => "Local Disk",
        }
        .to_string()
    })
}
