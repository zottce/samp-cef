use std::net::SocketAddr;
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use cef::types::list::List;
use cef_sys::{cef_key_event_t, cef_key_event_type_t};

use winapi::shared::minwindef::{LPARAM, UINT, WPARAM};
use winapi::um::winuser::*;

use crate::audio::Audio;
use crate::browser::manager::{Manager, MouseKey};
use crate::external::CallbackList;
use crate::network::NetworkClient;
use crate::static_cell::StaticCell;

use client_api::gta::camera::CCamera;
use client_api::gta::menu_manager::CMenuManager;
use client_api::samp::inputs;
use client_api::samp::netgame::NetGame;
use client_api::samp::objects::Object;
use client_api::samp::players::local_player;

use client_api::wndproc;

use crossbeam_channel::{Receiver, Sender};

use retour::GenericDetour;

const CEF_SERVER_PORT_OFFSET: u16 = 2;
pub const CEF_PLUGIN_VERSION: i32 = 0x00_01_00;
const CONNECT_BACKOFF_BASE: Duration = Duration::from_secs(1);
const CONNECT_BACKOFF_MAX: Duration = Duration::from_secs(10);
const AUDIO_SPATIAL_UPDATE_INTERVAL: Duration = Duration::from_millis(33);

static APP: StaticCell<App> = StaticCell::new();

pub enum Event {
    Connect(SocketAddr),
    Timeout,
    NetworkError,
    NetworkJoined,
    BadVersion,

    CreateBrowser {
        id: u32,
        url: String,
        hidden: bool,
        focused: bool,
    },

    CreateExternBrowser(ExternalBrowser),

    DestroyBrowser(u32),
    HideBrowser(u32, bool),
    FocusBrowser(u32, bool),
    EmitEvent(String, List),
    EmitEventOnServer(String, String),
    BrowserCreated(u32, i32),
    AppendToObject(u32, i32),
    RemoveFromObject(u32, i32),
    ToggleDevTools(u32, bool),
    SetAudioSettings(u32, crate::audio::BrowserAudioSettings),
    LoadUrl(u32, String),

    CefInitialize,

    AlwaysListenKeys(u32, bool),
    Terminate,
}

#[derive(Debug)]
pub struct ExternalBrowser {
    pub id: u32,
    pub texture: String,
    pub url: String,
    pub scale: i32,
}

pub struct App {
    connected: bool,
    window_focused: bool,
    cef_ready: bool,
    samp_ready: bool,
    bad_version_notified: bool,
    connect_backoff: Duration,
    next_connect_attempt: Instant,
    last_audio_spatial_update: Instant,

    manager: Arc<Mutex<Manager>>,
    audio: Arc<Audio>,
    network: Option<NetworkClient>,
    callbacks: CallbackList,
    keystate_hook: GenericDetour<extern "stdcall" fn(i32) -> u16>,
    event_tx: Sender<Event>,
    event_rx: Receiver<Event>,

    //
    key_state: [bool; 512],

    // debug
    initialization: Instant,
}

impl Drop for App {
    fn drop(&mut self) {
        tracing::debug!("shutting down client");

        {
            let mut manager = self.manager.lock();
            manager.close_all_browsers();
            manager.shutdown_cef();
        }

        self.network.take();
        self.audio.terminate();

        quit();
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    #[allow(clippy::arc_with_non_send_sync)]
    pub fn new() -> App {
        let (event_tx, event_rx) = crossbeam_channel::unbounded();

        let audio = Audio::new();

        let manager = Arc::new(Mutex::new(Manager::new(event_tx.clone(), audio.clone())));

        let callbacks = crate::external::initialize(event_tx.clone(), manager.clone());

        let keystate_hook = client_api::utils::find_function::<extern "stdcall" fn(i32) -> u16>(
            "user32.dll",
            "GetAsyncKeyState",
        )
        .map(|func| unsafe {
            let hook = GenericDetour::new(func, async_key_state).unwrap();
            hook.enable().unwrap();
            hook
        })
        .unwrap();

        App {
            connected: false,
            cef_ready: false,
            samp_ready: false,
            window_focused: true,
            bad_version_notified: false,
            connect_backoff: CONNECT_BACKOFF_BASE,
            next_connect_attempt: Instant::now(),
            last_audio_spatial_update: Instant::now() - AUDIO_SPATIAL_UPDATE_INTERVAL,
            network: None,
            initialization: Instant::now(),
            manager,
            keystate_hook,
            event_tx,
            event_rx,
            callbacks,
            audio,

            //
            key_state: [false; 512],
        }
    }

    pub fn initialize_hooks() {
        tracing::debug!("initializing SA:MP hooks");

        // apply hook to WndProc
        while !wndproc::initialize(&wndproc::WndProcSettings {
            callback: shitty,
            hwnd: client_api::gta::hwnd(),
        }) {
            std::thread::sleep(Duration::from_millis(10));
        }

        client_api::wndproc::append_callback(win_event);

        NetGame::on_destroy(|| {
            tracing::debug!("SA:MP session is shutting down");
            uninitialize();
        });

        NetGame::on_reconnect(|| {
            if let Some(app) = App::get() {
                app.disconnect();
            }
        });

        client_api::gta::game::on_shutdown(|| {
            tracing::debug!("GTA is shutting down");
            uninitialize();
        });

        tracing::debug!("SA:MP hooks initialized");
    }

    pub fn connect(&mut self) {
        if self.network.is_some() {
            return;
        }

        let now = Instant::now();
        if now < self.next_connect_attempt {
            return;
        }

        if let Some(mut addr) = NetGame::get().addr() {
            if !self.samp_ready {
                App::initialize_hooks();
                self.samp_ready = true;
                self.manager.lock().initialize_cef();
            }

            tracing::debug!(game_server = %addr, "SA:MP server detected");

            addr.set_port(addr.port() + CEF_SERVER_PORT_OFFSET);

            tracing::trace!(
                server = %addr,
                elapsed_ms = self.initialization.elapsed().as_millis(),
                "queuing CEF server connection"
            );

            let network = NetworkClient::new(self.event_tx.clone());
            network.send(Event::Connect(addr));

            self.network = Some(network);
            self.next_connect_attempt = now + self.connect_backoff;
        }
    }

    pub fn disconnect(&mut self) {
        tracing::info!("disconnected from CEF server");

        self.reset_connection(true);
    }

    fn reset_connection(&mut self, notify_disconnect: bool) {
        if notify_disconnect {
            crate::external::call_disconnect();
        }

        // VSL: give the player his own SA-MP chat / GTA HUD back (shows SA-MP's own kick / ban lines);
        // not while the game is closing after the VSL quit (no flash of the old chat)
        if !crate::vsl::is_quitting() {
            crate::vsl::set_locked(false);
            crate::vsl::set_ui_locked(false);
        }

        let mut manager = self.manager.lock();
        manager.close_all_browsers();
        self.network.take();
        self.connected = false;
    }

    fn reset_connect_backoff(&mut self) {
        self.connect_backoff = CONNECT_BACKOFF_BASE;
        self.next_connect_attempt = Instant::now();
    }

    fn bump_connect_backoff(&mut self) {
        self.next_connect_attempt = Instant::now() + self.connect_backoff;
        let doubled = self.connect_backoff + self.connect_backoff;
        self.connect_backoff = std::cmp::min(doubled, CONNECT_BACKOFF_MAX);
    }

    pub fn manager(&self) -> Arc<Mutex<Manager>> {
        self.manager.clone()
    }

    fn get<'a>() -> Option<&'a mut App> {
        unsafe { APP.get_mut() }
    }
}

pub fn initialize() {
    let app = App::new();
    let manager = app.manager();

    crate::render::initialize(manager);

    unsafe {
        APP.set(app);
    }

    if client_api::samp::version::is_unknown_version() {
        tracing::error!("unsupported SA:MP version detected");

        client_api::utils::error_message_box(
            "Unsupported SA:MP",
            "You have installed an unsupported SA:MP version.\nCurrently supported versions are 0.3.DL R1, 0.3.7 R1, and 0.3.7 R3.",
        );

        // don't waste time
    } else {
        tracing::info!("client initialized");
    }
}

pub fn uninitialize() {
    static DESTROY: Once = Once::new();

    DESTROY.call_once(|| unsafe {
        APP.take();
    });
}

fn quit() {
    crate::external::quit();
    crate::render::uninitialize();

    client_api::wndproc::uninitialize();
}

fn shitty() {
    if let Some(app) = App::get() {
        if !app.samp_ready {
            tracing::info!(
                elapsed_ms = app.initialization.elapsed().as_millis(),
                "SA:MP initialized"
            );
            app.samp_ready = true;
            app.manager.lock().initialize_cef();
        } else if !app.window_focused {
            mainloop(); //
        }
    }
}

// inside GTA thread
pub fn mainloop() {
    if let Some(app) = App::get() {
        if !app.connected {
            app.connect();
        }

        if !app.samp_ready {
            return;
        }

        // VSL: old SA-MP chat / GTA HUD off while the server locks them; live HUD values to the pages
        if let Some(hud) = crate::vsl::frame() {
            let manager = app.manager.lock();
            manager.trigger_event("vsl:hud", hud);
        }

        {
            let menu = CMenuManager::get();
            let paused = menu.is_active() || !app.window_focused;

            app.audio.set_paused(paused);
            app.audio.set_gain(menu.sfx_volume());

            let show_cursor = {
                let mut manager = app.manager.lock();
                manager.set_corrupted(paused);
                manager.is_input_blocked() && !menu.is_active()
            };

            // do not redraw default cursor
            if show_cursor {
                client_api::samp::inputs::show_cursor(true);
            }
        }

        while let Ok(event) = app.event_rx.try_recv() {
            match event {
                Event::AlwaysListenKeys(browser_id, listen) => {
                    let manager = app.manager.lock();
                    manager.always_listen_keys(browser_id, listen);
                }

                Event::CreateBrowser {
                    id,
                    url,
                    hidden,
                    focused,
                } => {
                    tracing::trace!(
                        browser = id,
                        url = %url,
                        hidden,
                        focused,
                        "browser creation requested"
                    );

                    let show_cursor = {
                        let mut manager = app.manager.lock();
                        manager.create_browser(id, app.callbacks.clone(), &url);
                        manager.hide_browser(id, hidden);
                        manager.browser_focus(id, focused);

                        manager.is_input_blocked() && !CMenuManager::is_menu_active()
                    };

                    client_api::samp::inputs::show_cursor(show_cursor);
                }

                Event::CreateExternBrowser(ext) => {
                    tracing::trace!(
                        browser = ext.id,
                        texture = %ext.texture,
                        scale = ext.scale,
                        "object browser creation requested"
                    );
                    let mut manager = app.manager.lock();

                    manager.create_browser_on_texture(&ext, app.callbacks.clone());
                }

                Event::DestroyBrowser(id) => {
                    let mut manager = app.manager.lock();
                    manager.close_browser(id, true);
                }

                Event::HideBrowser(id, hide) => {
                    let manager = app.manager.lock();
                    manager.hide_browser(id, hide);
                }

                Event::FocusBrowser(id, focus) => {
                    let show_cursor = {
                        let mut manager = app.manager.lock();
                        manager.browser_focus(id, focus);
                        manager.is_input_blocked() && !CMenuManager::is_menu_active()
                    };

                    client_api::samp::inputs::show_cursor(show_cursor);
                }

                Event::EmitEvent(event, list) => {
                    match event.as_str() {
                        "vsl:chatlock" => crate::vsl::set_locked(list.integer(0) == 1),
                        "vsl:uilock" => crate::vsl::set_ui_locked(list.integer(0) == 1),
                        "vsl:escpass" => crate::vsl::set_esc_pass(list.integer(0) == 1),
                        "vsl:gameset" => {
                            let key = list.string(0).to_string();
                            crate::vsl::game_set(&key, list.integer(1));
                        }
                        "vsl:gameget" => {
                            let manager = app.manager.lock();
                            manager.trigger_event("vsl:gamecfg", crate::vsl::game_cfg());
                        }
                        "vsl:nativemenu" => crate::vsl::open_native_menu(),
                        "vsl:quitgame" => crate::vsl::begin_quit(),
                        _ => (),
                    }

                    let manager = app.manager.lock();
                    manager.trigger_event(&event, list);
                }

                Event::EmitEventOnServer(event, arguments) => {
                    // VSL: page heartbeat (watchdog), quit, SA-MP client commands typed in the VSL chat
                    match event.as_str() {
                        "vsl:beat" => {
                            crate::vsl::beat();
                            continue;
                        }
                        "vsl:say" => match crate::vsl::client_say(&arguments) {
                            crate::vsl::SayAction::Handled => continue,
                            crate::vsl::SayAction::Quit => {
                                crate::vsl::begin_quit();

                                if let Some(network) = app.network.as_mut() {
                                    network.send(Event::EmitEventOnServer(
                                        "vsl:ui".to_string(),
                                        "quit".to_string(),
                                    ));
                                }

                                continue;
                            }
                            crate::vsl::SayAction::Forward => (),
                        },
                        "vsl:ui" if arguments.trim() == "quit" => crate::vsl::begin_quit(),
                        _ => (),
                    }

                    if let Some(network) = app.network.as_mut() {
                        let event = Event::EmitEventOnServer(event, arguments);
                        network.send(event);
                    }
                }

                Event::BrowserCreated(id, code) => {
                    tracing::info!(browser = id, status = code, "browser created");

                    if let Some(network) = app.network.as_mut() {
                        let event = Event::BrowserCreated(id, code);
                        network.send(event);
                    }

                    crate::external::browser_created(id, code);
                }

                Event::CefInitialize => {
                    tracing::info!(
                        elapsed_ms = app.initialization.elapsed().as_millis(),
                        "CEF initialized"
                    );

                    app.cef_ready = true;
                    crate::external::call_initialize();
                }

                Event::AppendToObject(browser, object) => {
                    let mut manager = app.manager.lock();
                    manager.browser_append_to_object(browser, object);
                }

                Event::RemoveFromObject(browser, object) => {
                    let mut manager = app.manager.lock();
                    manager.browser_remove_from_object(browser, object);
                }

                Event::ToggleDevTools(browser, enabled) => {
                    let manager = app.manager.lock();
                    manager.toggle_dev_tools(browser, enabled);
                }

                Event::NetworkJoined => {
                    // VSL: a CEF server answered -> hide the old SA-MP chat right away (connect lines);
                    // the server unlocks it again if its page cannot load
                    crate::vsl::set_locked(true);
                    app.connected = true;
                    app.bad_version_notified = false;
                    app.reset_connect_backoff();
                    tracing::info!("connected to CEF server");
                    crate::external::call_connect();
                }

                Event::BadVersion => {
                    tracing::warn!("CEF client and server versions are incompatible");
                    if !app.bad_version_notified {
                        app.bad_version_notified = true;
                        client_api::utils::error_message_box(
                            "CEF version mismatch",
                            "Client version is incompatible with the server.\nPlease update your client or server plugin.",
                        );
                    }

                    app.reset_connection(false);
                    app.bump_connect_backoff();
                }

                Event::NetworkError => {
                    tracing::warn!("CEF network is unavailable; connection will be retried");
                    let notify_disconnect = app.connected;
                    app.reset_connection(notify_disconnect);
                    app.bump_connect_backoff();
                }

                Event::Timeout => {
                    tracing::debug!("CEF server connection timed out; connection will be retried");
                    let notify_disconnect = app.connected;
                    app.reset_connection(notify_disconnect);
                    app.bump_connect_backoff();
                }

                Event::SetAudioSettings(browser, audio_settings) => {
                    let mut manager = app.manager.lock();
                    manager.set_audio_settings(browser, audio_settings);
                }

                Event::LoadUrl(browser, url) => {
                    let manager = app.manager.lock();
                    manager.load_url(browser, &url);
                }

                _ => (),
            }
        }

        if app.cef_ready && app.connected {
            crate::external::call_mainloop();

            if app.last_audio_spatial_update.elapsed() >= AUDIO_SPATIAL_UPDATE_INTERVAL
                && let Some(local) = local_player()
            {
                app.last_audio_spatial_update = Instant::now();
                let position = local.position();
                let velocity = local.velocity();
                let matrix = CCamera::get().matrix();

                app.audio.set_position(position);
                app.audio.set_velocity(velocity);
                app.audio.set_orientation(matrix);

                let mut manager = app.manager.lock();

                for browser in manager.external_browsers() {
                    for &object_id in browser.object_ids.iter() {
                        if let Some(object) = Object::get(object_id) {
                            let obj_position = object.position();
                            let velocity = object.velocity();
                            let heading = object.heading();

                            if client_api::utils::distance(&position, &obj_position)
                                <= browser.audio_settings.max_distance
                            {
                                app.audio.set_object_settings(
                                    object_id,
                                    obj_position,
                                    velocity,
                                    heading,
                                    browser.audio_settings,
                                );
                            } else {
                                app.audio.object_mute(object_id);
                            }
                        } else {
                            app.audio.object_mute(object_id);
                        }
                    }
                }
            }
        }
    }
}

// TODO: Save key state. Mouse too?
fn win_event(msg: UINT, wparam: WPARAM, lparam: LPARAM) -> bool {
    if let Some(app) = App::get() {
        let mut manager = app.manager.lock();
        let mut notify_key_down = false;

        match msg {
            WM_MOUSEMOVE => {
                let [x, y] = [(lparam as u16) as i32, (lparam >> 16) as u16 as i32];

                manager.send_mouse_move_event(x, y);
            }

            // With CS_DBLCLKS Windows replaces the second BUTTONDOWN with BUTTONDBLCLK.
            // CEF still expects that message as the second mouse-down event, with
            // click_count=2; dropping it leaves the browser with an unmatched mouse-up.
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
                manager.send_mouse_click_event(MouseKey::Left, true)
            }
            WM_LBUTTONUP => manager.send_mouse_click_event(MouseKey::Left, false),
            WM_RBUTTONDOWN | WM_RBUTTONDBLCLK => {
                manager.send_mouse_click_event(MouseKey::Right, true)
            }
            WM_RBUTTONUP => manager.send_mouse_click_event(MouseKey::Right, false),
            WM_MBUTTONDOWN | WM_MBUTTONDBLCLK => {
                manager.send_mouse_click_event(MouseKey::Middle, true)
            }
            WM_MBUTTONUP => manager.send_mouse_click_event(MouseKey::Middle, false),

            WM_MOUSEWHEEL => {
                let delta = if (wparam >> 16) as i16 > 0 { 1 } else { -1 };
                manager.send_mouse_wheel(delta);
            }

            WM_KEYDOWN | WM_KEYUP | WM_CHAR | WM_SYSCHAR | WM_SYSKEYDOWN | WM_SYSKEYUP => {
                // VSL: on a VSL server T / F6 (chat), ESC (menu), TAB (players), F1 (help) open VSL pages
                if (crate::vsl::is_locked() || crate::vsl::is_ui_locked())
                    && !manager.is_input_blocked()
                    && !inputs::Input::is_active()
                    && !inputs::Dialog::is_input_focused()
                    && !CMenuManager::is_menu_active()
                {
                    if let Some(key) = crate::vsl::key_for(wparam) {
                        if msg == WM_KEYDOWN {
                            crate::vsl::log_key(wparam, false, true);
                            manager.trigger_event(crate::vsl::event_name(&key), List::new());
                            crate::vsl::swallow_next_char();
                            return true;
                        }

                        if msg == WM_KEYUP {
                            return true;
                        }
                    }

                    if msg == WM_CHAR && crate::vsl::take_swallowed_char() {
                        return true;
                    }
                }

                if msg == WM_KEYDOWN
                    && (lparam >> 30) & 1 == 0
                    && (crate::vsl::is_locked() || crate::vsl::is_ui_locked())
                    && !(manager.is_input_blocked() && wparam == 0x54)
                {
                    crate::vsl::log_key(wparam, manager.is_input_blocked(), false);   // a VSL key the page did not get
                }

                let is_system_key = msg == WM_SYSCHAR || msg == WM_SYSKEYDOWN || msg == WM_SYSKEYUP;

                let mut event: cef_key_event_t = unsafe { std::mem::zeroed() };

                event.size = std::mem::size_of::<cef_key_event_t>();
                event.windows_key_code = wparam as i32;
                event.native_key_code = lparam as i32;
                event.modifiers = crate::utils::cef_keyboard_modifiers(wparam, lparam);
                event.is_system_key = if is_system_key { 1 } else { 0 };

                if msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN {
                    event.type_ = cef_key_event_type_t::KEYEVENT_RAWKEYDOWN;
                } else if msg == WM_KEYUP || msg == WM_SYSKEYUP {
                    event.type_ = cef_key_event_type_t::KEYEVENT_KEYUP;
                } else if msg == WM_CHAR || msg == WM_SYSCHAR {
                    event.type_ = cef_key_event_type_t::KEYEVENT_CHAR;

                    // GTA:SA creates an ANSI window, so WM_CHAR values in the byte range use the
                    // active Windows-1251 code page. Preserve that behavior while also accepting
                    // UTF-16 values produced by Unicode input methods.
                    let character = if wparam <= u8::MAX as usize {
                        let bytes = [wparam as u8];
                        encoding_rs::WINDOWS_1251
                            .decode(&bytes)
                            .0
                            .encode_utf16()
                            .next()
                    } else {
                        u16::try_from(wparam).ok()
                    };

                    if let Some(character) = character {
                        event.windows_key_code = character as i32;
                        event.character = character;
                        event.unmodified_character = character;
                    }
                }

                // notify GTA. should be notified only once
                let key_index = wparam;
                if key_index < 512 {
                    if manager.is_input_blocked() {
                        // allowed keys (screenshot and chat cycle)
                        let is_allowed_key = wparam == VK_F8 as usize || wparam == VK_F7 as usize;

                        if (app.key_state[key_index]
                            && event.type_ == cef_key_event_type_t::KEYEVENT_KEYUP)
                            || is_allowed_key
                        {
                            app.key_state[key_index] = false;
                            notify_key_down = true;
                        }
                    } else if event.type_ != cef_key_event_type_t::KEYEVENT_CHAR {
                        app.key_state[key_index] =
                            event.type_ == cef_key_event_type_t::KEYEVENT_RAWKEYDOWN;
                    }
                }

                let input_active = inputs::Input::is_active()
                    || inputs::Dialog::is_input_focused()
                    || CMenuManager::is_menu_active();

                if !input_active {
                    manager.send_keyboard_event(event);
                }
            }

            WM_ACTIVATE | WM_ACTIVATEAPP => {
                let active = if msg == WM_ACTIVATEAPP {
                    wparam != 0
                } else {
                    (wparam & 0xFFFF) as u16 != WA_INACTIVE
                };

                crate::external::window_active(active);
                app.window_focused = active;
                app.audio
                    .set_paused(!active || CMenuManager::is_menu_active());
                manager.set_corrupted(!active);
                manager.do_not_draw(!active);

                return false;
            }

            _ => return false,
        }

        // game on pause or the window isn't active
        // allow user to use menu ...
        if manager.is_input_corrupted() {
            return false;
        }

        return manager.is_input_blocked() && !notify_key_down;
    }

    false
}

// TODO: Add ability to return the right AsyncKeyState result
extern "stdcall" fn async_key_state(key: i32) -> u16 {
    if let Some(app) = App::get() {
        let result = app.keystate_hook.call(key);

        // VSL: keys the VSL pages own are never seen as pressed by SA-MP's own polling
        if crate::vsl::hides_async_key(key) {
            return 0;
        }

        if let Some(manager) = app.manager.try_lock() {
            if manager.is_input_blocked() {
                return 0;
            } else {
                return result;
            }
        }
    }

    0
}
