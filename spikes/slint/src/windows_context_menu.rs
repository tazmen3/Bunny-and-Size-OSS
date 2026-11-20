//! Windows Shell menu for a folder contents or duplicate result row.
mod operations;

pub(super) use operations::Target;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{
    cell::RefCell,
    mem::size_of,
    os::windows::ffi::OsStrExt,
    path::{Component, Path, Prefix},
    ptr,
};
use windows::{
    Win32::{
        Foundation::{E_FAIL, E_INVALIDARG, HWND, LPARAM, LRESULT, POINT, WPARAM},
        Globalization::{CSTR_EQUAL, CompareStringOrdinal},
        Graphics::Gdi::ClientToScreen,
        System::{
            Com::CoTaskMemFree,
            Ole::{OleInitialize, OleUninitialize},
            SystemServices::SFGAO_FOLDER,
        },
        UI::{
            Shell::{
                CMF_CANRENAME, CMF_EXTENDEDVERBS, CMF_ITEMMENU, CMINVOKECOMMANDINFO,
                Common::ITEMIDLIST, DefSubclassProc, GCS_VERBW, IContextMenu, IContextMenu2,
                IContextMenu3, IShellFolder, RemoveWindowSubclass, SEE_MASK_FLAG_NO_UI,
                SEE_MASK_IDLIST, SEE_MASK_NOASYNC, SHBindToParent, SHELLEXECUTEINFOW,
                SHGetNameFromIDList, SHOpenFolderAndSelectItems, SHParseDisplayName,
                SIGDN_FILESYSPATH, SetWindowSubclass, ShellExecuteExW,
            },
            WindowsAndMessaging::{
                CreatePopupMenu, DeleteMenu, DestroyMenu, GetMenuItemCount, GetMenuItemInfoW,
                HMENU, IDOK, InsertMenuW, MB_DEFBUTTON2, MB_ICONWARNING, MB_OKCANCEL,
                MENUITEMINFOW, MF_BYPOSITION, MF_STRING, MFS_DISABLED, MFT_SEPARATOR, MIIM_FTYPE,
                MIIM_ID, MIIM_STATE, MIIM_SUBMENU, MessageBoxW, SW_SHOWNORMAL, TPM_RETURNCMD,
                TPM_RIGHTBUTTON, TrackPopupMenuEx, WM_DRAWITEM, WM_INITMENUPOPUP, WM_MEASUREITEM,
                WM_MENUCHAR,
            },
        },
    },
    core::{HSTRING, Interface, PCSTR, PCWSTR, PSTR, Result, w},
};

const FIRST_COMMAND: u32 = 1;
const LAST_COMMAND: u32 = 0x7fff;
const EXPLORER_COMMAND: u32 = LAST_COMMAND + 1;

pub(super) enum ExplorerAction {
    OpenFolder(slint::SharedString),
    RevealFile(slint::SharedString),
}

impl ExplorerAction {
    fn menu_label(&self, has_explore: bool) -> Option<&str> {
        match self {
            Self::RevealFile(label) => Some(label),
            Self::OpenFolder(label) if !has_explore => Some(label),
            Self::OpenFolder(_) => None,
        }
    }

    fn invoke(&self, hwnd: HWND, item: &ShellAllocation<ITEMIDLIST>) -> Result<()> {
        // SAFETY: the caller holds OLE initialized on the UI thread and the
        // absolute PIDL outlives both calls.
        unsafe {
            match self {
                Self::OpenFolder(_) => ShellExecuteExW(&mut SHELLEXECUTEINFOW {
                    cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
                    fMask: SEE_MASK_IDLIST | SEE_MASK_FLAG_NO_UI | SEE_MASK_NOASYNC,
                    hwnd,
                    lpVerb: w!("open"),
                    lpIDList: item.0.cast(),
                    nShow: SW_SHOWNORMAL.0,
                    ..Default::default()
                }),
                // With no child array, the Shell opens the parent and selects
                // the item identified by the absolute PIDL.
                Self::RevealFile(_) => SHOpenFolderAndSelectItems(item.0, None, 0),
            }
        }
    }
}

struct Apartment;

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: created after a successful OleInitialize on this thread.
        unsafe { OleUninitialize() };
    }
}

struct ShellAllocation<T>(*mut T);

impl<T> Drop for ShellAllocation<T> {
    fn drop(&mut self) {
        // SAFETY: the Shell returned this allocation from the COM allocator.
        unsafe { CoTaskMemFree(Some(self.0.cast())) };
    }
}

struct Popup(HMENU);

impl Drop for Popup {
    fn drop(&mut self) {
        // SAFETY: owns a popup returned by CreatePopupMenu.
        let _ = unsafe { DestroyMenu(self.0) };
    }
}

pub(super) enum Outcome {
    Unchanged,
    Rename(Target),
    Recycled { recorded: bool },
}

pub(super) fn show(
    window: &slint::Window,
    path: &Path,
    position: (f32, f32),
    failure: &str,
    recycle_file: &str,
    recycle_folder: &str,
    explorer: Option<&ExplorerAction>,
) -> Outcome {
    let owner = window.window_handle();
    let Ok(handle) = owner.window_handle() else {
        return Outcome::Unchanged;
    };
    let RawWindowHandle::Win32(native) = handle.as_raw() else {
        return Outcome::Unchanged;
    };
    let hwnd = HWND(native.hwnd.get() as _);
    let scale = window.scale_factor();
    let point = POINT {
        x: (position.0 * scale).round() as i32,
        y: (position.1 * scale).round() as i32,
    };
    match track(hwnd, path, point, recycle_file, recycle_folder, explorer) {
        Ok(outcome) => outcome,
        Err(_) => {
            // SAFETY: the owning window is borrowed and both strings are terminated.
            unsafe {
                MessageBoxW(
                    Some(hwnd),
                    &HSTRING::from(failure),
                    w!("Bunny and Size"),
                    MB_ICONWARNING,
                );
            }
            Outcome::Unchanged
        }
    }
}

fn track(
    hwnd: HWND,
    path: &Path,
    mut point: POINT,
    recycle_file: &str,
    recycle_folder: &str,
    explorer: Option<&ExplorerAction>,
) -> Result<Outcome> {
    let wide = shell_path(path)?;

    // SAFETY: all calls run on the window's thread. The native path and output
    // pointers outlive their calls. The child PIDL borrows the absolute PIDL,
    // which outlives the Shell interfaces. Drop order releases OLE last.
    unsafe {
        OleInitialize(None)?;
        let _apartment = Apartment;
        ClientToScreen(hwnd, &mut point).ok()?;
        let item = shell_item(path, &wide)?;
        let target = Target::capture(path).ok();
        let mut child = ptr::null_mut();
        let folder: IShellFolder = SHBindToParent(item.0, Some(&mut child))?;
        let context: IContextMenu = folder.GetUIObjectOf(hwnd, &[child], None)?;
        let menu = Popup(CreatePopupMenu()?);
        let commands = context.QueryContextMenu(
            menu.0,
            0,
            FIRST_COMMAND,
            LAST_COMMAND,
            CMF_ITEMMENU | CMF_CANRENAME | CMF_EXTENDEDVERBS,
        );
        commands.ok()?;
        let last = LAST_COMMAND.min(commands.0 as u32 & 0xffff);
        let handlers = MenuMessages::new(&context);
        let (allowed, open_with) =
            retain_allowed_commands(menu.0, &context, last, target.is_some(), handlers.is_some())?;
        let has_explore = allowed
            .iter()
            .any(|(_, action)| matches!(action, Action::Explore));
        let mut custom_explorer = None;
        if let Some(explorer) = explorer
            && let Some(label) = explorer.menu_label(has_explore)
        {
            InsertMenuW(
                menu.0,
                0,
                MF_BYPOSITION | MF_STRING,
                EXPLORER_COMMAND as usize,
                &HSTRING::from(label),
            )?;
            custom_explorer = Some(explorer);
        }
        if allowed.is_empty() && open_with.is_none() && custom_explorer.is_none() {
            return Err(E_FAIL.into());
        }

        // Return an ID instead of dispatching WM_COMMAND. Menu notifications
        // still go to the temporary subclass so the Shell can fill its submenu.
        let hook = handlers
            .map(|handlers| MenuHook::install(hwnd, handlers))
            .transpose()?;
        let command = TrackPopupMenuEx(
            menu.0,
            (TPM_RETURNCMD | TPM_RIGHTBUTTON).0,
            point.x,
            point.y,
            hwnd,
            None,
        )
        .0 as u32;
        drop(hook);
        if command == EXPLORER_COMMAND
            && let Some(explorer) = custom_explorer
        {
            if matches!(explorer, ExplorerAction::OpenFolder(_)) {
                let mut attributes = SFGAO_FOLDER.0;
                folder.GetAttributesOf(&[child], &mut attributes)?;
                if attributes & SFGAO_FOLDER.0 == 0 {
                    return Err(E_INVALIDARG.into());
                }
            }
            explorer.invoke(hwnd, &item)?;
            return Ok(Outcome::Unchanged);
        }
        let action = allowed
            .iter()
            .find_map(|&(id, action)| (id == command).then_some(action));
        match action {
            Some(Action::Rename) => return Ok(Outcome::Rename(target.ok_or(E_FAIL)?)),
            Some(Action::Recycle) => {
                let target = target.ok_or(E_FAIL)?;
                let preview = if target.is_directory() {
                    recycle_folder
                } else {
                    recycle_file
                };
                let preview = format!("{preview}\n\n{}", crate::workshop::label(path));
                if MessageBoxW(
                    Some(hwnd),
                    &HSTRING::from(preview),
                    w!("Bunny and Size"),
                    MB_OKCANCEL | MB_DEFBUTTON2 | MB_ICONWARNING,
                ) == IDOK
                {
                    let recorded = target.recycle()?;
                    return Ok(Outcome::Recycled { recorded });
                }
            }
            Some(Action::Shell | Action::Explore) => invoke(&context, hwnd, command)?,
            None if (FIRST_COMMAND..=last).contains(&command) => {
                if let Some(submenu) = open_with
                    && contains_leaf(submenu, command)?
                {
                    invoke(&context, hwnd, command)?;
                }
            }
            None => {}
        }
    }
    Ok(Outcome::Unchanged)
}

fn invoke(context: &IContextMenu, hwnd: HWND, command: u32) -> Result<()> {
    let invocation = CMINVOKECOMMANDINFO {
        cbSize: size_of::<CMINVOKECOMMANDINFO>() as u32,
        hwnd,
        // MAKEINTRESOURCEA encodes the offset into lpVerb.
        lpVerb: PCSTR(ptr::without_provenance((command - FIRST_COMMAND) as usize)),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: command was verified against the active Shell menu and all
    // invocation fields are valid for its lifetime.
    unsafe { context.InvokeCommand(&invocation) }
}

struct MenuMessages {
    v2: Option<IContextMenu2>,
    v3: Option<IContextMenu3>,
}

impl MenuMessages {
    fn new(context: &IContextMenu) -> Option<Self> {
        let v3 = context.cast().ok();
        let v2 = context.cast().ok();
        (v2.is_some() || v3.is_some()).then_some(Self { v2, v3 })
    }
}

thread_local! {
    static MENU_MESSAGES: RefCell<Option<MenuMessages>> = const { RefCell::new(None) };
}

struct MenuHook(HWND);

impl MenuHook {
    fn install(hwnd: HWND, handlers: MenuMessages) -> Result<Self> {
        MENU_MESSAGES.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.is_some() {
                return Err(E_FAIL.into());
            }
            // SAFETY: hwnd belongs to this UI thread. The callback has static
            // lifetime and its optional context is cleared by this guard.
            unsafe { SetWindowSubclass(hwnd, Some(menu_proc), 0, 0).ok()? };
            *slot = Some(handlers);
            Ok(Self(hwnd))
        })
    }
}

impl Drop for MenuHook {
    fn drop(&mut self) {
        // SAFETY: same window, callback, ID and UI thread as the installation.
        let _ = unsafe { RemoveWindowSubclass(self.0, Some(menu_proc), 0) };
        MENU_MESSAGES.with(|slot| slot.take());
    }
}

unsafe extern "system" fn menu_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if matches!(message, WM_INITMENUPOPUP | WM_MENUCHAR)
        || matches!(message, WM_DRAWITEM | WM_MEASUREITEM) && wparam.0 == 0
    {
        // Release the TLS borrow before COM can re-enter this callback.
        let handlers = MENU_MESSAGES.with(|slot| {
            slot.borrow()
                .as_ref()
                .map(|handler| (handler.v3.clone(), handler.v2.clone()))
        });
        if let Some((v3, v2)) = handlers {
            let mut result = LRESULT(0);
            // SAFETY: pass the unmodified menu message and its Win32 payload
            // to the interface responsible for this menu.
            unsafe {
                if let Some(v3) = v3
                    && v3
                        .HandleMenuMsg2(message, wparam, lparam, Some(&mut result))
                        .is_ok()
                {
                    return result;
                }
                if message != WM_MENUCHAR
                    && let Some(v2) = v2
                    && v2.HandleMenuMsg(message, wparam, lparam).is_ok()
                {
                    return LRESULT(isize::from(message != WM_INITMENUPOPUP));
                }
            }
        }
    }
    // SAFETY: forward the original window message to the remaining subclass chain.
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

fn shell_path(path: &Path) -> Result<Vec<u16>> {
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return Err(E_INVALIDARG.into());
    };
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if !path.is_absolute() || wide.contains(&0) {
        return Err(E_INVALIDARG.into());
    }
    match prefix.kind() {
        Prefix::VerbatimDisk(_) => drop(wide.drain(..4)),
        Prefix::VerbatimUNC(_, _) => {
            wide.splice(..8, [u16::from(b'\\'); 2]);
        }
        Prefix::Disk(_) | Prefix::UNC(_, _) => {
            for unit in &mut wide {
                if *unit == u16::from(b'/') {
                    *unit = u16::from(b'\\');
                }
            }
        }
        _ => return Err(E_INVALIDARG.into()),
    }
    wide.push(0);
    Ok(wide)
}

fn shell_item(path: &Path, wide: &[u16]) -> Result<ShellAllocation<ITEMIDLIST>> {
    // SAFETY: shell_path supplies a terminated buffer. The Shell allocates both
    // successful outputs and the returned file-system name is terminated.
    unsafe {
        let mut pidl = ptr::null_mut();
        SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut pidl, 0, None)?;
        let item = ShellAllocation(pidl);
        let resolved = ShellAllocation(SHGetNameFromIDList(item.0, SIGDN_FILESYSPATH)?.0);
        let name = PCWSTR(resolved.0);
        let resolved = name.as_wide();
        let input = &wide[..wide.len() - 1];
        // Reject lossy normalization, such as trailing dots in a verbatim
        // name. For a difference in case, consult the filesystem to confirm
        // which spelling the original path names, even on case-sensitive NTFS.
        if resolved != input
            && (CompareStringOrdinal(resolved, input, true) != CSTR_EQUAL
                || !path
                    .canonicalize()
                    .ok()
                    .and_then(|canonical| shell_path(&canonical).ok())
                    .is_some_and(|canonical| canonical[..canonical.len() - 1] == *resolved))
        {
            return Err(E_INVALIDARG.into());
        }
        Ok(item)
    }
}

#[derive(Clone, Copy)]
enum Action {
    Shell,
    Explore,
    Rename,
    Recycle,
}

fn menu_item(menu: HMENU, position: u32) -> Result<MENUITEMINFOW> {
    let mut item = MENUITEMINFOW {
        cbSize: size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_ID | MIIM_FTYPE | MIIM_STATE | MIIM_SUBMENU,
        ..Default::default()
    };
    // SAFETY: the menu is alive and the item is sized and writable.
    unsafe { GetMenuItemInfoW(menu, position, true, &mut item)? };
    Ok(item)
}

fn menu_count(menu: HMENU) -> Result<u32> {
    // SAFETY: the menu is alive; a negative count indicates failure.
    u32::try_from(unsafe { GetMenuItemCount(Some(menu)) }).map_err(|_| E_FAIL.into())
}

type MenuSelection = (Vec<(u32, Action)>, Option<HMENU>);

fn retain_allowed_commands(
    menu: HMENU,
    context: &IContextMenu,
    last: u32,
    can_mutate: bool,
    has_handlers: bool,
) -> Result<MenuSelection> {
    let mut allowed = Vec::new();
    let mut open_with = None;
    for position in (0..menu_count(menu)?).rev() {
        let item = menu_item(menu, position)?;
        let verb = ((FIRST_COMMAND..=last).contains(&item.wID)
            && (item.fType & MFT_SEPARATOR).0 == 0)
            .then(|| command_verb(context, item.wID - FIRST_COMMAND))
            .flatten();
        if verb.as_deref() == Some("openas")
            && !item.hSubMenu.is_invalid()
            && has_handlers
            && open_with.is_none()
        {
            open_with = Some(item.hSubMenu);
            continue;
        }
        let action = match verb.as_deref() {
            Some("explore") if (item.fState & MFS_DISABLED).0 == 0 => Some(Action::Explore),
            Some(
                "open" | "opennewwindow" | "properties" | "copy" | "copyaspath" | "cut" | "openas",
            ) => Some(Action::Shell),
            Some("rename") if can_mutate => Some(Action::Rename),
            Some("delete") if can_mutate => Some(Action::Recycle),
            _ => None,
        };
        if let Some(action) = action
            && item.hSubMenu.is_invalid()
        {
            if (item.fState & MFS_DISABLED).0 == 0 {
                allowed.push((item.wID, action));
            }
        } else {
            // SAFETY: this position is still valid as we remove in reverse order.
            unsafe { DeleteMenu(menu, position, MF_BYPOSITION)? };
        }
    }
    Ok((allowed, open_with))
}

fn contains_leaf(menu: HMENU, command: u32) -> Result<bool> {
    // The Shell creates the application IDs lazily and does not give them
    // canonical verbs. Accept a leaf only from the retained openas submenu.
    for position in 0..menu_count(menu)? {
        let item = menu_item(menu, position)?;
        if item.wID == command
            && item.hSubMenu.is_invalid()
            && (item.fType & MFT_SEPARATOR).0 == 0
            && (item.fState & MFS_DISABLED).0 == 0
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn command_verb(context: &IContextMenu, offset: u32) -> Option<String> {
    let mut verb = [0xffffu16; 128];
    // SAFETY: GCS_VERBW writes up to cchMax UTF-16 code units to this buffer.
    unsafe {
        context.GetCommandString(
            offset as usize,
            GCS_VERBW,
            None,
            PSTR(verb.as_mut_ptr().cast()),
            verb.len() as u32,
        )
    }
    .ok()?;
    let end = verb.iter().position(|&c| c == 0)?;
    Some(String::from_utf16(&verb[..end]).ok()?.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_folder_command_only_fills_the_gap_for_an_absent_shell_command() {
        let open = ExplorerAction::OpenFolder("Open folder".into());
        let reveal = ExplorerAction::RevealFile("Reveal file".into());
        assert_eq!(open.menu_label(false), Some("Open folder"));
        assert_eq!(open.menu_label(true), None);
        assert_eq!(reveal.menu_label(true), Some("Reveal file"));
    }

    #[test]
    fn shell_resolves_native_unicode_paths_and_refuses_lossy_aliases() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("Note été.txt");
        std::fs::write(&file, "test").unwrap();
        let verbatim = file.canonicalize().unwrap();
        // SAFETY: the test thread owns its OLE initialization and balances it.
        unsafe { OleInitialize(None) }.unwrap();
        let _apartment = Apartment;
        for path in [&file, &verbatim] {
            assert!(shell_item(path, &shell_path(path).unwrap()).is_ok());
        }
        let ambiguous = verbatim.with_file_name("Note été.txt.");
        assert!(shell_item(&ambiguous, &shell_path(&ambiguous).unwrap()).is_err());
    }
}
