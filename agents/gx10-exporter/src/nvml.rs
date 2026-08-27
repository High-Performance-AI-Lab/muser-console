//! NVML, opened at run time.
//!
//! `libnvidia-ml` is never linked. The build host has no CUDA toolchain,
//! and the exporter has to build, ship, and *start* on a machine with no
//! NVIDIA driver at all — on such a host it simply reports
//! `muser_agent_up 0`. So the library is opened with `dlopen` at scrape
//! time and the ten entry points the exporter needs are resolved with
//! `dlsym`:
//!
//! `nvmlInit_v2`, `nvmlShutdown`, `nvmlDeviceGetCount_v2`,
//! `nvmlDeviceGetHandleByIndex_v2`, `nvmlDeviceGetUUID`,
//! `nvmlDeviceGetName`, `nvmlDeviceGetUtilizationRates`,
//! `nvmlDeviceGetPowerUsage`, `nvmlDeviceGetTemperature`,
//! `nvmlDeviceGetMemoryInfo`.
//!
//! Every `unsafe` block below is one FFI call or one symbol transmute and
//! carries its own safety note. Nothing calls into the library unless every
//! symbol resolved and `nvmlInit_v2` returned `NVML_SUCCESS`; a partial
//! load is treated as no load, so a missing entry point can never turn into
//! a per-field gap that looks like a driver answering `None`.

use std::ffi::{c_char, c_int, c_uint, c_ulonglong, c_void, CStr, CString};
use std::sync::Mutex;

use crate::source::{DeviceSample, GpuSource, ScrapeResult};

/// `NVML_SUCCESS`. Every other return code is a failure, and this exporter
/// deliberately does not distinguish them in the exposition: a code that is
/// not success means the field is not published, full stop.
const NVML_SUCCESS: c_int = 0;

/// `NVML_DEVICE_UUID_V2_BUFFER_SIZE` / `NVML_DEVICE_NAME_V2_BUFFER_SIZE`.
const UUID_BUFFER_LEN: usize = 96;
const NAME_BUFFER_LEN: usize = 96;

/// `NVML_TEMPERATURE_GPU`, the die sensor.
const TEMPERATURE_GPU: c_int = 0;

/// The soname the driver package installs, then the unversioned symlink
/// that only exists where a devel package is present.
const LIBRARY_CANDIDATES: [&str; 2] = ["libnvidia-ml.so.1", "libnvidia-ml.so"];

/// `nvmlUtilization_t`. Layout must match NVML's exactly.
#[repr(C)]
#[derive(Clone, Copy)]
// `memory` is the memory-controller busy percentage, which this exporter
// does not publish; it still occupies its slot in the struct NVML writes.
#[allow(dead_code)]
struct Utilization {
    gpu: c_uint,
    memory: c_uint,
}

/// `nvmlMemory_t`. Layout must match NVML's exactly.
#[repr(C)]
#[derive(Clone, Copy)]
// `free` is derivable from the two fields that are published and is not a
// series of its own; it still occupies its slot.
#[allow(dead_code)]
struct MemoryInfo {
    total: c_ulonglong,
    free: c_ulonglong,
    used: c_ulonglong,
}

/// `nvmlDevice_t` — an opaque handle owned by the library.
type NvmlDevice = *mut c_void;

type FnInit = unsafe extern "C" fn() -> c_int;
type FnShutdown = unsafe extern "C" fn() -> c_int;
type FnGetCount = unsafe extern "C" fn(*mut c_uint) -> c_int;
type FnGetHandle = unsafe extern "C" fn(c_uint, *mut NvmlDevice) -> c_int;
type FnGetText = unsafe extern "C" fn(NvmlDevice, *mut c_char, c_uint) -> c_int;
type FnGetUtilization = unsafe extern "C" fn(NvmlDevice, *mut Utilization) -> c_int;
type FnGetPower = unsafe extern "C" fn(NvmlDevice, *mut c_uint) -> c_int;
type FnGetTemperature = unsafe extern "C" fn(NvmlDevice, c_int, *mut c_uint) -> c_int;
type FnGetMemory = unsafe extern "C" fn(NvmlDevice, *mut MemoryInfo) -> c_int;

/// An initialised NVML: the `dlopen` handle plus the resolved entry points.
struct Library {
    handle: *mut c_void,
    shutdown: FnShutdown,
    get_count: FnGetCount,
    get_handle: FnGetHandle,
    get_uuid: FnGetText,
    get_name: FnGetText,
    get_utilization: FnGetUtilization,
    get_power: FnGetPower,
    get_temperature: FnGetTemperature,
    get_memory: FnGetMemory,
}

// SAFETY: `handle` is a loader handle and the function pointers are code
// addresses; neither is tied to the thread that produced it. NVML's own
// entry points are documented thread-safe, and `NvmlSource` additionally
// serialises every call behind a mutex, so the handle is never used from
// two threads at once.
unsafe impl Send for Library {}

impl Library {
    /// Tries each candidate soname in order and returns the first that
    /// loads, resolves, and initialises.
    fn load(candidates: &[String]) -> Result<Library, String> {
        let mut last = "no NVML library name to try".to_owned();
        for candidate in candidates {
            match Library::open(candidate) {
                Ok(library) => return Ok(library),
                Err(error) => last = error,
            }
        }
        Err(last)
    }

    fn open(name: &str) -> Result<Library, String> {
        let c_name = CString::new(name)
            .map_err(|_| format!("library name '{name}' is not a valid C string"))?;
        // SAFETY: `c_name` is NUL-terminated and outlives the call. dlopen
        // returns null on failure, checked immediately below.
        let handle = unsafe { libc::dlopen(c_name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            return Err(format!("dlopen {name}: {}", loader_error()));
        }
        match Library::resolve(handle) {
            Ok(library) => Ok(library),
            Err(error) => {
                // SAFETY: `handle` came from a successful dlopen above and
                // is not referenced anywhere after this point.
                unsafe {
                    let _ = libc::dlclose(handle);
                }
                Err(format!("{name}: {error}"))
            }
        }
    }

    /// Resolves every entry point up front, then initialises.
    ///
    /// All-or-nothing on purpose: discovering a missing symbol at scrape
    /// time would surface as one absent field, which reads as "the driver
    /// declined to answer" rather than "this exporter is talking to the
    /// wrong library".
    fn resolve(handle: *mut c_void) -> Result<Library, String> {
        // SAFETY (all ten): each type parameter is the exact signature NVML
        // declares for that symbol in nvml.h, and `symbol` refuses a null
        // address, so no call below can dispatch through a bad pointer.
        let init = unsafe { symbol::<FnInit>(handle, "nvmlInit_v2") }?;
        let shutdown = unsafe { symbol::<FnShutdown>(handle, "nvmlShutdown") }?;
        let get_count = unsafe { symbol::<FnGetCount>(handle, "nvmlDeviceGetCount_v2") }?;
        let get_handle = unsafe { symbol::<FnGetHandle>(handle, "nvmlDeviceGetHandleByIndex_v2") }?;
        let get_uuid = unsafe { symbol::<FnGetText>(handle, "nvmlDeviceGetUUID") }?;
        let get_name = unsafe { symbol::<FnGetText>(handle, "nvmlDeviceGetName") }?;
        let get_utilization =
            unsafe { symbol::<FnGetUtilization>(handle, "nvmlDeviceGetUtilizationRates") }?;
        let get_power = unsafe { symbol::<FnGetPower>(handle, "nvmlDeviceGetPowerUsage") }?;
        let get_temperature =
            unsafe { symbol::<FnGetTemperature>(handle, "nvmlDeviceGetTemperature") }?;
        let get_memory = unsafe { symbol::<FnGetMemory>(handle, "nvmlDeviceGetMemoryInfo") }?;

        // SAFETY: `init` resolved above and takes no arguments.
        let status = unsafe { init() };
        if status != NVML_SUCCESS {
            return Err(format!("nvmlInit_v2 returned {status}"));
        }

        Ok(Library {
            handle,
            shutdown,
            get_count,
            get_handle,
            get_uuid,
            get_name,
            get_utilization,
            get_power,
            get_temperature,
            get_memory,
        })
    }

    /// Every device NVML enumerates, each carrying only the fields that
    /// answered. A device whose handle cannot be fetched is skipped rather
    /// than published empty; its siblings are unaffected.
    fn devices(&self) -> Result<Vec<DeviceSample>, String> {
        let mut count: c_uint = 0;
        // SAFETY: NVML writes one c_uint through the pointer; `count` is a
        // live local for the duration of the call.
        let status = unsafe { (self.get_count)(&mut count) };
        if status != NVML_SUCCESS {
            return Err(format!("nvmlDeviceGetCount_v2 returned {status}"));
        }

        let mut devices = Vec::with_capacity(count as usize);
        for index in 0..count {
            let Some(device) = self.device_handle(index) else {
                continue;
            };
            let memory = self.memory(device);
            devices.push(DeviceSample {
                index,
                uuid: self.text(self.get_uuid, device, UUID_BUFFER_LEN),
                name: self.text(self.get_name, device, NAME_BUFFER_LEN),
                utilization_ratio: self.utilization_percent(device).map(ratio_from_percent),
                power_watts: self.power_milliwatts(device).map(watts_from_milliwatts),
                temperature_celsius: self.temperature_celsius(device).map(f64::from),
                memory_used_bytes: memory.map(|(used, _total)| used),
                memory_total_bytes: memory.map(|(_used, total)| total),
            });
        }
        Ok(devices)
    }

    fn device_handle(&self, index: c_uint) -> Option<NvmlDevice> {
        let mut device: NvmlDevice = std::ptr::null_mut();
        // SAFETY: NVML writes one opaque handle through the pointer;
        // `device` is a live local for the duration of the call.
        let status = unsafe { (self.get_handle)(index, &mut device) };
        (status == NVML_SUCCESS && !device.is_null()).then_some(device)
    }

    /// Shared body of the two string probes (`GetUUID` / `GetName`), which
    /// have identical signatures.
    fn text(&self, probe: FnGetText, device: NvmlDevice, capacity: usize) -> Option<String> {
        let mut buffer = vec![0 as c_char; capacity];
        // SAFETY: NVML writes at most `capacity` bytes into `buffer`, which
        // is a live allocation of exactly that many `c_char`s.
        let status = unsafe { probe(device, buffer.as_mut_ptr(), capacity as c_uint) };
        read_c_string(status, &mut buffer)
    }

    fn utilization_percent(&self, device: NvmlDevice) -> Option<c_uint> {
        let mut rates = Utilization { gpu: 0, memory: 0 };
        // SAFETY: NVML fills the two-`c_uint` struct through the pointer;
        // `rates` is a live local with NVML's own layout.
        let status = unsafe { (self.get_utilization)(device, &mut rates) };
        (status == NVML_SUCCESS).then_some(rates.gpu)
    }

    fn power_milliwatts(&self, device: NvmlDevice) -> Option<c_uint> {
        let mut milliwatts: c_uint = 0;
        // SAFETY: NVML writes one c_uint through the pointer.
        let status = unsafe { (self.get_power)(device, &mut milliwatts) };
        (status == NVML_SUCCESS).then_some(milliwatts)
    }

    fn temperature_celsius(&self, device: NvmlDevice) -> Option<c_uint> {
        let mut celsius: c_uint = 0;
        // SAFETY: NVML writes one c_uint through the pointer; the sensor
        // selector is the documented NVML_TEMPERATURE_GPU enumerator.
        let status = unsafe { (self.get_temperature)(device, TEMPERATURE_GPU, &mut celsius) };
        (status == NVML_SUCCESS).then_some(celsius)
    }

    /// `(used, total)` bytes, or nothing: the two come from one call, so
    /// they succeed or fail together.
    fn memory(&self, device: NvmlDevice) -> Option<(u64, u64)> {
        let mut info = MemoryInfo {
            total: 0,
            free: 0,
            used: 0,
        };
        // SAFETY: NVML fills the three-`c_ulonglong` struct through the
        // pointer; `info` is a live local with NVML's own layout.
        let status = unsafe { (self.get_memory)(device, &mut info) };
        (status == NVML_SUCCESS).then_some((info.used, info.total))
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: the documented teardown pair for a handle this struct
        // owns and is giving up; nothing dispatches through it afterwards.
        unsafe {
            let _ = (self.shutdown)();
            let _ = libc::dlclose(self.handle);
        }
    }
}

/// Resolves one symbol and reinterprets it as `T`.
///
/// # Safety
///
/// The caller must instantiate `T` with the exact `extern "C"` signature
/// NVML declares for `name`; calling through a mismatched signature is
/// undefined behaviour. `T` must be a function-pointer type, which the
/// width check below enforces at run time.
unsafe fn symbol<T: Copy>(handle: *mut c_void, name: &str) -> Result<T, String> {
    if std::mem::size_of::<T>() != std::mem::size_of::<*mut c_void>() {
        return Err(format!("{name}: not a function pointer on this target"));
    }
    let c_name =
        CString::new(name).map_err(|_| format!("symbol name '{name}' is not a valid C string"))?;
    // SAFETY: `handle` came from dlopen and `c_name` is NUL-terminated and
    // outlives the call. dlsym returns null when the symbol is absent.
    let address = unsafe { libc::dlsym(handle, c_name.as_ptr()) };
    if address.is_null() {
        return Err(format!("{name}: symbol not present"));
    }
    // SAFETY: `address` is a non-null code address and `T` is the same
    // width as a pointer (checked above); the caller pins the signature.
    Ok(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&address) })
}

/// The loader's own explanation, for the log. Contains a path or a symbol
/// name at worst; this process holds no credentials to leak into it.
fn loader_error() -> String {
    // SAFETY: dlerror returns a pointer to a NUL-terminated buffer owned by
    // the loader, or null when no error is pending. The string is copied
    // out immediately, before anything else can call dlerror again.
    let message = unsafe { libc::dlerror() };
    if message.is_null() {
        return "not loadable on this host".to_owned();
    }
    // SAFETY: `message` is non-null and loader-owned, NUL-terminated.
    unsafe { CStr::from_ptr(message) }
        .to_string_lossy()
        .into_owned()
}

/// Reads a string NVML wrote into `buffer`.
///
/// `None` when the call failed or the driver's bytes are not UTF-8: a label
/// the exporter cannot reproduce faithfully is omitted, never guessed at or
/// lossily patched.
fn read_c_string(status: c_int, buffer: &mut [c_char]) -> Option<String> {
    if status != NVML_SUCCESS {
        return None;
    }
    // A driver that filled the buffer without terminating it would run the
    // read past the end; terminate it ourselves before reading.
    if let Some(last) = buffer.last_mut() {
        *last = 0;
    }
    // SAFETY: `buffer` is non-empty and NUL-terminated (forced above), and
    // stays borrowed for the whole of the read.
    let text = unsafe { CStr::from_ptr(buffer.as_ptr()) };
    text.to_str().ok().map(str::to_owned)
}

/// NVML reports GPU utilization as whole percent; the exposition publishes
/// a ratio. No clamping: a value outside 0..1 would mean the driver said
/// something outside 0..100, and inventing a plausible number in its place
/// is exactly what this exporter exists not to do.
fn ratio_from_percent(percent: c_uint) -> f64 {
    f64::from(percent) / 100.0
}

/// NVML reports board power in milliwatts.
fn watts_from_milliwatts(milliwatts: c_uint) -> f64 {
    f64::from(milliwatts) / 1000.0
}

/// The production source: NVML, loaded on demand.
pub struct NvmlSource {
    candidates: Vec<String>,
    library: Mutex<Option<Library>>,
}

impl NvmlSource {
    /// The driver's own soname first, then the unversioned symlink.
    pub fn new() -> NvmlSource {
        NvmlSource::with_candidates(&LIBRARY_CANDIDATES)
    }

    /// Same source with an explicit candidate list. Used by the tests to
    /// pin the "no NVML on this host" path deterministically instead of
    /// depending on what the test machine happens to have installed.
    pub fn with_candidates(candidates: &[&str]) -> NvmlSource {
        NvmlSource {
            candidates: candidates.iter().map(|name| (*name).to_owned()).collect(),
            library: Mutex::new(None),
        }
    }
}

impl Default for NvmlSource {
    fn default() -> NvmlSource {
        NvmlSource::new()
    }
}

impl GpuSource for NvmlSource {
    fn scrape(&self) -> ScrapeResult {
        let mut guard = self
            .library
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if guard.is_none() {
            // Retried on every scrape rather than once at startup: a driver
            // can appear after the exporter does (container restart, module
            // load), and the honest report until it does is simply that the
            // source did not answer.
            *guard = Some(Library::load(&self.candidates)?);
        }

        let outcome = guard
            .as_ref()
            .expect("the library was just loaded or was already present")
            .devices();
        if outcome.is_err() {
            // The library loaded but the driver would not answer. Drop the
            // handle so the next scrape re-initialises instead of reporting
            // through a stale one.
            *guard = None;
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(buffer: &mut [c_char], bytes: &[u8]) {
        for (slot, byte) in buffer.iter_mut().zip(bytes) {
            *slot = *byte as c_char;
        }
    }

    #[test]
    fn a_host_without_nvml_reports_a_failure_not_an_empty_device_list() {
        let source = NvmlSource::with_candidates(&["libnvidia-ml-absent-from-this-host.so.1"]);
        let error = source.scrape().expect_err("no such library exists");
        assert!(
            error.contains("libnvidia-ml-absent-from-this-host.so.1"),
            "{error}"
        );
        // An empty Ok would publish `muser_agent_up 1` with no devices,
        // which claims a working NVML that found no GPUs. That is a
        // different fact and must not be conflated with this one.
        assert!(
            source.scrape().is_err(),
            "the failure is reported on every scrape, never cached into a success"
        );
    }

    #[test]
    fn the_candidate_list_is_tried_in_order_and_the_last_failure_is_reported() {
        let source =
            NvmlSource::with_candidates(&["libnvidia-ml-absent-a.so", "libnvidia-ml-absent-b.so"]);
        let error = source.scrape().expect_err("neither library exists");
        assert!(error.contains("libnvidia-ml-absent-b.so"), "{error}");
    }

    #[test]
    fn an_empty_candidate_list_is_a_failure() {
        let source = NvmlSource::with_candidates(&[]);
        assert!(source.scrape().is_err());
    }

    #[test]
    fn unit_conversions_match_what_nvml_documents() {
        assert_eq!(ratio_from_percent(0), 0.0);
        assert_eq!(ratio_from_percent(37), 0.37);
        assert_eq!(ratio_from_percent(100), 1.0);
        assert_eq!(watts_from_milliwatts(0), 0.0);
        assert_eq!(watts_from_milliwatts(145_500), 145.5);
    }

    #[test]
    fn driver_text_is_read_only_when_the_call_succeeded_and_the_bytes_are_utf8() {
        let mut buffer = [0 as c_char; 16];
        fill(&mut buffer, b"NVIDIA GB10\0");
        assert_eq!(
            read_c_string(NVML_SUCCESS, &mut buffer),
            Some("NVIDIA GB10".to_owned())
        );
        assert_eq!(
            read_c_string(9, &mut buffer),
            None,
            "a failed call publishes no label, whatever is left in the buffer"
        );

        let mut invalid = [0 as c_char; 4];
        fill(&mut invalid, &[0xff, 0xfe, 0x00]);
        assert_eq!(
            read_c_string(NVML_SUCCESS, &mut invalid),
            None,
            "driver bytes that are not UTF-8 are omitted, not lossily patched"
        );
    }

    #[test]
    fn an_unterminated_buffer_is_terminated_before_it_is_read() {
        let mut buffer = [b'A' as c_char; 8];
        assert_eq!(
            read_c_string(NVML_SUCCESS, &mut buffer),
            Some("AAAAAAA".to_owned())
        );
    }

    #[test]
    fn a_symbol_that_is_not_a_function_pointer_is_refused() {
        // A pointer-to-pointer is wider than a code address on no target we
        // build for, but `u128` is: the width check is what stops a
        // mis-instantiated `symbol::<T>` from transmuting garbage.
        // SAFETY: the call cannot dispatch — it fails the width check
        // before ever touching the (null) handle.
        let refused = unsafe { symbol::<u128>(std::ptr::null_mut(), "nvmlInit_v2") };
        assert!(refused.is_err());
    }

    #[test]
    fn nvml_structs_have_nvmls_layout() {
        assert_eq!(std::mem::size_of::<Utilization>(), 8);
        assert_eq!(std::mem::size_of::<MemoryInfo>(), 24);
    }
}
