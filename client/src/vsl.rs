//! VSL Roleplay patch (2026-11-04): the server can LOCK the old SA-MP chat, because VSL draws its own
//! chat box in CEF. While locked:
//!  * the SA-MP chat window is kept off (mode 0) every frame, so F7 cannot bring it back;
//!  * T and F6 open the VSL CEF chat (event "vsl:chatkey" to the pages) instead of the SA-MP input line.
//! The server locks with the browser event "vsl:chatlock" (1 / 0). Any connection reset unlocks and
//! restores the player's own chat mode, so other servers are never touched.
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use client_api::samp::version::{version, Version};

static LOCKED: AtomicBool = AtomicBool::new(false);
static SAVED_MODE: AtomicI32 = AtomicI32::new(-1);
static SWALLOW_CHAR: AtomicBool = AtomicBool::new(false);

/// CChat::m_nMode (0 off, 1 light, 2 full) - same place in 0.3.7-R1, R3 and 0.3.DL-R1.
const CHAT_MODE_OFFSET: usize = 0x8;

fn chat_ptr() -> Option<*mut u8> {
    // CChat pointer = the word right before CInput in samp.dll
    let offset = match version() {
        Version::V037 => 0x21A0E4,
        Version::V037R3 => 0x26E8C8,
        Version::V03DL => 0x2ACA10,
        _ => return None,
    };

    unsafe {
        let base = client_api::samp::handle();

        if base.is_null() {
            return None;
        }

        let chat = (base.add(offset) as *const *mut u8).read_unaligned();

        if chat.is_null() { None } else { Some(chat) }
    }
}

pub fn set_locked(lock: bool) {
    let was = LOCKED.swap(lock, Ordering::SeqCst);

    if was && !lock {
        restore();
    }

    tracing::info!(locked = lock, "VSL chat lock");
}

pub fn is_locked() -> bool {
    LOCKED.load(Ordering::Relaxed)
}

/// called once per frame from the main loop
pub fn frame() {
    if !is_locked() {
        return;
    }

    if let Some(chat) = chat_ptr() {
        unsafe {
            let mode = chat.add(CHAT_MODE_OFFSET) as *mut i32;
            let current = mode.read_unaligned();

            if current != 0 {
                if SAVED_MODE.load(Ordering::Relaxed) < 0 {
                    SAVED_MODE.store(current, Ordering::Relaxed);
                }

                mode.write_unaligned(0);
            }
        }
    }
}

fn restore() {
    let saved = SAVED_MODE.swap(-1, Ordering::Relaxed);

    if saved > 0 {
        if let Some(chat) = chat_ptr() {
            unsafe { (chat.add(CHAT_MODE_OFFSET) as *mut i32).write_unaligned(saved) };
        }
    }
}

/// the key opening the VSL chat (T or F6)
pub fn is_chat_key(wparam: usize) -> bool {
    wparam == 0x54 || wparam == 0x75
}

pub fn swallow_next_char() {
    SWALLOW_CHAR.store(true, Ordering::Relaxed);
}

pub fn take_swallowed_char() -> bool {
    SWALLOW_CHAR.swap(false, Ordering::Relaxed)
}
