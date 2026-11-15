//! The edit panel (egui, toggled with `E`): tone sliders, resize, quick edit buttons and the
//! quality used when saving. It only reports what the user did; the app applies it.

use iv_ops::{Adjust, Op};

pub struct PanelState {
    pub open: bool,
    /// Slider values; mirrors the newest adjustment edit.
    pub adjust: Adjust,
    pub resize: (u32, u32),
    pub keep_aspect: bool,
    /// JPEG / WebP / AVIF quality used by Save and Save As.
    pub quality: u8,
}

impl Default for PanelState {
    fn default() -> Self {
        Self { open: false, adjust: Adjust::default(), resize: (0, 0), keep_aspect: true, quality: 90 }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum PanelAction {
    /// The sliders moved: replace the adjustment being dragged.
    Adjust(Adjust),
    Edit(Op),
    Undo,
    Redo,
    Save,
    SaveAs,
}

/// What the panel needs to know about the current image.
pub struct PanelContext {
    /// Size of the edited image, if one is shown.
    pub size: Option<(u32, u32)>,
    pub can_undo: bool,
    pub can_redo: bool,
    pub edited: bool,
}

/// Keeps the resize fields consistent: changing one side with "keep aspect" follows with the
/// other, from the current image's aspect ratio.
pub fn follow_aspect(changed_width: bool, value: (u32, u32), image: (u32, u32)) -> (u32, u32) {
    let (iw, ih) = (image.0.max(1) as f64, image.1.max(1) as f64);
    if changed_width {
        (value.0.max(1), ((value.0 as f64 * ih / iw).round() as u32).max(1))
    } else {
        (((value.1 as f64 * iw / ih).round() as u32).max(1), value.1.max(1))
    }
}

pub fn show(ui: &mut egui::Ui, st: &mut PanelState, cx: &PanelContext) -> Vec<PanelAction> {
    let mut actions = Vec::new();
    egui::Panel::right("edit-panel").resizable(false).default_size(250.0).show(ui, |ui| {
        ui.heading("Edit");
        ui.horizontal(|ui| {
            if ui.button("Rotate left").clicked() {
                actions.push(PanelAction::Edit(Op::rotate_ccw()));
            }
            if ui.button("Rotate right").clicked() {
                actions.push(PanelAction::Edit(Op::rotate_cw()));
            }
        });
        ui.horizontal(|ui| {
            if ui.button("Flip H").clicked() {
                actions.push(PanelAction::Edit(Op::flip_h()));
            }
            if ui.button("Flip V").clicked() {
                actions.push(PanelAction::Edit(Op::flip_v()));
            }
        });
        ui.separator();

        ui.label("Adjust");
        let before = st.adjust;
        ui.add(egui::Slider::new(&mut st.adjust.brightness, -1.0..=1.0).text("Brightness"));
        ui.add(egui::Slider::new(&mut st.adjust.contrast, -1.0..=1.0).text("Contrast"));
        ui.add(egui::Slider::new(&mut st.adjust.saturation, -1.0..=1.0).text("Saturation"));
        ui.add(egui::Slider::new(&mut st.adjust.gamma, 0.2..=5.0).logarithmic(true).text("Gamma"));
        if ui.button("Reset adjustments").clicked() {
            st.adjust = Adjust::default();
        }
        if st.adjust != before {
            actions.push(PanelAction::Adjust(st.adjust));
        }
        ui.separator();

        ui.label("Resize");
        if let Some(size) = cx.size {
            if st.resize == (0, 0) {
                st.resize = size;
            }
            let (mut w, mut h) = st.resize;
            ui.horizontal(|ui| {
                let dw = ui.add(egui::DragValue::new(&mut w).range(1..=65535).suffix(" w"));
                let dh = ui.add(egui::DragValue::new(&mut h).range(1..=65535).suffix(" h"));
                if st.keep_aspect && dw.changed() {
                    (w, h) = follow_aspect(true, (w, h), size);
                } else if st.keep_aspect && dh.changed() {
                    (w, h) = follow_aspect(false, (w, h), size);
                }
            });
            st.resize = (w, h);
            ui.checkbox(&mut st.keep_aspect, "Keep aspect ratio");
            ui.horizontal(|ui| {
                if ui.button("50%").clicked() {
                    st.resize = (size.0.div_ceil(2), size.1.div_ceil(2));
                }
                if ui.add_enabled(st.resize != size, egui::Button::new("Apply")).clicked() {
                    actions.push(PanelAction::Edit(Op::Resize { width: st.resize.0, height: st.resize.1 }));
                }
            });
        }
        ui.separator();

        ui.horizontal(|ui| {
            if ui.add_enabled(cx.can_undo, egui::Button::new("Undo")).clicked() {
                actions.push(PanelAction::Undo);
            }
            if ui.add_enabled(cx.can_redo, egui::Button::new("Redo")).clicked() {
                actions.push(PanelAction::Redo);
            }
        });
        ui.add(egui::Slider::new(&mut st.quality, 1..=100).text("Save quality"));
        ui.horizontal(|ui| {
            if ui.add_enabled(cx.edited, egui::Button::new("Save")).clicked() {
                actions.push(PanelAction::Save);
            }
            if ui.button("Save as…").clicked() {
                actions.push(PanelAction::SaveAs);
            }
        });
    });
    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aspect_follows_the_edited_side() {
        assert_eq!(follow_aspect(true, (1920, 999), (3840, 2400)), (1920, 1200));
        assert_eq!(follow_aspect(false, (1, 600), (3840, 2400)), (960, 600));
        assert_eq!(follow_aspect(true, (0, 0), (10, 10)), (1, 1));
    }

    /// Drives the real panel through egui's test harness: widgets found by their label and
    /// clicked or focused, the way a user would.
    #[test]
    fn buttons_and_sliders_report_actions() {
        use egui_kittest::Harness;
        use egui_kittest::kittest::Queryable;

        struct S {
            st: PanelState,
            actions: Vec<PanelAction>,
        }
        let cx = PanelContext { size: Some((400, 300)), can_undo: true, can_redo: false, edited: true };
        let mut h = Harness::new_ui_state(
            |ui, s: &mut S| {
                let a = show(ui, &mut s.st, &cx);
                s.actions.extend(a);
            },
            S { st: PanelState::default(), actions: Vec::new() },
        );
        h.run();
        assert!(h.state().actions.is_empty());

        h.get_by_label("Rotate right").click();
        h.run();
        assert_eq!(h.state().actions, vec![PanelAction::Edit(Op::rotate_cw())]);

        h.get_by_label("Undo").click();
        h.run();
        assert_eq!(h.state().actions.last(), Some(&PanelAction::Undo));

        h.get_by_label("50%").click();
        h.run();
        h.get_by_label("Apply").click();
        h.run();
        assert_eq!(h.state().actions.last(), Some(&PanelAction::Edit(Op::Resize { width: 200, height: 150 })));

        // keyboard on a focused slider moves it: the panel reports the new adjustment
        h.get_by_role_and_label(egui::accesskit::Role::Slider, "Brightness").focus();
        h.run();
        h.key_press(egui::Key::ArrowRight);
        h.run();
        match h.state().actions.last() {
            Some(PanelAction::Adjust(a)) => assert!(a.brightness > 0.0, "{a:?}"),
            other => panic!("expected an adjustment, got {other:?}"),
        }
    }

    #[test]
    fn untouched_panel_reports_nothing() {
        let ctx = egui::Context::default();
        let mut st = PanelState::default();
        let cx = PanelContext { size: Some((400, 300)), can_undo: false, can_redo: false, edited: false };
        let mut actions = Vec::new();
        let mut full = ctx.run_ui(egui::RawInput::default(), |ui| actions = show(ui, &mut st, &cx));
        // no renderer here to upload the font atlas to
        full.textures_delta.clear();
        assert!(actions.is_empty(), "{actions:?}");
        assert_eq!(st.resize, (400, 300), "resize fields start at the image size");
    }
}
