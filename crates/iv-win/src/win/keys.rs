//! Virtual-key codes → the keymap's canonical key names.

use windows::Win32::UI::Input::KeyboardAndMouse::*;

use crate::keymap::canonical_key;

/// The keymap name of a key press: named keys by name, letters as their uppercase letter, and
/// digits and punctuation as the character the keyboard layout gives with only Shift applied
/// (so Ctrl+= is "=" and Shift+= is "+", as on every layout).
pub fn key_name(vk: VIRTUAL_KEY, scan_code: u32, shift: bool) -> Option<String> {
    let named = match vk {
        VK_LEFT => "Left",
        VK_RIGHT => "Right",
        VK_UP => "Up",
        VK_DOWN => "Down",
        VK_ESCAPE => "Esc",
        VK_RETURN => "Enter",
        VK_SPACE => "Space",
        VK_TAB => "Tab",
        VK_BACK => "Backspace",
        VK_DELETE => "Delete",
        VK_INSERT => "Insert",
        VK_HOME => "Home",
        VK_END => "End",
        VK_PRIOR => "PageUp",
        VK_NEXT => "PageDown",
        VK_ADD => "+",
        VK_SUBTRACT => "-",
        VK_MULTIPLY => "*",
        VK_DIVIDE => "/",
        VK_DECIMAL => ".",
        _ => "",
    };
    if !named.is_empty() {
        return Some(named.to_string());
    }
    let v = vk.0;
    if (VK_F1.0..=VK_F24.0).contains(&v) {
        return Some(format!("F{}", v - VK_F1.0 + 1));
    }
    if (VK_NUMPAD0.0..=VK_NUMPAD9.0).contains(&v) {
        return Some(char::from(b'0' + (v - VK_NUMPAD0.0) as u8).to_string());
    }
    if (u16::from(b'A')..=u16::from(b'Z')).contains(&v) {
        return Some(char::from(v as u8).to_string());
    }
    if matches!(
        vk,
        VK_SHIFT | VK_CONTROL | VK_MENU | VK_LSHIFT | VK_RSHIFT | VK_LCONTROL | VK_RCONTROL | VK_LWIN | VK_RWIN
    ) {
        return None;
    }
    let mut state = [0u8; 256];
    if shift {
        state[VK_SHIFT.0 as usize] = 0x80;
    }
    let mut buf = [0u16; 8];
    // flag 4: leave the keyboard's dead-key state alone
    let n = unsafe { ToUnicode(u32::from(v), scan_code, Some(&state), &mut buf, 4) };
    if n == 1 {
        let c = char::from_u32(u32::from(buf[0]))?;
        return canonical_key(&c.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(vk: VIRTUAL_KEY, shift: bool) -> Option<String> {
        let scan = unsafe { MapVirtualKeyW(u32::from(vk.0), MAPVK_VK_TO_VSC) };
        key_name(vk, scan, shift)
    }

    #[test]
    fn named_letters_function_keys_and_numpad() {
        assert_eq!(name(VK_RIGHT, false).as_deref(), Some("Right"));
        assert_eq!(name(VK_ESCAPE, false).as_deref(), Some("Esc"));
        assert_eq!(name(VK_F11, false).as_deref(), Some("F11"));
        assert_eq!(name(VIRTUAL_KEY(u16::from(b'O')), true).as_deref(), Some("O"));
        assert_eq!(name(VK_NUMPAD1, false).as_deref(), Some("1"));
        assert_eq!(name(VK_ADD, false).as_deref(), Some("+"));
        assert_eq!(name(VK_SHIFT, true), None);
        assert_eq!(name(VK_CONTROL, false), None);
    }

    /// Digits and punctuation come from the layout; US and Korean layouts agree on these.
    #[test]
    fn layout_characters_follow_shift() {
        assert_eq!(name(VIRTUAL_KEY(u16::from(b'1')), false).as_deref(), Some("1"));
        assert_eq!(name(VK_OEM_PLUS, false).as_deref(), Some("="));
        assert_eq!(name(VK_OEM_PLUS, true).as_deref(), Some("+"));
        assert_eq!(name(VK_OEM_MINUS, false).as_deref(), Some("-"));
    }
}
