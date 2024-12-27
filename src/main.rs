use std::env;
use std::fs;
use std::path::Path;
use std::io;
use std::time::{Duration, Instant};
use std::sync::mpsc;
use std::thread;
use widestring::U16CString;
use winapi::ctypes::c_void;
use winapi::shared::ntdef::HRESULT;
use winapi::shared::minwindef::ULONG;
use clap::Parser;
use std::process::Command;

// FFI definitions remain the same
#[link(name = "amsi")]
extern "system" {
    fn AmsiInitialize(appName: LPCWSTR, amsiContext: *mut HAMSICONTEXT) -> HRESULT;
    fn AmsiUninitialize(amsiContext: HAMSICONTEXT);
    fn AmsiOpenSession(amsiContext: HAMSICONTEXT, amsiSession: *mut HAMSISESSION) -> HRESULT;
    fn AmsiCloseSession(amsiContext: HAMSICONTEXT, amsiSession: HAMSISESSION);
    fn AmsiScanBuffer(
        amsiContext: HAMSICONTEXT,
        buffer: LPCVOID,
        length: ULONG,
        contentName: LPCWSTR,
        session: HAMSISESSION,
        result: *mut AMSI_RESULT,
    ) -> HRESULT;
}

// Type definitions remain the same
type HAMSICONTEXT = *mut c_void;
type HAMSISESSION = *mut c_void;
type AMSI_RESULT = i32;
type LPCWSTR = *const u16;
type LPCVOID = *const c_void;

// Constants
const S_OK: i32 = 0;
const AMSI_RESULT_CLEAN: i32 = 0;
const AMSI_RESULT_NOT_DETECTED: i32 = 1;
const AMSI_RESULT_BLOCKED_BY_ADMIN_START: i32 = 16384;
const AMSI_RESULT_BLOCKED_BY_ADMIN_END: i32 = 20479;
const AMSI_RESULT_DETECTED: i32 = 32768;

#[derive(Debug, PartialEq, Clone)]
enum ScanResult {
    NoThreatFound,
    ThreatFound(String),
    FileNotFound,
    Timeout,
    Error(String),
}

#[derive(Clone, Copy)]
struct Progress {
    low: usize,
    high: usize,
    malicious: bool,
}

// Base scanner trait
trait Scanner {
    fn scan(&self, file_path: &str, debug: bool) -> Result<(), String>;
}

// AMSI Scanner implementation
struct AMSIScanner {
    context: HAMSICONTEXT,
}

impl AMSIScanner {
    fn new() -> Result<Self, String> {
        let mut context = std::ptr::null_mut();
        let app_name = U16CString::from_str("RustAMSIScanner").unwrap();
        let result = unsafe { AmsiInitialize(app_name.as_ptr(), &mut context) };
        if result != S_OK {
            return Err("Failed to initialize AMSI".to_string());
        }
        Ok(AMSIScanner { context })
    }

    fn scan_buffer(&self, buffer: &[u8], content_name: &str) -> Result<AMSI_RESULT, String> {
        let mut session = std::ptr::null_mut();
        unsafe { AmsiOpenSession(self.context, &mut session) };

        let content_name = U16CString::from_str(content_name).unwrap();
        let mut result = 0;
        unsafe {
            AmsiScanBuffer(
                self.context,
                buffer.as_ptr() as *const c_void,
                buffer.len() as u32,
                content_name.as_ptr(),
                session,
                &mut result,
            );
            AmsiCloseSession(self.context, session);
        }

        Ok(result)
    }

    fn binary_search(&self, file_path: &str, debug: bool) -> Result<usize, String> {
        let file_content = fs::read(file_path).map_err(|e| e.to_string())?;
        let size = file_content.len();
        if debug {
            println!("Debug: Starting binary search on {}, total size: {} bytes", file_path, size);
        }

        let mut last_good = 0;
        let mut upper_bound = size;
        let mut iteration = 0;

        while upper_bound - last_good > 1 {
            iteration += 1;
            let mid = (last_good + upper_bound) / 2;
            
            if debug {
                println!("Debug: Iteration {}: Scanning range 0x{:X} - 0x{:X} ({} bytes)", 
                         iteration, last_good, mid, mid - last_good);
            }

            let result = self.scan_buffer(&file_content[0..mid], "memory_scan");
            match result {
                Ok(AMSI_RESULT_DETECTED) => {
                    if debug {
                        println!("Debug: Iteration {}: Threat detected, narrowing search to first half", iteration);
                    }
                    upper_bound = mid;
                },
                Ok(_) => {
                    if debug {
                        println!("Debug: Iteration {}: No threat detected, expanding search to second half", iteration);
                    }
                    last_good = mid;
                },
                Err(e) => {
                    if debug {
                        println!("Debug: Iteration {}: Error during scan: {}", iteration, e);
                    }
                    return Err(format!("Scan error at offset 0x{:X}: {}", mid, e));
                }
            }
        }

        Ok(last_good)
    }
}

impl Scanner for AMSIScanner {
    fn scan(&self, file_path: &str, debug: bool) -> Result<(), String> {
        let start_time = Instant::now();
        let original_file = fs::read(file_path).map_err(|e| e.to_string())?;
        let result = self.scan_buffer(&original_file, file_path);

        match result {
            Ok(scan_result) => {
                match scan_result {
                    AMSI_RESULT_CLEAN | AMSI_RESULT_NOT_DETECTED => {
                        println!("No threats found in the file.");
                    },
                    AMSI_RESULT_BLOCKED_BY_ADMIN_START | AMSI_RESULT_BLOCKED_BY_ADMIN_END => {
                        println!("Scan was blocked by admin policy.");
                    },
                    AMSI_RESULT_DETECTED => {
                        println!("\n[*] Threat detected in the file. Starting binary search to isolate malicious content...");
                        let offset = self.binary_search(file_path, debug)?;
                        let end_time = start_time.elapsed();
                        ScanResultPrinter::print_results(file_path, &original_file, offset, end_time);
                    }
                    _ => println!("Unknown scan result: {}", scan_result),
                }
            },
            Err(e) => {
                println!("\n[!] Error during initial scan: {}", e);
                println!("[*] Attempting binary search to isolate potential threat...");
                let offset = self.binary_search(file_path, debug)?;
                let end_time = start_time.elapsed();
                ScanResultPrinter::print_error_results(file_path, &original_file, offset, end_time);
            }
        }

        Ok(())
    }
}

impl Drop for AMSIScanner {
    fn drop(&mut self) {
        unsafe { AmsiUninitialize(self.context) };
    }
}

// Windows Defender Scanner implementation
struct WindowsDefenderScanner {
    tx: mpsc::Sender<Progress>,
}

impl WindowsDefenderScanner {
    fn new(tx: mpsc::Sender<Progress>) -> Self {
        WindowsDefenderScanner { tx }
    }

    fn scan_file(&self, file: &str) -> io::Result<ScanResult> {
        if !Path::new(file).exists() {
            return Ok(ScanResult::FileNotFound);
        }

        let start = Instant::now();
        let output = Command::new(r"C:\Program Files\Windows Defender\MpCmdRun.exe")
            .args(&["-Scan", "-ScanType", "3", "-File", file, "-DisableRemediation", "-Trace", "-Level", "0x10"])
            .output()?;

        if start.elapsed() > Duration::from_secs(30) {
            return Ok(ScanResult::Timeout);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);

        if stdout.contains("CmdTool: Failed with hr = 0x80508023") {
            return Ok(ScanResult::Error("Scan failed. Check MpCmdRun.log for details.".to_string()));
        }

        let threat_name = stdout.lines()
            .find(|line| line.trim().starts_with("Threat"))
            .and_then(|line| line.split_once(':'))
            .map(|(_, sig)| sig.trim().to_string());

        match output.status.code() {
            Some(0) => Ok(ScanResult::NoThreatFound),
            Some(2) => Ok(ScanResult::ThreatFound(threat_name.unwrap_or_else(|| "Unknown".to_string()))),
            _ => Ok(ScanResult::Error(format!("Unexpected exit code: {:?}", output.status.code()))),
        }
    }
}

impl Scanner for WindowsDefenderScanner {
    fn scan(&self, file_path: &str, debug: bool) -> Result<(), String> {
        let start_time = Instant::now();

        // Always print file information
        println!("File Path: {}", file_path);
        if let Ok(file_metadata) = fs::metadata(file_path) {
            println!("File Size: {}", ScanResultPrinter::format_bytes(file_metadata.len() as usize));
        } else {
            println!("File Size: N/A");
        }

        // Original detection logic
        let original_file_detection_status = self.scan_file(file_path).map_err(|e| e.to_string())?;
        match original_file_detection_status {
            ScanResult::NoThreatFound => {
                println!("[+] No threat found in submitted file.");
                return Ok(());
            }
            ScanResult::ThreatFound(threat_name) => {
                println!("Threat found in the original file: {}", threat_name);
                println!("Beginning binary search...");
            }
            ScanResult::Error(err) => {
                println!("[-] Error scanning the original file: {}", err);
                return Ok(());
            }
            _ => {
                println!("[-] Unexpected result when scanning the original file");
                return Ok(());
            }
        }

        let scanner_state = WindowsDefenderScannerState::new(file_path, debug)?;
        let result = scanner_state.perform_binary_search(&self.tx);
        match result {
            Ok((last_good, iteration, original_file_contents)) => {
                let end_time = start_time.elapsed();

                fs::write(&scanner_state.test_file_path, &original_file_contents[0..last_good + 1])
                    .map_err(|e| e.to_string())?;

                let final_threat = if let Ok(ScanResult::ThreatFound(threat_name)) =
                    self.scan_file(&scanner_state.test_file_path.to_string_lossy())
                {
                    Some(threat_name)
                } else {
                    None
                };

                ScanResultPrinter::print_defender_results(
                    file_path,
                    &original_file_contents,
                    last_good,
                    end_time,
                    iteration,
                    final_threat.as_deref(),
                );
            }
            Err(e) => println!("[-] Error during binary search: {}", e),
        }
        scanner_state.cleanup()?;

        Ok(())
    }
}


// Helper struct for Windows Defender scanner state
struct WindowsDefenderScannerState {
    temp_dir: std::path::PathBuf,
    test_file_path: std::path::PathBuf,
    original_file_contents: Vec<u8>,
    debug: bool,
}

impl WindowsDefenderScannerState {
    fn new(file_path: &str, debug: bool) -> Result<Self, String> {
        let temp_dir = env::temp_dir().join("windef_scan").join(format!("{:x}", rand::random::<u64>()));
        fs::create_dir_all(&temp_dir).map_err(|e| e.to_string())?;
        let test_file_path = temp_dir.join("testfile.exe");

        if debug {
            ScanResultPrinter::print_debug(&format!("Created temporary directory: {:?}", temp_dir), debug);
            ScanResultPrinter::print_debug(&format!("Test file path: {:?}", test_file_path), debug);
        }

        let original_file_contents = fs::read(file_path).map_err(|e| e.to_string())?;
        
        Ok(WindowsDefenderScannerState {
            temp_dir,
            test_file_path,
            original_file_contents,
            debug,
        })
    }

    fn perform_binary_search(&self, tx: &mpsc::Sender<Progress>) -> Result<(usize, usize, Vec<u8>), String> {
        let original_file_size = self.original_file_contents.len();
        
        if self.debug {
            ScanResultPrinter::print_debug(&format!("Starting binary search on file size: {} bytes", original_file_size), self.debug);
        }

        let mut last_good = 0;
        let mut upper_bound = original_file_size;
        let mut iteration = 0;

        while upper_bound - last_good > 1 {
            iteration += 1;
            let mid = last_good + (upper_bound - last_good) / 2;

            if self.debug {
                ScanResultPrinter::print_debug_iteration(
                    iteration,
                    last_good,
                    upper_bound,
                    upper_bound - last_good,
                    self.debug
                );
            }

            fs::write(&self.test_file_path, &self.original_file_contents[0..mid])
                .map_err(|e| e.to_string())?;

            let detection_status = WindowsDefenderScanner::new(tx.clone())
                .scan_file(&self.test_file_path.to_string_lossy())
                .map_err(|e| e.to_string())?;

            match detection_status {
                ScanResult::ThreatFound(_) => {
                    if self.debug {
                        ScanResultPrinter::print_debug("Threat detected, narrowing search to first half", self.debug);
                    }
                    upper_bound = mid;
                    tx.send(Progress { low: 0, high: mid, malicious: true }).unwrap();
                }
                ScanResult::NoThreatFound => {
                    if self.debug {
                        ScanResultPrinter::print_debug("No threat detected, expanding search to second half", self.debug);
                    }
                    last_good = mid;
                    tx.send(Progress { low: 0, high: mid, malicious: false }).unwrap();
                }
                _ => {
                    if self.debug {
                        ScanResultPrinter::print_debug("Unknown result, treating as malicious", self.debug);
                    }
                    upper_bound = mid;
                    tx.send(Progress { low: 0, high: mid, malicious: true }).unwrap();
                }
            }
        }

        if self.debug {
            ScanResultPrinter::print_debug(&format!("Binary search completed after {} iterations", iteration), self.debug);
            ScanResultPrinter::print_debug(&format!("Final isolation at offset: 0x{:08X}", last_good), self.debug);
        }

        Ok((last_good, iteration, self.original_file_contents.clone()))
    }

    fn cleanup(&self) -> Result<(), String> {
        if let Err(e) = fs::remove_dir_all(&self.temp_dir) {
            println!("Warning: Failed to remove temporary directory: {}", e);
        } else if self.debug {
            println!("Debug: Removed temporary directory: {:?}", self.temp_dir);
        }
        Ok(())
    }
}

// Helper struct for printing scan results
struct ScanResultPrinter;

impl ScanResultPrinter {
    fn print_header(title: &str) {
        if Args::parse().raw {
            println!("\n{}", title);
            println!("{}", "=".repeat(title.len()));
        } else {
            println!("\n\x1b[1;36m{}\x1b[0m", title);
            println!("\x1b[36m{}\x1b[0m", "=".repeat(title.len()));
        }
    }

    fn print_info(label: &str, value: String) {
        if Args::parse().raw {
            println!("{:<30} {}", label, value);
        } else {
            println!("\x1b[1;37m{:<30}\x1b[0m {}", label, value);
        }
    }

    fn print_warning(message: &str) {
        if Args::parse().raw {
            println!("[!] {}", message);
        } else {
            println!("\x1b[1;33m[!] {}\x1b[0m", message);
        }
    }

    fn print_success(message: &str) {
        if Args::parse().raw {
            println!("[+] {}", message);
        } else {
            println!("\x1b[1;32m[+] {}\x1b[0m", message);
        }
    }

    fn print_error(message: &str) {
        if Args::parse().raw {
            println!("[-] {}", message);
        } else {
            println!("\x1b[1;31m[-] {}\x1b[0m", message);
        }
    }

    fn format_duration(duration: Duration) -> String {
        if duration.as_secs() > 0 {
            format!("{}.{:03}s", duration.as_secs(), duration.subsec_millis())
        } else {
            format!("{}ms", duration.as_millis())
        }
    }

    fn print_debug(message: &str, debug: bool) {
        if debug {
            if Args::parse().raw {
                println!("[DEBUG] {}", message);
            } else {
                println!("\x1b[1;33m[DEBUG]\x1b[0m {}", message);
            }
        }
    }

    fn print_debug_iteration(iteration: usize, last_good: usize, upper_bound: usize, size_diff: usize, debug: bool) {
        if debug {
            if Args::parse().raw {
                println!("[DEBUG] Iteration {:3}: Range 0x{:08X} - 0x{:08X} ({:6} bytes)", 
                    iteration, last_good, upper_bound, size_diff);
            } else {
                println!("\x1b[1;33m[DEBUG]\x1b[0m Iteration {:3}: Range 0x{:08X} - 0x{:08X} ({:6} bytes)", 
                    iteration, last_good, upper_bound, size_diff);
            }
        }
    }

    fn format_bytes(bytes: usize) -> String {
        if bytes >= 1_000_000 {
            format!("{:.2} MB ({} bytes)", bytes as f64 / 1_000_000.0, bytes)
        } else if bytes >= 1_000 {
            format!("{:.2} KB ({} bytes)", bytes as f64 / 1_000.0, bytes)
        } else {
            format!("{} bytes", bytes)
        }
    }

    fn print_results(file_path: &str, original_file: &[u8], offset: usize, duration: Duration) {
        Self::print_header("AMSI Scan Results");
        Self::print_info("File Path:", file_path.to_string());
        Self::print_info("File Size:", Self::format_bytes(original_file.len()));
        Self::print_info("Detection Offset:", format!("0x{:X}", offset));
        Self::print_info("Scan Duration:", Self::format_duration(duration));

        if Args::parse().raw {
            println!("\nHex Dump Analysis");
            println!("{}", "-".repeat(16));
            println!("Showing ±128 bytes around detection point:");
        } else {
            println!("\n\x1b[1;35mHex Dump Analysis\x1b[0m");
            println!("\x1b[35m{}\x1b[0m", "-".repeat(16));
            println!("\x1b[33mShowing ±128 bytes around detection point:\x1b[0m");
        }
        let start = offset.saturating_sub(128);
        let end = (offset + 128).min(original_file.len());
        let threat_data = &original_file[start..end];
        println!("{}", Self::hex_dump(threat_data, 16));
    }

    fn print_error_results(file_path: &str, original_file: &[u8], offset: usize, duration: Duration) {
        Self::print_header("Scan Results (After Error)");
        Self::print_warning("Scan completed with errors");
        Self::print_info("File Path:", file_path.to_string());
        Self::print_info("File Size:", Self::format_bytes(original_file.len()));
        Self::print_info("Potential Detection at:", format!("0x{:X}", offset));
        Self::print_info("Scan Duration:", Self::format_duration(duration));

        if Args::parse().raw {
            println!("\nHex Dump Analysis");
            println!("{}", "-".repeat(16));
            println!("Showing ±64 bytes around potential detection:");
        } else {
            println!("\n\x1b[1;35mHex Dump Analysis\x1b[0m");
            println!("\x1b[35m{}\x1b[0m", "-".repeat(16));
            println!("\x1b[33mShowing ±64 bytes around potential detection:\x1b[0m");
        }
        let start = offset.saturating_sub(64);
        let end = (offset + 64).min(original_file.len());
        let threat_data = &original_file[start..end];
        println!("{}", Self::hex_dump(threat_data, 16));
    }

    fn print_defender_results(file_path: &str, original_file: &[u8], offset: usize, duration: Duration, iterations: usize, threat_name: Option<&str>) {
        Self::print_header("Windows Defender Scan Results");
        Self::print_info("File Path:", file_path.to_string());
        Self::print_info("File Size:", Self::format_bytes(original_file.len()));
        Self::print_info("Scan Duration:", Self::format_duration(duration));
        Self::print_info("Search Iterations:", iterations.to_string());
        Self::print_info("Detection Offset:", format!("0x{:X}", offset));
        Self::print_info("Relative Location:", format!("{} / {} bytes", offset, original_file.len()));

        if let Some(name) = threat_name {
            Self::print_info("Final threat detection:", name.to_string());
        }

        if Args::parse().raw {
            println!("\nHex Dump Analysis");
            println!("{}", "-".repeat(16));
            println!("Showing ±128 bytes around detection point:");
        } else {
            println!("\n\x1b[1;35mHex Dump Analysis\x1b[0m");
            println!("\x1b[35m{}\x1b[0m", "-".repeat(16));
            println!("\x1b[33mShowing ±128 bytes around detection point:\x1b[0m");
        }
        let start = offset.saturating_sub(128);
        let end = (offset + 128).min(original_file.len());
        let threat_data = &original_file[start..end];
        println!("{}", Self::hex_dump(threat_data, 16));
    }

    fn hex_dump(bytes: &[u8], bytes_per_line: usize) -> String {
        let raw = Args::parse().raw;
        let mut result = String::new();
        for (i, chunk) in bytes.chunks(bytes_per_line).enumerate() {
            // Offset
            if raw {
                result.push_str(&format!("{:08x}   ", i * bytes_per_line));
            } else {
                result.push_str(&format!("\x1b[1;37m{:08x}\x1b[0m   ", i * bytes_per_line));
            }
            
            // Hex values
            for (j, byte) in chunk.iter().enumerate() {
                if raw {
                    result.push_str(&format!("{:02X} ", byte));
                } else {
                    result.push_str(&format!("\x1b[36m{:02X}\x1b[0m ", byte));
                }
                if j % 8 == 7 {
                    result.push(' ');
                }
            }
            
            // Padding for incomplete lines
            for _ in chunk.len()..bytes_per_line {
                result.push_str("   ");
                if chunk.len() % 8 == 7 {
                    result.push(' ');
                }
            }
            
            // ASCII representation
            result.push_str("  ");
            for &byte in chunk {
                let ch = if byte.is_ascii_graphic() { byte as char } else { '.' };
                if raw {
                    result.push(ch);
                } else if ch == '.' {
                    result.push_str(&format!("\x1b[1;30m{}\x1b[0m", ch));
                } else {
                    result.push_str(&format!("\x1b[1;37m{}\x1b[0m", ch));
                }
            }
            result.push('\n');
        }
        result
    }
}
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to the file to scan
    #[arg(short, long)]
    file: String,

    /// Enable debug mode
    #[arg(short, long)]
    debug: bool,

    /// Use AMSI scan
    #[arg(short, long)]
    amsi: bool,

    /// Use Windows Defender scan
    #[arg(short, long)]
    msdefender: bool,
    
    /// Raw output without ANSI colors
    #[arg(short, long)]
    raw: bool,
}

// Main application struct to handle scanning operations
struct ScannerApp {
    args: Args,
}

impl ScannerApp {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        Ok(ScannerApp {
            args: Args::parse(),
        })
    }

    fn run(&self) -> Result<(), Box<dyn std::error::Error>> {
        if !Path::new(&self.args.file).exists() {
            println!("[-] Can't access the target file");
            return Ok(());
        }

        let start = Instant::now();
        let (tx, rx) = mpsc::channel::<Progress>();

        // Progress monitoring thread
        thread::spawn(move || {
            let mut last_update = Instant::now();
            for progress in rx {
                if last_update.elapsed() >= Duration::from_secs(2) {
                    println!("0x{:X} -> 0x{:X} - malicious: {} - {:?}", 
                             progress.low, progress.high, progress.malicious, start.elapsed());
                    last_update = Instant::now();
                }
            }
        });

        if self.args.msdefender {
            println!("Starting Windows Defender scan...");
            let defender_scanner = WindowsDefenderScanner::new(tx.clone());
            defender_scanner.scan(&self.args.file, self.args.debug)
                .map_err(|e| Box::new(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        }

        if self.args.amsi {
            println!("Starting AMSI scan...");
            let amsi_scanner = AMSIScanner::new()
                .map_err(|e| Box::new(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
            amsi_scanner.scan(&self.args.file, self.args.debug)
                .map_err(|e| Box::new(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        }

        if !self.args.amsi && !self.args.msdefender {
            println!("Please specify either --amsi or --defender flag (or both) for scanning.");
        }

        Ok(())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = ScannerApp::new()?;
    app.run()
}