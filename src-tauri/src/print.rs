//! Silent receipt printing for the bundled till (DigitGlance Trade, window
//! `till`): two commands that hand ESC/POS bytes to a printer installed in
//! Windows through its driver, with no print dialog.
//!
//! - `printers_list`: the printers installed in Windows (local and connected),
//!   with their port and which one is the Windows default.
//! - `print_raw`: base64 ESC/POS to a named printer as one RAW spooler job
//!   (OpenPrinter, StartDocPrinter with datatype RAW, StartPagePrinter,
//!   WritePrinter until every byte is written, EndPagePrinter, EndDocPrinter,
//!   ClosePrinter on every path). The spooler and the driver do the rest; the
//!   till never sees the dialog.
//!
//! The till's side is apps/till/src/lib/desktop-print-bridge.ts in the web
//! monorepo (window.DigitGlancePrint, method `driver`). Both commands are
//! granted by capabilities/till.json to the `till` window only: the remote web
//! origin (the `backoffice` window) has no IPC and cannot print this way.
//!
//! Errors are `{ code, message }` like the store's (store/error.rs), with the
//! printing codes `printer_not_found`, `printer_offline`, `access_denied`,
//! `spooler`, `io`, `too_large`, `invalid` and `unsupported` (not Windows).
//! The Win32 message text rides in `message`.
//!
//! Nothing here has been run against a physical printer: the Win32 calls
//! compile against windows-sys, and the parts that need no printer (the
//! base64 and size guard, the error mapping, the job name) are unit tested.

use base64::Engine;
use serde::Serialize;

/// A receipt over this size is not a receipt: refuse rather than spool it.
pub const MAX_BASE64_LEN: usize = 2 * 1024 * 1024;

/// The job name in the Windows print queue when the till sends none.
pub const DEFAULT_JOB_NAME: &str = "DigitGlance receipt";

/// The one error shape every command returns: `{ code, message }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrintError {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for PrintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for PrintError {}

impl PrintError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        PrintError {
            code,
            message: message.into(),
        }
    }
}

pub type Result<T> = std::result::Result<T, PrintError>;

/// One printer installed in Windows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrinterInfo {
    pub name: String,
    pub port: Option<String>,
    pub is_default: bool,
}

/// What `print_raw` answers: the bytes the spooler accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RawPrintResult {
    pub sent: usize,
}

// --------------------------------------------------------------- pure parts

/// The ESC/POS bytes out of the till's base64, held to the size limit.
pub fn decode_payload(bytes: &str) -> Result<Vec<u8>> {
    if bytes.len() > MAX_BASE64_LEN {
        return Err(PrintError::new(
            "too_large",
            format!(
                "The receipt is too large to print ({} KB of data; the limit is {} KB).",
                bytes.len() / 1024,
                MAX_BASE64_LEN / 1024
            ),
        ));
    }
    let trimmed = bytes.trim();
    if trimmed.is_empty() {
        return Err(PrintError::new("invalid", "There is nothing to print."));
    }
    let data = base64::engine::general_purpose::STANDARD
        .decode(trimmed)
        .map_err(|e| PrintError::new("invalid", format!("The print data is not valid base64: {e}")))?;
    if data.is_empty() {
        return Err(PrintError::new("invalid", "There is nothing to print."));
    }
    Ok(data)
}

/// The name the job shows in the Windows print queue: the till's, or the default, one line, not too long.
pub fn job_name(name: Option<String>) -> String {
    let cleaned: String = name
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(120)
        .collect();
    if cleaned.is_empty() {
        DEFAULT_JOB_NAME.to_string()
    } else {
        cleaned
    }
}

// Win32 error codes the mapping knows (WinError.h). Kept as plain numbers so
// the mapping is testable on every platform.
pub const ERROR_ACCESS_DENIED: u32 = 5;
pub const ERROR_NOT_READY: u32 = 21;
pub const ERROR_PRINTQ_FULL: u32 = 61;
pub const ERROR_PRINT_CANCELLED: u32 = 63;
pub const ERROR_SERVICE_NOT_ACTIVE: u32 = 1062;
pub const ERROR_DEVICE_NOT_CONNECTED: u32 = 1167;
pub const RPC_S_SERVER_UNAVAILABLE: u32 = 1722;
pub const RPC_S_SERVER_TOO_BUSY: u32 = 1723;
pub const RPC_S_CALL_FAILED: u32 = 1726;
pub const RPC_S_CALL_FAILED_DNE: u32 = 1727;
pub const EPT_S_NOT_REGISTERED: u32 = 1753;
pub const ERROR_INVALID_PRINTER_NAME: u32 = 1801;
pub const ERROR_UNKNOWN_PORT: u32 = 1796;
pub const ERROR_SPL_NO_STARTDOC: u32 = 3003;
pub const ERROR_PRINTER_NOT_FOUND: u32 = 3012;
pub const ERROR_PRINTER_DRIVER_PACKAGE_IN_USE: u32 = 3013;

/// The till's error code for a Win32 error.
pub fn map_win32(code: u32) -> &'static str {
    match code {
        ERROR_INVALID_PRINTER_NAME | ERROR_PRINTER_NOT_FOUND | ERROR_UNKNOWN_PORT => "printer_not_found",
        ERROR_ACCESS_DENIED => "access_denied",
        RPC_S_SERVER_UNAVAILABLE
        | RPC_S_SERVER_TOO_BUSY
        | RPC_S_CALL_FAILED
        | RPC_S_CALL_FAILED_DNE
        | EPT_S_NOT_REGISTERED
        | ERROR_SERVICE_NOT_ACTIVE
        | ERROR_SPL_NO_STARTDOC
        | ERROR_PRINTQ_FULL => "spooler",
        ERROR_NOT_READY | ERROR_DEVICE_NOT_CONNECTED => "printer_offline",
        _ => "io",
    }
}

/// The sentence for a Win32 failure, in the till's words first and Windows' words after.
pub fn describe_win32(code: u32, printer: &str, what: &str, win32_text: &str) -> PrintError {
    let mapped = map_win32(code);
    let message = match mapped {
        "printer_not_found" => format!("No printer called \"{printer}\" is installed in Windows. Pick the printer again in Printer settings."),
        "access_denied" => format!("Windows did not allow this app to use the printer \"{printer}\". Check the printer's permissions in Windows."),
        "spooler" => format!("The Windows print service (Print Spooler) did not take the job. Check the service is running, then try again. Windows said: {win32_text} (error {code})."),
        "printer_offline" => format!("The printer \"{printer}\" is offline or not connected. Check it is on and plugged in, then try again."),
        _ => format!("{what}: {win32_text} (Windows error {code})."),
    };
    PrintError::new(mapped, message)
}

// --------------------------------------------------------------- commands

/// The printers installed in Windows, with their port and the Windows default.
#[tauri::command]
pub async fn printers_list() -> Result<Vec<PrinterInfo>> {
    run_blocking(platform::list).await
}

/// Base64 ESC/POS bytes to the named printer as one RAW spooler job, no dialog.
#[tauri::command]
pub async fn print_raw(printer: String, bytes: String, job_name: Option<String>) -> Result<RawPrintResult> {
    let printer = printer.trim().to_string();
    if printer.is_empty() {
        return Err(PrintError::new("invalid", "Choose a printer first."));
    }
    let data = decode_payload(&bytes)?;
    let name = self::job_name(job_name);
    run_blocking(move || platform::print_raw(&printer, &name, &data)).await
}

/// Spooler calls block; keep them off the async runtime.
async fn run_blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    match tauri::async_runtime::spawn_blocking(f).await {
        Ok(r) => r,
        Err(e) => Err(PrintError::new("io", format!("The print task did not finish: {e}"))),
    }
}

// --------------------------------------------------------------- Windows

#[cfg(windows)]
mod platform {
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Graphics::Printing::{
        ClosePrinter, EndDocPrinter, EndPagePrinter, EnumPrintersW, GetDefaultPrinterW, OpenPrinterW, StartDocPrinterW,
        StartPagePrinter, WritePrinter, DOC_INFO_1W, PRINTER_ENUM_CONNECTIONS, PRINTER_ENUM_LOCAL, PRINTER_HANDLE,
        PRINTER_INFO_2W,
    };
    use windows_sys::Win32::System::Diagnostics::Debug::{FormatMessageW, FORMAT_MESSAGE_FROM_SYSTEM, FORMAT_MESSAGE_IGNORE_INSERTS};

    use super::{describe_win32, PrintError, PrinterInfo, RawPrintResult, Result};

    const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// A NUL-terminated UTF-16 string the spooler owns, or None for a null pointer.
    unsafe fn from_pwstr(p: *const u16) -> Option<String> {
        if p.is_null() {
            return None;
        }
        let mut len = 0usize;
        while *p.add(len) != 0 {
            len += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
        let s = s.trim();
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    }

    /// Windows' own text for an error code, one line.
    fn win32_text(code: u32) -> String {
        let mut buf = [0u16; 512];
        let n = unsafe {
            FormatMessageW(
                FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
                null(),
                code,
                0,
                buf.as_mut_ptr(),
                buf.len() as u32,
                null(),
            )
        };
        let text = String::from_utf16_lossy(&buf[..n as usize]);
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let text = text.trim_end_matches('.').to_string();
        if text.is_empty() {
            "Unknown error".to_string()
        } else {
            text
        }
    }

    fn last_error(printer: &str, what: &str) -> PrintError {
        let code = unsafe { GetLastError() };
        describe_win32(code, printer, what, &win32_text(code))
    }

    /// The Windows default printer's name, or None when there is none.
    fn default_printer() -> Option<String> {
        let mut len = 0u32;
        unsafe { GetDefaultPrinterW(null_mut(), &mut len) };
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u16; len as usize];
        if unsafe { GetDefaultPrinterW(buf.as_mut_ptr(), &mut len) } == 0 {
            return None;
        }
        unsafe { from_pwstr(buf.as_ptr()) }
    }

    pub fn list() -> Result<Vec<PrinterInfo>> {
        const WHAT: &str = "Could not list the printers installed in Windows";
        let flags = PRINTER_ENUM_LOCAL | PRINTER_ENUM_CONNECTIONS;
        let mut needed = 0u32;
        let mut returned = 0u32;
        // First call: the size. It fails with ERROR_INSUFFICIENT_BUFFER by design.
        let ok = unsafe { EnumPrintersW(flags, null(), 2, null_mut(), 0, &mut needed, &mut returned) };
        if ok == 0 {
            let code = unsafe { GetLastError() };
            if code != ERROR_INSUFFICIENT_BUFFER {
                return Err(describe_win32(code, "", WHAT, &win32_text(code)));
            }
        }
        if needed == 0 {
            return Ok(Vec::new());
        }
        // PRINTER_INFO_2W holds pointers: give the buffer pointer alignment.
        let words = (needed as usize).div_ceil(std::mem::size_of::<u64>());
        let mut buf = vec![0u64; words];
        let ok = unsafe {
            EnumPrintersW(flags, null(), 2, buf.as_mut_ptr() as *mut u8, needed, &mut needed, &mut returned)
        };
        if ok == 0 {
            return Err(last_error("", WHAT));
        }
        let default = default_printer();
        let infos = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const PRINTER_INFO_2W, returned as usize) };
        let mut out: Vec<PrinterInfo> = infos
            .iter()
            .filter_map(|p| {
                let name = unsafe { from_pwstr(p.pPrinterName) }?;
                let is_default = default.as_deref() == Some(name.as_str());
                Some(PrinterInfo {
                    port: unsafe { from_pwstr(p.pPortName) },
                    name,
                    is_default,
                })
            })
            .collect();
        // The Windows default first, the rest as Windows lists them.
        out.sort_by_key(|p| !p.is_default);
        Ok(out)
    }

    pub fn print_raw(printer: &str, job: &str, data: &[u8]) -> Result<RawPrintResult> {
        let name = wide(printer);
        let mut handle = PRINTER_HANDLE::default();
        if unsafe { OpenPrinterW(name.as_ptr(), &mut handle, null()) } == 0 {
            return Err(last_error(printer, "Could not open the printer"));
        }
        // Everything from here closes the handle on the way out, whatever happened.
        let result = send_document(handle, printer, job, data);
        unsafe { ClosePrinter(handle) };
        result
    }

    fn send_document(handle: PRINTER_HANDLE, printer: &str, job: &str, data: &[u8]) -> Result<RawPrintResult> {
        let mut doc_name = wide(job);
        let mut datatype = wide("RAW");
        let info = DOC_INFO_1W {
            pDocName: doc_name.as_mut_ptr(),
            pOutputFile: null_mut(),
            pDatatype: datatype.as_mut_ptr(),
        };
        if unsafe { StartDocPrinterW(handle, 1, &info) } == 0 {
            return Err(last_error(printer, "Could not start the print job"));
        }
        let written = send_page(handle, printer, data);
        // EndDocPrinter always runs so the spooler never keeps a half-open job.
        let ended = unsafe { EndDocPrinter(handle) } != 0;
        let sent = written?;
        if !ended {
            return Err(last_error(printer, "Could not finish the print job"));
        }
        Ok(RawPrintResult { sent })
    }

    fn send_page(handle: PRINTER_HANDLE, printer: &str, data: &[u8]) -> Result<usize> {
        if unsafe { StartPagePrinter(handle) } == 0 {
            return Err(last_error(printer, "Could not start the page"));
        }
        let mut sent = 0usize;
        let outcome = loop {
            if sent >= data.len() {
                break Ok(());
            }
            let chunk = &data[sent..];
            let len = chunk.len().min(u32::MAX as usize) as u32;
            let mut written = 0u32;
            if unsafe { WritePrinter(handle, chunk.as_ptr() as *const core::ffi::c_void, len, &mut written) } == 0 {
                break Err(last_error(printer, "The printer stopped taking data"));
            }
            if written == 0 {
                break Err(PrintError::new(
                    "io",
                    format!("The printer \"{printer}\" accepted no more data after {sent} bytes."),
                ));
            }
            sent += written as usize;
        };
        unsafe { EndPagePrinter(handle) };
        outcome.map(|_| sent)
    }
}

// --------------------------------------------------------------- elsewhere

#[cfg(not(windows))]
mod platform {
    use super::{PrintError, PrinterInfo, RawPrintResult, Result};

    const MESSAGE: &str = "Printing to an installed printer is available on Windows only. Use system print here.";

    pub fn list() -> Result<Vec<PrinterInfo>> {
        Err(PrintError::new("unsupported", MESSAGE))
    }

    pub fn print_raw(_printer: &str, _job: &str, _data: &[u8]) -> Result<RawPrintResult> {
        Err(PrintError::new("unsupported", MESSAGE))
    }
}

// --------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_the_till_base64_and_refuses_junk_and_nothing() {
        assert_eq!(decode_payload("GyFA").unwrap(), vec![0x1b, 0x21, 0x40]);
        assert_eq!(decode_payload("  GyFA\n").unwrap(), vec![0x1b, 0x21, 0x40]);
        assert_eq!(decode_payload("").unwrap_err().code, "invalid");
        assert_eq!(decode_payload("   ").unwrap_err().code, "invalid");
        assert_eq!(decode_payload("not base64!").unwrap_err().code, "invalid");
    }

    #[test]
    fn refuses_a_payload_over_two_megabytes_before_decoding() {
        let big = "A".repeat(MAX_BASE64_LEN + 1);
        let e = decode_payload(&big).unwrap_err();
        assert_eq!(e.code, "too_large");
        assert!(e.message.contains("too large"));
        // Exactly the limit is still decoded (as valid base64).
        let edge = "A".repeat(MAX_BASE64_LEN);
        assert!(decode_payload(&edge).is_ok());
    }

    #[test]
    fn maps_win32_errors_to_the_till_codes() {
        assert_eq!(map_win32(ERROR_INVALID_PRINTER_NAME), "printer_not_found");
        assert_eq!(map_win32(ERROR_PRINTER_NOT_FOUND), "printer_not_found");
        assert_eq!(map_win32(ERROR_ACCESS_DENIED), "access_denied");
        assert_eq!(map_win32(RPC_S_SERVER_UNAVAILABLE), "spooler");
        assert_eq!(map_win32(ERROR_SERVICE_NOT_ACTIVE), "spooler");
        assert_eq!(map_win32(ERROR_SPL_NO_STARTDOC), "spooler");
        assert_eq!(map_win32(ERROR_DEVICE_NOT_CONNECTED), "printer_offline");
        assert_eq!(map_win32(ERROR_NOT_READY), "printer_offline");
        assert_eq!(map_win32(ERROR_PRINT_CANCELLED), "io");
        assert_eq!(map_win32(ERROR_PRINTER_DRIVER_PACKAGE_IN_USE), "io");
        assert_eq!(map_win32(99_999), "io");
    }

    #[test]
    fn describes_a_failure_in_the_tills_words_with_windows_text_kept() {
        let e = describe_win32(ERROR_INVALID_PRINTER_NAME, "EPSON TM-T20III", "Could not open the printer", "The printer name is invalid");
        assert_eq!(e.code, "printer_not_found");
        assert!(e.message.contains("\"EPSON TM-T20III\""));
        let e = describe_win32(RPC_S_SERVER_UNAVAILABLE, "X", "Could not open the printer", "The RPC server is unavailable");
        assert_eq!(e.code, "spooler");
        assert!(e.message.contains("Print Spooler") && e.message.contains("1722"));
        let e = describe_win32(1, "X", "Could not start the page", "Incorrect function");
        assert_eq!(e.code, "io");
        assert_eq!(e.message, "Could not start the page: Incorrect function (Windows error 1).");
        // Serialises as the store's shape.
        assert_eq!(
            serde_json::to_value(&e).unwrap(),
            serde_json::json!({ "code": "io", "message": "Could not start the page: Incorrect function (Windows error 1)." })
        );
    }

    #[test]
    fn job_name_defaults_trims_and_keeps_one_line() {
        assert_eq!(job_name(None), DEFAULT_JOB_NAME);
        assert_eq!(job_name(Some("   ".into())), DEFAULT_JOB_NAME);
        assert_eq!(job_name(Some("  DigitGlance receipt sale-1:1\n".into())), "DigitGlance receipt sale-1:1");
        assert_eq!(job_name(Some("x".repeat(300))).chars().count(), 120);
    }

    #[test]
    fn printer_info_serialises_camel_case() {
        let p = PrinterInfo { name: "P".into(), port: Some("USB001".into()), is_default: true };
        assert_eq!(serde_json::to_value(&p).unwrap(), serde_json::json!({ "name": "P", "port": "USB001", "isDefault": true }));
    }

    #[test]
    fn print_commands_agree_with_build_rs_capability_lib_and_docs() {
        let build = include_str!("../build.rs");
        let till: serde_json::Value = serde_json::from_str(include_str!("../capabilities/till.json")).unwrap();
        let default: serde_json::Value = serde_json::from_str(include_str!("../capabilities/default.json")).unwrap();
        let store: serde_json::Value = serde_json::from_str(include_str!("../capabilities/local-store.json")).unwrap();
        let lib = include_str!("lib.rs");
        let docs = include_str!("../../docs/LOCAL-STORE-API.md");
        assert!(till.get("remote").is_none(), "the remote origin must never be granted printing");
        let perms = |v: &serde_json::Value| -> Vec<String> {
            v["permissions"].as_array().unwrap().iter().filter_map(|p| p.as_str().map(String::from)).collect()
        };
        let (till_perms, default_perms, store_perms) = (perms(&till), perms(&default), perms(&store));
        assert_eq!(till["windows"], serde_json::json!(["till"]));
        for c in ["printers_list", "print_raw"] {
            let perm = format!("allow-{}", c.replace('_', "-"));
            assert!(build.contains(&format!("\"{c}\"")), "{c} missing from build.rs");
            assert!(till_perms.contains(&perm), "{c} missing from capabilities/till.json");
            assert!(!default_perms.contains(&perm), "{c} must not be in default.json");
            assert!(!store_perms.contains(&perm), "{c} must not be in local-store.json");
            assert!(lib.contains(&format!("print::{c},")), "{c} missing from generate_handler!");
            assert!(docs.contains(&format!("`{c}`")), "{c} missing from docs/LOCAL-STORE-API.md");
        }
    }
}
