//! Колесо волн под плашкой трека. Данные качаются в фоне, состояние живёт в UI-потоке.

use crate::api::{self, Api, WheelItem};
use crate::blob::{self, Blobs};
use crate::config::log;
use crate::images::Images;
use crate::{App, WheelCard};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

struct Home {
    ui: slint::Weak<App>,
    images: Images,
    blobs: Blobs,
    wheel: Vec<WheelItem>,
}

thread_local! {
    static HOME: RefCell<Option<Home>> = const { RefCell::new(None) };
}

fn with_home<R>(f: impl FnOnce(&mut Home) -> R) -> Option<R> {
    HOME.with(|h| h.borrow_mut().as_mut().map(f))
}

/// Вошли в аккаунт: загрузить колесо.
pub fn start(ui: &App, api: Arc<Api>, images: Images) {
    HOME.with(|h| {
        *h.borrow_mut() = Some(Home { ui: ui.as_weak(), images, blobs: Blobs::default(), wheel: Vec::new() });
    });
    std::thread::spawn(move || {
        let wheel = api.wheel(&[]);
        let _ = slint::invoke_from_event_loop(move || match wheel {
            Ok(items) => {
                with_home(|h| h.set_wheel(items));
            }
            Err(e) => log(format!("колесо: {e}")),
        });
    });
}

/// Карточка колеса по номеру.
pub fn wheel_item(i: usize) -> Option<WheelItem> {
    with_home(|h| h.wheel.get(i).cloned()).flatten()
}

impl Home {
    fn set_wheel(&mut self, items: Vec<WheelItem>) {
        let Some(ui) = self.ui.upgrade() else { return };
        let rows: Vec<WheelCard> = items
            .iter()
            .map(|w| WheelCard {
                name: w.name.clone().into(),
                description: w.description.clone().into(),
                cover: Default::default(),
                round: w.round,
                blob: self.blobs.get(blob::tint(w.color)),
            })
            .collect();
        let model = Rc::new(VecModel::from(rows));
        let px = (64.0 * ui.window().scale_factor()).round() as u32;
        for (i, w) in items.iter().enumerate() {
            let Some(uri) = &w.cover else { continue };
            let m = model.clone();
            self.images.get(&api::pic(uri, px), px, move |img| {
                if let Some(mut row) = m.row_data(i) {
                    row.cover = img.clone();
                    m.set_row_data(i, row);
                }
            });
        }
        ui.set_wheel(ModelRc::from(model));
        // У оригинала по центру вторая карточка, первая выглядывает слева.
        if items.len() > 1 {
            ui.set_wheel_offset(ui.get_wheel_home());
        }
        self.wheel = items;
    }
}
