use anyhow::{Context, Result};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{
    Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent, TrayIconId,
};

const OPEN_MENU_ID: &str = "aec-bridge.tray.open";
const QUIT_MENU_ID: &str = "aec-bridge.tray.quit";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrayAction {
    Open,
    Quit,
}

fn menu_action(event: &MenuEvent, open_id: &MenuId, quit_id: &MenuId) -> Option<TrayAction> {
    if event.id == *open_id {
        Some(TrayAction::Open)
    } else if event.id == *quit_id {
        Some(TrayAction::Quit)
    } else {
        None
    }
}

fn tray_action(event: &TrayIconEvent, tray_id: &TrayIconId) -> Option<TrayAction> {
    if event.id() != tray_id {
        return None;
    }
    matches!(
        event,
        TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } | TrayIconEvent::DoubleClick {
            button: MouseButton::Left,
            ..
        }
    )
    .then_some(TrayAction::Open)
}

pub struct TrayController {
    // The last TrayIcon clone removes the notification-area icon when dropped.
    _icon: TrayIcon,
    tray_id: TrayIconId,
    open_id: MenuId,
    quit_id: MenuId,
    status_item: MenuItem,
    last_status: String,
}

impl TrayController {
    pub fn create(status: &str) -> Result<Self> {
        let open_id = MenuId::new(OPEN_MENU_ID);
        let quit_id = MenuId::new(QUIT_MENU_ID);
        let open_item = MenuItem::with_id(open_id.clone(), "Open AEC Bridge", true, None);
        let status_item = MenuItem::new(format!("Status: {status}"), false, None);
        let separator = PredefinedMenuItem::separator();
        let quit_item = MenuItem::with_id(quit_id.clone(), "Quit AEC Bridge", true, None);
        let menu = Menu::with_items(&[&open_item, &status_item, &separator, &quit_item])
            .context("could not create the tray menu")?;
        let (rgba, width, height) = icon_rgba();
        let icon =
            Icon::from_rgba(rgba, width, height).context("could not create the tray icon image")?;
        let tray_icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip("AEC Bridge")
            .with_icon(icon)
            .build()
            .context("Windows could not create the notification-area icon")?;
        let tray_id = tray_icon.id().clone();

        Ok(Self {
            _icon: tray_icon,
            tray_id,
            open_id,
            quit_id,
            status_item,
            last_status: status.to_owned(),
        })
    }

    pub fn set_status(&mut self, status: &str) {
        if self.last_status == status {
            return;
        }
        self.status_item.set_text(format!("Status: {status}"));
        self.last_status = status.to_owned();
    }

    pub fn poll_actions(&self) -> Vec<TrayAction> {
        let mut actions = Vec::new();

        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if let Some(action) = menu_action(&event, &self.open_id, &self.quit_id) {
                actions.push(action);
            }
        }

        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if let Some(action) = tray_action(&event, &self.tray_id) {
                actions.push(action);
            }
        }

        actions
    }
}

fn icon_rgba() -> (Vec<u8>, u32, u32) {
    const SIZE: u32 = 32;
    const CENTER: f32 = 15.5;
    const RADIUS: f32 = 14.5;
    const BARS: [(f32, f32); 5] = [
        (8.0, 7.0),
        (12.0, 13.0),
        (16.0, 19.0),
        (20.0, 13.0),
        (24.0, 7.0),
    ];

    let mut pixels = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let distance = ((px - CENTER).powi(2) + (py - CENTER).powi(2)).sqrt();
            let background_alpha = ((RADIUS + 0.75 - distance) * 255.0).clamp(0.0, 255.0) as u8;
            let in_wave = BARS.iter().any(|(center_x, height)| {
                (px - center_x).abs() <= 1.25 && (py - CENTER).abs() <= height / 2.0
            });

            if in_wave && background_alpha > 0 {
                pixels.extend_from_slice(&[246, 250, 255, 255]);
            } else {
                pixels.extend_from_slice(&[40, 143, 235, background_alpha]);
            }
        }
    }
    (pixels, SIZE, SIZE)
}

#[cfg(test)]
mod tests {
    use super::{TrayAction, icon_rgba, menu_action, tray_action};
    use tray_icon::dpi::PhysicalPosition;
    use tray_icon::menu::{MenuEvent, MenuId};
    use tray_icon::{MouseButton, MouseButtonState, Rect, TrayIconEvent, TrayIconId};

    #[test]
    fn tray_icon_has_valid_rgba_dimensions_and_transparency() {
        let (pixels, width, height) = icon_rgba();
        assert_eq!((width, height), (32, 32));
        assert_eq!(pixels.len(), (width * height * 4) as usize);
        assert_eq!(pixels[3], 0);
        assert!(
            pixels
                .as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel[3] == 255)
        );
    }

    #[test]
    fn menu_ids_map_to_open_and_quit_actions() {
        let open_id = MenuId::new("open");
        let quit_id = MenuId::new("quit");
        assert_eq!(
            menu_action(
                &MenuEvent {
                    id: open_id.clone()
                },
                &open_id,
                &quit_id
            ),
            Some(TrayAction::Open)
        );
        assert_eq!(
            menu_action(
                &MenuEvent {
                    id: quit_id.clone()
                },
                &open_id,
                &quit_id
            ),
            Some(TrayAction::Quit)
        );
        assert_eq!(
            menu_action(
                &MenuEvent {
                    id: MenuId::new("other")
                },
                &open_id,
                &quit_id
            ),
            None
        );
    }

    #[test]
    fn only_left_clicks_on_our_tray_icon_restore_the_window() {
        let tray_id = TrayIconId::new("ours");
        let left_click = TrayIconEvent::Click {
            id: tray_id.clone(),
            position: PhysicalPosition::new(0.0, 0.0),
            rect: Rect::default(),
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
        };
        let right_click = TrayIconEvent::Click {
            id: tray_id.clone(),
            position: PhysicalPosition::new(0.0, 0.0),
            rect: Rect::default(),
            button: MouseButton::Right,
            button_state: MouseButtonState::Up,
        };
        let other_icon = TrayIconEvent::DoubleClick {
            id: TrayIconId::new("other"),
            position: PhysicalPosition::new(0.0, 0.0),
            rect: Rect::default(),
            button: MouseButton::Left,
        };

        assert_eq!(tray_action(&left_click, &tray_id), Some(TrayAction::Open));
        assert_eq!(tray_action(&right_click, &tray_id), None);
        assert_eq!(tray_action(&other_icon, &tray_id), None);
    }
}
