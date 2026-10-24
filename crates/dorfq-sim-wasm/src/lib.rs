//! `dorfq-sim` compiled to WebAssembly, for the report site's playground.
//!
//! The surface is four C functions over the module's linear memory, with no
//! bindings generator: the page's worker copies a configuration's text in
//! through [`alloc`], calls [`run`], reads the framed reply and hands it
//! back to [`dealloc`]. A reply is one tag byte (0 a report document, 1 an
//! error), a little-endian `u32` length, then that many bytes of UTF-8.
//!
//! A panic in wasm is a trap: the instance stops and the message would be
//! lost, so [`run`] installs a hook that records it for [`panic_message`]
//! and then calls the hook it replaced.

use std::panic;
use std::sync::{Mutex, Once};

use dorfq_sim::{SimConfig, simulate};

/// The `name` every playground document carries.
const NAME: &str = "playground";

const TAG_DOCUMENT: u8 = 0;
const TAG_ERROR: u8 = 1;

static HOOK: Once = Once::new();
static PANIC: Mutex<String> = Mutex::new(String::new());

/// Allocates `len` bytes for the caller to fill and pass to [`run`].
#[unsafe(no_mangle)]
pub extern "C" fn alloc(len: usize) -> *mut u8 {
    Box::into_raw(vec![0u8; len].into_boxed_slice()).cast()
}

/// Frees a buffer from [`alloc`], [`run`] or [`panic_message`].
///
/// # Safety
///
/// `ptr` and `len` are exactly a buffer one of those functions returned,
/// with `len` the full length (the five framing bytes included for a
/// reply), not yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dealloc(ptr: *mut u8, len: usize) {
    // SAFETY: the caller passes back a boxed slice of exactly `len` bytes.
    drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)) });
}

/// Parses `len` bytes of TOML at `ptr` and runs it traced. The reply is a
/// framed buffer the caller frees with [`dealloc`]: the JSON document
/// `Report::to_json` writes, named `playground`, or the configuration
/// error's text.
///
/// # Safety
///
/// `ptr` points at `len` initialised bytes that stay valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn run(ptr: *const u8, len: usize) -> *mut u8 {
    HOOK.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            *PANIC.lock().unwrap_or_else(|e| e.into_inner()) = info.to_string();
            previous(info);
        }));
    });
    // SAFETY: the caller guarantees `len` readable bytes at `ptr`.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    match simulate_text(bytes) {
        Ok(document) => frame(TAG_DOCUMENT, &document),
        Err(error) => frame(TAG_ERROR, &error),
    }
}

/// The last panic's message, framed as [`run`]'s reply with tag 0; empty
/// when nothing has panicked.
#[unsafe(no_mangle)]
pub extern "C" fn panic_message() -> *mut u8 {
    let message = PANIC.lock().unwrap_or_else(|e| e.into_inner()).clone();
    frame(TAG_DOCUMENT, &message)
}

fn simulate_text(bytes: &[u8]) -> Result<String, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    let config = SimConfig::parse(text).map_err(|e| e.to_string())?;
    let report = simulate(&config, true);
    Ok(report.to_json(NAME, &config, text))
}

fn frame(tag: u8, text: &str) -> *mut u8 {
    let length = u32::try_from(text.len()).expect("a reply fits in wasm32 memory");
    let mut buffer = Vec::with_capacity(5 + text.len());
    buffer.push(tag);
    buffer.extend_from_slice(&length.to_le_bytes());
    buffer.extend_from_slice(text.as_bytes());
    Box::into_raw(buffer.into_boxed_slice()).cast()
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../../dorfq-sim/examples/sim.toml");

    /// Runs `text` through the exported surface and returns the tag and the
    /// reply's text, freeing both buffers as the worker does.
    fn call(text: &str) -> (u8, String) {
        let input = alloc(text.len());
        // SAFETY: `input` holds `text.len()` bytes from `alloc`.
        unsafe { std::ptr::copy_nonoverlapping(text.as_ptr(), input, text.len()) };
        // SAFETY: `input` holds `text.len()` initialised bytes.
        let reply = unsafe { run(input, text.len()) };
        // SAFETY: `input` is the buffer `alloc` returned, of that length.
        unsafe { dealloc(input, text.len()) };
        read(reply)
    }

    fn read(reply: *mut u8) -> (u8, String) {
        // SAFETY: every reply starts with the five framing bytes.
        let head = unsafe { std::slice::from_raw_parts(reply, 5) };
        let tag = head[0];
        let length = u32::from_le_bytes(head[1..5].try_into().unwrap()) as usize;
        // SAFETY: the framing says `length` bytes follow the head.
        let body = unsafe { std::slice::from_raw_parts(reply.add(5), length) };
        let text = String::from_utf8(body.to_vec()).unwrap();
        // SAFETY: the reply is a buffer of exactly `5 + length` bytes.
        unsafe { dealloc(reply, 5 + length) };
        (tag, text)
    }

    #[test]
    fn the_example_runs_to_a_playground_document() {
        let (tag, document) = call(EXAMPLE);
        assert_eq!(tag, TAG_DOCUMENT);
        assert!(
            document.starts_with(r#"{"schema":3,"name":"playground","config":"#),
            "{}",
            &document[..80.min(document.len())]
        );
        assert!(document.contains(r##""config_text":"# A week of arrivals"##));
    }

    #[test]
    fn an_invalid_configuration_is_its_error_text() {
        let text: String = EXAMPLE
            .lines()
            .map(|line| {
                if line.starts_with("p_bad ") {
                    "p_bad = 1.5"
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("\np_bad = 1.5\n"),
            "the example has a p_bad line"
        );
        let (tag, message) = call(&text);
        assert_eq!(tag, TAG_ERROR);
        assert!(message.contains("p_bad"), "{message}");
    }

    /// The only test that panics, so the empty check cannot read another
    /// test's message. The host unwinds where wasm traps; the hook runs
    /// before either.
    #[test]
    fn the_hook_records_a_panic_for_panic_message() {
        let (tag, message) = call("");
        assert_eq!(tag, TAG_ERROR);
        assert!(!message.is_empty());
        assert_eq!(read(panic_message()), (TAG_DOCUMENT, String::new()));

        assert!(panic::catch_unwind(|| panic!("boom")).is_err());
        let (tag, recorded) = read(panic_message());
        assert_eq!(tag, TAG_DOCUMENT);
        assert!(recorded.contains("boom"), "{recorded}");
    }
}
