//! VSL Roleplay patch (2026-11-04). The VSL server draws its whole interface in CEF, so on a VSL
//! server the plugin hides what SA-MP / GTA draw by themselves and hands the keys to the pages.
//!
//! Events from the server (browser events the plugin also reads):
//!  * "vsl:chatlock" 1/0  - old SA-MP chat window kept off (mode 0); T / F6 -> "vsl:chatkey"
//!  * "vsl:uilock"   1/0  - GTA HUD + radar off (unless the player turned it back on), ESC -> "vsl:esc",
//!                          TAB -> "vsl:tab", F1 -> "vsl:f1", live HUD values -> "vsl:hud"
//!  * "vsl:escpass"  1/0  - a SA-MP textdraw selection is open: ESC must reach SA-MP (cancel)
//!  * "vsl:gameset"  key value - "hud" 0/1 (show the GTA HUD), "fps" 0/1 (frame limiter)
//!  * "vsl:gameget"        - answer "vsl:gamecfg" hud fps
//!  * "vsl:nativemenu"     - open the GTA pause menu (settings the VSL menu does not have)
//!  * "vsl:quitgame"       - leave the game (SA-MP /q)
//! Any connection reset unlocks everything and restores the player's own settings: other servers
//! are never touched.
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use cef::types::list::List;
use client_api::samp::version::{version, Version};

static LOCKED: AtomicBool = AtomicBool::new(false);
static SAVED_MODE: AtomicI32 = AtomicI32::new(-1);
static SWALLOW_CHAR: AtomicBool = AtomicBool::new(false);

static UI_LOCK: AtomicBool = AtomicBool::new(false);
static ESC_PASS: AtomicBool = AtomicBool::new(false);
static HUD_WANTED: AtomicBool = AtomicBool::new(false); // player choice: show the GTA HUD anyway
static SAVED_HUD: AtomicI32 = AtomicI32::new(-1);
static LAST_HUD_MS: AtomicU64 = AtomicU64::new(0);
static LAST_HUD: std::sync::Mutex<[i32; 11]> = std::sync::Mutex::new([i32::MIN; 11]);

/// CChat::m_nMode (0 off, 1 light, 2 full) - same place in 0.3.7-R1, R3 and 0.3.DL-R1.
const CHAT_MODE_OFFSET: usize = 0x8;

// GTA San Andreas 1.0 US (the only exe SA-MP runs on)
const MENU_MANAGER: usize = 0xBA6748;
const MENU_HUD_ON: usize = 0x21; // bool m_bHudOn (HUD + radar)
const MENU_ACTIVATE_NEXT: usize = 0x33; // bool m_bActivateMenuNextFrame
const MENU_FRAME_LIMITER: usize = 0x4C; // bool m_bFrameLimiterOn
const PLAYER_PED: usize = 0xB6F5F0; // CPed* of the local player
const PLAYER_MONEY: usize = 0xB7CE50; // int
const CLOCK_HOURS: usize = 0xB70153; // u8
const CLOCK_MINUTES: usize = 0xB70152; // u8
const PED_HEALTH: usize = 0x540; // f32
const PED_ARMOUR: usize = 0x548; // f32
const PED_STATE: usize = 0x530; // i32, 50 = driving
const PED_VEHICLE: usize = 0x58C; // CVehicle*
const PED_WEAPONS: usize = 0x5A0; // CWeapon[13], 0x1C each
const PED_ACTIVE_SLOT: usize = 0x718; // u8
const VEH_MOVE_SPEED: usize = 0x44; // CVector
const VEH_HEALTH: usize = 0x4C0; // f32

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

fn menu_byte(offset: usize) -> *mut u8 {
    (MENU_MANAGER + offset) as *mut u8
}

// ------------------------------------------------------------------ chat lock
pub fn set_locked(lock: bool) {
    let was = LOCKED.swap(lock, Ordering::SeqCst);

    if was && !lock {
        let saved = SAVED_MODE.swap(-1, Ordering::Relaxed);

        if saved > 0 {
            if let Some(chat) = chat_ptr() {
                unsafe { (chat.add(CHAT_MODE_OFFSET) as *mut i32).write_unaligned(saved) };
            }
        }
    }

    tracing::info!(locked = lock, "VSL chat lock");
}

pub fn is_locked() -> bool {
    LOCKED.load(Ordering::Relaxed)
}

// ------------------------------------------------------------------ UI lock
pub fn set_ui_locked(lock: bool) {
    let was = UI_LOCK.swap(lock, Ordering::SeqCst);

    if was && !lock {
        ESC_PASS.store(false, Ordering::Relaxed);
        let saved = SAVED_HUD.swap(-1, Ordering::Relaxed);

        if saved >= 0 {
            unsafe { menu_byte(MENU_HUD_ON).write(saved as u8) };
        }

        if let Ok(mut last) = LAST_HUD.lock() {
            *last = [i32::MIN; 11];
        }
    }

    tracing::info!(locked = lock, "VSL UI lock");
}

pub fn is_ui_locked() -> bool {
    UI_LOCK.load(Ordering::Relaxed)
}

pub fn set_esc_pass(pass: bool) {
    ESC_PASS.store(pass, Ordering::Relaxed);
}

pub fn esc_pass() -> bool {
    ESC_PASS.load(Ordering::Relaxed)
}

/// "vsl:gameset" key value
pub fn game_set(key: &str, value: i32) {
    match key {
        "hud" => HUD_WANTED.store(value == 1, Ordering::Relaxed),
        "fps" => unsafe { menu_byte(MENU_FRAME_LIMITER).write(if value == 1 { 1 } else { 0 }) },
        _ => (),
    }
}

/// "vsl:gamecfg" hud fps
pub fn game_cfg() -> List {
    let list = List::new();
    list.set_integer(0, if HUD_WANTED.load(Ordering::Relaxed) { 1 } else { 0 });
    list.set_integer(1, unsafe { menu_byte(MENU_FRAME_LIMITER).read() } as i32);
    list
}

pub fn open_native_menu() {
    unsafe { menu_byte(MENU_ACTIVATE_NEXT).write(1) };
}

/// SA-MP client command (/q to leave the game)
pub fn run_client_command(name: &str) {
    use client_api::samp::inputs::Input;

    if let Some(input) = Input::get() {
        unsafe {
            let count = std::ptr::addr_of!(input.m_nCommandCount).read_unaligned().max(0) as usize;
            let names = std::ptr::addr_of!(input.m_szCommandName).read_unaligned();
            let procs = std::ptr::addr_of!(input.m_pCommandProc).read_unaligned();

            for i in 0..count.min(procs.len()) {
                let raw = &names[i];
                let len = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
                let cmd: Vec<u8> = raw[..len].iter().map(|&c| c as u8).collect();

                if cmd.eq_ignore_ascii_case(name.as_bytes()) {
                    if let Some(proc_) = procs[i] {
                        let empty = [0 as std::os::raw::c_char];
                        proc_(empty.as_ptr());
                    }

                    return;
                }
            }
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// called once per frame from the main loop; returns new HUD values when something changed
pub fn frame() -> Option<List> {
    if is_locked() {
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

    if !is_ui_locked() {
        return None;
    }

    unsafe {
        let hud = menu_byte(MENU_HUD_ON);
        let want = if HUD_WANTED.load(Ordering::Relaxed) { 1u8 } else { 0u8 };
        let current = hud.read();

        if SAVED_HUD.load(Ordering::Relaxed) < 0 {
            SAVED_HUD.store(current as i32, Ordering::Relaxed);
        }

        if current != want {
            hud.write(want);
        }
    }

    // live HUD values, at most ~7 times per second and only when something changed
    let now = now_ms();

    if now.saturating_sub(LAST_HUD_MS.load(Ordering::Relaxed)) < 150 {
        return None;
    }

    LAST_HUD_MS.store(now, Ordering::Relaxed);

    unsafe {
        let ped = (PLAYER_PED as *const usize).read_unaligned();

        if ped == 0 {
            return None;
        }

        let health = ((ped + PED_HEALTH) as *const f32).read_unaligned();
        let armour = ((ped + PED_ARMOUR) as *const f32).read_unaligned();
        let slot = ((ped + PED_ACTIVE_SLOT) as *const u8).read_unaligned().min(12) as usize;
        let weapon = ped + PED_WEAPONS + slot * 0x1C;
        let w_type = (weapon as *const i32).read_unaligned();
        let w_clip = ((weapon + 0x8) as *const u32).read_unaligned();
        let w_total = ((weapon + 0xC) as *const u32).read_unaligned();
        let money = (PLAYER_MONEY as *const i32).read_unaligned();
        let hours = (CLOCK_HOURS as *const u8).read_unaligned();
        let minutes = (CLOCK_MINUTES as *const u8).read_unaligned();

        let state = ((ped + PED_STATE) as *const i32).read_unaligned();
        let vehicle = ((ped + PED_VEHICLE) as *const usize).read_unaligned();
        let (in_vehicle, speed, veh_health) = if state == 50 && vehicle != 0 {
            let vx = ((vehicle + VEH_MOVE_SPEED) as *const f32).read_unaligned();
            let vy = ((vehicle + VEH_MOVE_SPEED + 4) as *const f32).read_unaligned();
            let vz = ((vehicle + VEH_MOVE_SPEED + 8) as *const f32).read_unaligned();
            let kmh = (vx * vx + vy * vy + vz * vz).sqrt() * 180.0;
            let hp = ((vehicle + VEH_HEALTH) as *const f32).read_unaligned();
            (1, kmh as i32, hp as i32)
        } else {
            (0, 0, 0)
        };

        let values = [
            health.max(0.0).min(1000.0) as i32,
            armour.max(0.0).min(1000.0) as i32,
            money,
            w_type,
            w_clip.min(99999) as i32,
            w_total.min(99999) as i32,
            hours as i32,
            minutes as i32,
            in_vehicle,
            speed.clamp(0, 999),
            veh_health.clamp(0, 1000),
        ];

        if let Ok(mut last) = LAST_HUD.lock() {
            if *last == values {
                return None;
            }

            *last = values;
        }

        let list = List::new();

        for (i, v) in values.iter().enumerate() {
            list.set_integer(i, *v);
        }

        Some(list)
    }
}

/// keys that open VSL pages instead of SA-MP / GTA screens
pub enum VslKey {
    Chat,
    Esc,
    Tab,
    Help,
}

pub fn key_for(wparam: usize) -> Option<VslKey> {
    match wparam {
        0x54 | 0x75 if is_locked() => Some(VslKey::Chat), // T, F6
        0x1B if is_ui_locked() && !esc_pass() => Some(VslKey::Esc), // ESC
        0x09 if is_ui_locked() => Some(VslKey::Tab),       // TAB
        0x70 if is_ui_locked() => Some(VslKey::Help),      // F1
        _ => None,
    }
}

pub fn event_name(key: &VslKey) -> &'static str {
    match key {
        VslKey::Chat => "vsl:chatkey",
        VslKey::Esc => "vsl:esc",
        VslKey::Tab => "vsl:tab",
        VslKey::Help => "vsl:f1",
    }
}

pub fn swallow_next_char() {
    SWALLOW_CHAR.store(true, Ordering::Relaxed);
}

pub fn take_swallowed_char() -> bool {
    SWALLOW_CHAR.swap(false, Ordering::Relaxed)
}
