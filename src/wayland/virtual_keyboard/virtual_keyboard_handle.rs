use std::os::unix::io::OwnedFd;
use std::{
    fmt,
    sync::{Arc, Mutex},
};

use tracing::debug;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::server::zwp_virtual_keyboard_v1::Error::NoKeymap;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::server::zwp_virtual_keyboard_v1::{
    self, ZwpVirtualKeyboardV1,
};
use wayland_server::{
    Client, DataInit, DisplayHandle, Resource, backend::ClientId, protocol::wl_keyboard::KeymapFormat,
};
use xkbcommon::xkb;

use super::VirtualKeyboardHandler;
use crate::backend::input::{KeyState, Keycode};
use crate::input::keyboard::{IsolatedKeyboardState, KeyboardTarget};
use crate::{
    input::{Seat, SeatHandler},
    utils::SERIAL_COUNTER,
    wayland::{Dispatch2, seat::WaylandFocus},
};

#[derive(Debug, Default)]
pub(crate) struct VirtualKeyboard {
    // Per-virtual-keyboard keyboard state, isolated from the seat's physical keyboard so its
    // keys and modifiers go through the compositor's shortcut filter without contaminating
    // (or being contaminated by) the physical keyboard.
    state: Option<IsolatedKeyboardState>,
}

// This is OK because all parts of `xkb` will remain on the
// same thread
unsafe impl Send for VirtualKeyboard {}

/// Handle to a virtual keyboard instance
#[derive(Debug, Clone, Default)]
pub(crate) struct VirtualKeyboardHandle {
    pub(crate) inner: Arc<Mutex<VirtualKeyboard>>,
}

/// User data of ZwpVirtualKeyboardV1 object
pub struct VirtualKeyboardUserData<D: SeatHandler> {
    pub(super) handle: VirtualKeyboardHandle,
    pub(crate) seat: Seat<D>,
}

impl<D: SeatHandler> fmt::Debug for VirtualKeyboardUserData<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VirtualKeyboardUserData")
            .field("handle", &self.handle)
            .field("seat", &self.seat.arc)
            .finish()
    }
}

impl<D> Dispatch2<ZwpVirtualKeyboardV1, D> for VirtualKeyboardUserData<D>
where
    D: SeatHandler + VirtualKeyboardHandler + 'static,
    <D as SeatHandler>::KeyboardFocus: WaylandFocus,
{
    fn request(
        &self,
        user_data: &mut D,
        _client: &Client,
        virtual_keyboard: &ZwpVirtualKeyboardV1,
        request: zwp_virtual_keyboard_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwp_virtual_keyboard_v1::Request::Keymap { format, fd, size } => {
                update_keymap(self, format, fd, size as usize);
            }
            zwp_virtual_keyboard_v1::Request::Key { time, key, state } => {
                // Ensure keymap was initialized.
                let mut virtual_data = self.handle.inner.lock().unwrap();
                let iso = match virtual_data.state.as_mut() {
                    Some(iso) => iso,
                    None => {
                        virtual_keyboard.post_error(NoKeymap, "`key` sent before keymap.");
                        return;
                    }
                };

                // This should be wl_keyboard::KeyState, but the protocol does not state
                // the parameter is an enum.
                let key_state = if state == 1 {
                    KeyState::Pressed
                } else {
                    KeyState::Released
                };
                // Keycodes on the wire are evdev codes; the internal representation is offset by 8.
                let keycode = Keycode::new(key + 8);

                // Hand the key off to the compositor, which runs it through its shortcut filter
                // and delivers it to the focused client (driven by this isolated state).
                user_data.virtual_keyboard_key(&self.seat, iso, keycode, key_state, time);
            }
            zwp_virtual_keyboard_v1::Request::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
            } => {
                // Ensure keymap was initialized.
                let mut virtual_data = self.handle.inner.lock().unwrap();
                let iso = match virtual_data.state.as_mut() {
                    Some(iso) => iso,
                    None => {
                        virtual_keyboard.post_error(NoKeymap, "`modifiers` sent before keymap.");
                        return;
                    }
                };

                // Update the isolated modifier state so subsequent keys match shortcuts.
                iso.update_modifiers(mods_depressed, mods_latched, mods_locked, group);
                let mods = iso.modifier_state();

                // Ensure virtual keyboard's keymap is active, and report the modifier change to
                // the focused client (mirroring how the physical keyboard delivers modifier keys).
                let keyboard_handle = self.seat.get_keyboard().unwrap();
                let mut internal = keyboard_handle.arc.internal.lock().unwrap();
                let focus = internal.focus.as_mut().map(|(focus, _)| focus);
                let keymap_changed = keyboard_handle.send_keymap(user_data, &focus, iso.keymap_file(), mods);

                if !keymap_changed {
                    if let Some(focus) = focus {
                        focus.modifiers(&self.seat, user_data, mods, SERIAL_COUNTER.next_serial());
                    }
                }
            }
            zwp_virtual_keyboard_v1::Request::Destroy => {
                // Held-key release happens in `destroyed`, which also covers clients that drop
                // the object without an explicit destroy request.
            }
            _ => unreachable!(),
        }
    }

    fn destroyed(&self, state: &mut D, _client: ClientId, _resource: &ZwpVirtualKeyboardV1) {
        // Release any keys this virtual keyboard still holds, so they don't stick in the
        // focused client.
        let mut virtual_data = self.handle.inner.lock().unwrap();
        if let Some(iso) = virtual_data.state.as_mut() {
            state.virtual_keyboard_destroyed(&self.seat, iso);
        }
    }
}

/// Handle the zwp_virtual_keyboard_v1::keymap request.
fn update_keymap<D>(data: &VirtualKeyboardUserData<D>, format: u32, fd: OwnedFd, size: usize)
where
    D: SeatHandler + 'static,
{
    // Only libxkbcommon compatible keymaps are supported.
    if format != KeymapFormat::XkbV1 as u32 {
        debug!("Unsupported keymap format: {format:?}");
        return;
    }

    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    // SAFETY: we can map the keymap into the memory.
    let new_keymap = match unsafe {
        xkb::Keymap::new_from_fd(
            &context,
            fd,
            size,
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
    } {
        Ok(Some(new_keymap)) => new_keymap,
        Ok(None) => {
            debug!("Invalid libxkbcommon keymap");
            return;
        }
        Err(err) => {
            debug!("Could not map the keymap: {err:?}");
            return;
        }
    };

    // Store active virtual keyboard map.
    let mut inner = data.handle.inner.lock().unwrap();
    inner.state = Some(IsolatedKeyboardState::new_from_keymap(context, new_keymap));
}
