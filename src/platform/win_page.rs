//! Working out which web page a browser window is showing.
//!
//! A program's window cannot move to another computer, but for a browser
//! the next best thing is easy: open the same page over there. Browsers do
//! not offer the address of a tab being dragged, so it is read from the
//! address bar through the accessibility interface that screen readers use.

use crate::transfer::safe_url;
use std::ffi::c_void;
use std::sync::mpsc;
use std::time::Duration;
use windows::core::BSTR;
use windows::Win32::System::Variant::{VARIANT, VT_I4};
use windows::Win32::Foundation::HWND as UiaHwnd;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, TreeScope_Descendants, UIA_ControlTypePropertyId, UIA_EditControlTypeId, UIA_ValueValuePropertyId,
};
use windows_sys::Win32::Foundation::{CloseHandle, HWND, LPARAM};
use windows_sys::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetGUIThreadInfo, GetWindowThreadProcessId, IsWindowVisible, GUITHREADINFO,
};

const BROWSERS: [&str; 7] = ["chrome.exe", "msedge.exe", "firefox.exe", "brave.exe", "vivaldi.exe", "opera.exe", "arc.exe"];
const GUI_INMOVESIZE: u32 = 0x2;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

/// True if the window belongs to a web browser we know how to read.
fn is_browser(window: HWND) -> bool {
    unsafe {
        let mut process = 0u32;
        GetWindowThreadProcessId(window, &mut process);
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process);
        if handle.is_null() {
            return false;
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len) != 0;
        CloseHandle(handle);
        if !ok {
            return false;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]).to_lowercase();
        let name = path.rsplit('\\').next().unwrap_or("");
        BROWSERS.contains(&name)
    }
}

/// What the address bar says, as a full address. Browsers hide "https://",
/// and the bar may hold a half-typed search instead, which is not a page.
fn as_address(shown: &str) -> Option<String> {
    let shown = shown.trim();
    if let Some(url) = safe_url(shown) {
        return Some(url);
    }
    let host = shown.split(['/', '?', '#']).next().unwrap_or("");
    let plausible = !shown.contains("://") && host.contains('.') && !host.starts_with('.') && !host.ends_with('.');
    plausible.then(|| safe_url(&format!("https://{shown}"))).flatten()
}

fn read_address_bar(window: isize) -> Option<String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let automation: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok()?;
        let root = automation.ElementFromHandle(UiaHwnd(window as *mut c_void)).ok()?;
        // The address bar is the first text box in every browser we know.
        let mut text_box = VARIANT::default();
        (*text_box.Anonymous.Anonymous).vt = VT_I4;
        (*text_box.Anonymous.Anonymous).Anonymous.lVal = UIA_EditControlTypeId.0;
        let is_text_box = automation.CreatePropertyCondition(UIA_ControlTypePropertyId, &text_box).ok()?;
        let bar = root.FindFirst(TreeScope_Descendants, &is_text_box).ok()?;
        let value = bar.GetCurrentPropertyValue(UIA_ValueValuePropertyId).ok()?;
        as_address(&BSTR::try_from(&value).ok()?.to_string())
    }
}

/// The page a browser window shows, or `None` if it is not a browser, has
/// no page, or does not answer quickly.
pub fn page_of(window: HWND) -> Option<String> {
    if window.is_null() || !is_browser(window) {
        return None;
    }
    // Asked on a separate thread with a time limit: the browser is busy
    // being moved and must not be able to hold us up.
    let (answer, wait) = mpsc::channel();
    let window = window as isize;
    std::thread::spawn(move || {
        let _ = answer.send(read_address_bar(window));
    });
    wait.recv_timeout(Duration::from_millis(800)).ok().flatten()
}

/// True if the user is moving a browser window by its title bar right now.
/// Quick: it does not ask the browser anything.
pub fn browser_being_moved() -> bool {
    unsafe {
        let mut info: GUITHREADINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
        GetGUIThreadInfo(0, &mut info) != 0 && info.flags & GUI_INMOVESIZE != 0 && is_browser(info.hwndMoveSize)
    }
}

/// If the user is moving a browser window by its title bar right now, the
/// page it shows.
pub fn page_being_moved() -> Option<String> {
    unsafe {
        let mut info: GUITHREADINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
        // Thread 0 means "whichever thread has the user's attention".
        if GetGUIThreadInfo(0, &mut info) == 0 || info.flags & GUI_INMOVESIZE == 0 {
            return None;
        }
        page_of(info.hwndMoveSize)
    }
}

unsafe extern "system" fn collect(window: HWND, list: LPARAM) -> i32 {
    if IsWindowVisible(window) != 0 {
        (*(list as *mut Vec<isize>)).push(window as isize);
    }
    1
}

/// For `doctor`: how many open browser windows there are and whether the
/// page of one could be read. Only reads; nothing is opened or changed.
pub fn survey() -> (usize, Option<String>) {
    let mut windows: Vec<isize> = Vec::new();
    unsafe {
        EnumWindows(Some(collect), &mut windows as *mut Vec<isize> as LPARAM);
    }
    let browsers: Vec<HWND> = windows.into_iter().map(|w| w as HWND).filter(|w| is_browser(*w)).collect();
    let page = browsers.iter().find_map(|w| page_of(*w));
    (browsers.len(), page)
}

#[cfg(test)]
mod tests {
    use super::as_address;

    #[test]
    fn address_bar_text_becomes_an_address_or_nothing() {
        assert_eq!(as_address("https://example.com/a").as_deref(), Some("https://example.com/a"));
        assert_eq!(as_address("example.com/docs?page=2").as_deref(), Some("https://example.com/docs?page=2"));
        assert_eq!(as_address("  news.example.org ").as_deref(), Some("https://news.example.org"));
        // Searches, internal pages and local files are not pages to reopen.
        for not_a_page in ["how to tie a tie", "", "chrome://settings", "edge://newtab", "file:///C:/x.html", "about:blank", "localhost", ".com", "a b.com"] {
            assert_eq!(as_address(not_a_page), None, "{not_a_page:?}");
        }
    }
}
