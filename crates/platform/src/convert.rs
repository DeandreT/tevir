use domain::{
    ButtonAction, InputEvent, InputKind, KeyAction, MouseButton, PhysicalKey, ScrollDelta,
};
use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event as NativeEvent, KeyboardEvent,
    PointerEvent,
};
use keycode::{KeyMap, KeyMapping};
use thiserror::Error;

const MICROPIXELS_PER_PIXEL: f64 = 1_000_000.0;
const HID_USAGE_PAGES: [u16; 3] = [0x07, 0x0c, 0x01];

pub(crate) fn from_native(event: NativeEvent) -> Result<Option<InputEvent>, ConversionError> {
    let (elapsed_micros, kind) = match event {
        NativeEvent::Keyboard(KeyboardEvent::Key { time, key, state }) => {
            let action = match state {
                0 => KeyAction::Release,
                1 => KeyAction::Press,
                2 => KeyAction::Repeat {
                    count: NonZeroU16::MIN,
                },
                other => return Err(ConversionError::UnknownKeyState(other)),
            };
            (
                u64::from(time),
                InputKind::Key {
                    key: hid_key_for_evdev(key)?,
                    action,
                },
            )
        }
        NativeEvent::Keyboard(KeyboardEvent::Modifiers { .. }) => return Ok(None),
        NativeEvent::Pointer(PointerEvent::Motion { time, dx, dy }) => (
            u64::from(time),
            InputKind::PointerRelative {
                dx_micropixels: to_micropixels(dx)?,
                dy_micropixels: to_micropixels(dy)?,
            },
        ),
        NativeEvent::Pointer(PointerEvent::Button {
            time,
            button,
            state,
        }) => {
            let action = match state {
                0 => ButtonAction::Release,
                1 => ButtonAction::Press,
                other => return Err(ConversionError::UnknownButtonState(other)),
            };
            (
                u64::from(time),
                InputKind::PointerButton {
                    button: mouse_button_from_native(button)?,
                    action,
                },
            )
        }
        NativeEvent::Pointer(PointerEvent::Axis { time, axis, value }) => {
            let value = to_micropixels(value)?;
            let delta = match axis {
                0 => ScrollDelta::Continuous {
                    horizontal_micropixels: 0,
                    vertical_micropixels: value,
                },
                1 => ScrollDelta::Continuous {
                    horizontal_micropixels: value,
                    vertical_micropixels: 0,
                },
                other => return Err(ConversionError::UnknownAxis(other)),
            };
            (u64::from(time), InputKind::Scroll(delta))
        }
        NativeEvent::Pointer(PointerEvent::AxisDiscrete120 { axis, value }) => {
            let delta = match axis {
                0 => ScrollDelta::Discrete {
                    horizontal_120: 0,
                    vertical_120: value,
                },
                1 => ScrollDelta::Discrete {
                    horizontal_120: value,
                    vertical_120: 0,
                },
                other => return Err(ConversionError::UnknownAxis(other)),
            };
            (0, InputKind::Scroll(delta))
        }
    };

    Ok(Some(InputEvent {
        elapsed_micros,
        kind,
    }))
}

pub(crate) fn to_native(event: InputEvent) -> Result<NativeEvent, ConversionError> {
    let time = u32::try_from(event.elapsed_micros).unwrap_or(u32::MAX);
    match event.kind {
        InputKind::Key { key, action } => {
            let state = match action {
                KeyAction::Press => 1,
                KeyAction::Repeat { .. } => 2,
                KeyAction::Release => 0,
            };
            Ok(NativeEvent::Keyboard(KeyboardEvent::Key {
                time,
                key: u32::from(evdev_for_hid(key)?),
                state,
            }))
        }
        InputKind::PointerButton { button, action } => {
            Ok(NativeEvent::Pointer(PointerEvent::Button {
                time,
                button: native_mouse_button(button),
                state: match action {
                    ButtonAction::Press => 1,
                    ButtonAction::Release => 0,
                },
            }))
        }
        InputKind::PointerRelative {
            dx_micropixels,
            dy_micropixels,
        } => Ok(NativeEvent::Pointer(PointerEvent::Motion {
            time,
            dx: dx_micropixels as f64 / MICROPIXELS_PER_PIXEL,
            dy: dy_micropixels as f64 / MICROPIXELS_PER_PIXEL,
        })),
        InputKind::PointerAbsolute(_) => Err(ConversionError::UnsupportedAbsolutePointer),
        InputKind::Scroll(ScrollDelta::Discrete {
            horizontal_120,
            vertical_120,
        }) => {
            if vertical_120 != 0 {
                Ok(NativeEvent::Pointer(PointerEvent::AxisDiscrete120 {
                    axis: 0,
                    value: vertical_120,
                }))
            } else {
                Ok(NativeEvent::Pointer(PointerEvent::AxisDiscrete120 {
                    axis: 1,
                    value: horizontal_120,
                }))
            }
        }
        InputKind::Scroll(ScrollDelta::Continuous {
            horizontal_micropixels,
            vertical_micropixels,
        }) => {
            if vertical_micropixels != 0 {
                Ok(NativeEvent::Pointer(PointerEvent::Axis {
                    time,
                    axis: 0,
                    value: vertical_micropixels as f64 / MICROPIXELS_PER_PIXEL,
                }))
            } else {
                Ok(NativeEvent::Pointer(PointerEvent::Axis {
                    time,
                    axis: 1,
                    value: horizontal_micropixels as f64 / MICROPIXELS_PER_PIXEL,
                }))
            }
        }
    }
}

fn hid_key_for_evdev(code: u32) -> Result<PhysicalKey, ConversionError> {
    let evdev = u16::try_from(code).map_err(|_| ConversionError::UnknownEvdevKey(code))?;
    let mapping = KeyMap::from_key_mapping(KeyMapping::Evdev(evdev))
        .map_err(|()| ConversionError::UnknownEvdevKey(code))?;
    let usage_page = HID_USAGE_PAGES
        .into_iter()
        .find(|page| {
            KeyMap::from_usb_code(*page, mapping.usb).is_ok_and(|candidate| candidate == mapping)
        })
        .ok_or(ConversionError::UnknownEvdevKey(code))?;
    Ok(PhysicalKey::new(usage_page, mapping.usb))
}

fn evdev_for_hid(key: PhysicalKey) -> Result<u16, ConversionError> {
    KeyMap::from_usb_code(key.usage_page, key.usage_id)
        .map(|mapping| mapping.evdev)
        .map_err(|()| ConversionError::UnknownHidKey {
            usage_page: key.usage_page,
            usage_id: key.usage_id,
        })
}

fn to_micropixels(value: f64) -> Result<i64, ConversionError> {
    let scaled = value * MICROPIXELS_PER_PIXEL;
    if !scaled.is_finite() || scaled < i64::MIN as f64 || scaled > i64::MAX as f64 {
        return Err(ConversionError::InvalidMotion(value));
    }
    Ok(scaled.round() as i64)
}

fn mouse_button_from_native(button: u32) -> Result<MouseButton, ConversionError> {
    match button {
        BTN_LEFT => Ok(MouseButton::Primary),
        BTN_RIGHT => Ok(MouseButton::Secondary),
        BTN_MIDDLE => Ok(MouseButton::Middle),
        BTN_BACK => Ok(MouseButton::Back),
        BTN_FORWARD => Ok(MouseButton::Forward),
        other => u16::try_from(other)
            .map(MouseButton::Other)
            .map_err(|_| ConversionError::UnknownMouseButton(other)),
    }
}

const fn native_mouse_button(button: MouseButton) -> u32 {
    match button {
        MouseButton::Primary => BTN_LEFT,
        MouseButton::Secondary => BTN_RIGHT,
        MouseButton::Middle => BTN_MIDDLE,
        MouseButton::Back => BTN_BACK,
        MouseButton::Forward => BTN_FORWARD,
        MouseButton::Other(other) => other as u32,
    }
}

use std::num::NonZeroU16;

#[derive(Clone, Debug, Error, PartialEq)]
pub enum ConversionError {
    #[error("unknown evdev key code {0}")]
    UnknownEvdevKey(u32),
    #[error("unknown HID key usage {usage_page:#06x}:{usage_id:#06x}")]
    UnknownHidKey { usage_page: u16, usage_id: u16 },
    #[error("unknown key state {0}")]
    UnknownKeyState(u8),
    #[error("unknown pointer button state {0}")]
    UnknownButtonState(u32),
    #[error("unknown pointer axis {0}")]
    UnknownAxis(u8),
    #[error("unknown mouse button {0}")]
    UnknownMouseButton(u32),
    #[error("motion value {0} is not finite or representable")]
    InvalidMotion(f64),
    #[error("absolute pointer injection is not supported by the native backend")]
    UnsupportedAbsolutePointer,
}

#[cfg(test)]
mod tests {
    use domain::{InputKind, KeyAction, PhysicalKey};
    use input_event::{Event as NativeEvent, KeyboardEvent, PointerEvent};

    use super::{from_native, to_native};

    #[test]
    fn normalizes_evdev_keys_to_hid_and_back() {
        let native = NativeEvent::Keyboard(KeyboardEvent::Key {
            time: 42,
            key: 30,
            state: 1,
        });

        let event = from_native(native)
            .unwrap_or_else(|error| panic!("conversion failed: {error}"))
            .unwrap_or_else(|| panic!("key event must not be filtered"));

        assert_eq!(
            event.kind,
            InputKind::Key {
                key: PhysicalKey::new(0x07, 0x04),
                action: KeyAction::Press,
            }
        );
        assert_eq!(
            to_native(event).unwrap_or_else(|error| panic!("conversion failed: {error}")),
            native
        );
    }

    #[test]
    fn retains_subpixel_pointer_motion() {
        let native = NativeEvent::Pointer(PointerEvent::Motion {
            time: 7,
            dx: 0.125,
            dy: -1.75,
        });
        let event = from_native(native)
            .unwrap_or_else(|error| panic!("conversion failed: {error}"))
            .unwrap_or_else(|| panic!("pointer event must not be filtered"));

        assert_eq!(
            event.kind,
            InputKind::PointerRelative {
                dx_micropixels: 125_000,
                dy_micropixels: -1_750_000,
            }
        );
    }
}
