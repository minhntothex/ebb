use std::ptr::NonNull;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, MainThreadMarker};
use objc2_app_kit::{
    NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSColor, NSFont,
    NSFontAttributeName, NSFontWeightSemibold, NSForegroundColorAttributeName, NSMenu, NSStatusBar,
    NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{
    NSArray, NSAttributedString, NSDictionary, NSMutableAttributedString, NSString,
};

use crate::model::{MISSING_VALUE, MenuBarSegment, Provider};
use crate::ui::view_model::{UiSnapshot, provider_colors};

const MENU_BAR_FONT_SIZE: f64 = 11.5;
const SEGMENT_SEPARATOR: &str = "  ";
const STALE_ALPHA: f64 = 0.65;

/// Dynamic `NSColor` adapting automatically between Dark Mode and Light Mode.
pub fn provider_dynamic_color(provider: Provider, stale: bool) -> Retained<NSColor> {
    let pair = provider_colors(provider);
    let alpha = if stale { STALE_ALPHA } else { 1.0 };
    let (dr, dg, db) = pair.dark.components_f64();
    let (lr, lg, lb) = pair.light.components_f64();

    let block = RcBlock::new(
        move |appearance: NonNull<NSAppearance>| -> NonNull<NSColor> {
            let appearance_ref = unsafe { appearance.as_ref() };
            let dark_name = unsafe { NSAppearanceNameDarkAqua };
            let light_name = unsafe { NSAppearanceNameAqua };
            let candidates = NSArray::from_slice(&[dark_name, light_name]);

            let is_dark = appearance_ref
                .bestMatchFromAppearancesWithNames(&candidates)
                .is_some_and(|matched| matched.isEqualToString(dark_name));

            let color = if is_dark {
                NSColor::colorWithSRGBRed_green_blue_alpha(dr, dg, db, alpha)
            } else {
                NSColor::colorWithSRGBRed_green_blue_alpha(lr, lg, lb, alpha)
            };
            let ptr = Retained::autorelease_return(color);
            unsafe { NonNull::new_unchecked(ptr) }
        },
    );

    unsafe { NSColor::colorWithName_dynamicProvider(None, &block) }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RenderKey {
    segments: Vec<MenuBarSegment>,
    fallback_only: bool,
    tooltip: String,
}

/// Snapshot of real AppKit button state used by `--ui-smoke-test`.
#[derive(Debug, Clone)]
pub struct StatusBarDebugInfo {
    pub title_len: usize,
    pub width_pt: f64,
    pub height_pt: f64,
}

/// Owns the single variable-length macOS `NSStatusItem` and updates it strictly on change.
pub struct StatusBarView {
    item: Retained<NSStatusItem>,
    font: Retained<NSFont>,
    last_render: Option<RenderKey>,
}

impl StatusBarView {
    pub fn new(_mtm: MainThreadMarker) -> Self {
        let status_bar = NSStatusBar::systemStatusBar();
        let item = status_bar.statusItemWithLength(NSVariableStatusItemLength);
        let font = unsafe {
            NSFont::monospacedDigitSystemFontOfSize_weight(MENU_BAR_FONT_SIZE, NSFontWeightSemibold)
        };
        Self {
            item,
            font,
            last_render: None,
        }
    }

    pub fn set_menu(&self, menu: &NSMenu) {
        self.item.setMenu(Some(menu));
    }

    /// Update the Menu Bar button title and tooltip only when rendered state changed.
    /// Returns `true` if AppKit was mutated.
    pub fn apply(&mut self, snapshot: &UiSnapshot, mtm: MainThreadMarker) -> bool {
        let next_key = RenderKey {
            segments: snapshot.menu_bar_segments.clone(),
            fallback_only: snapshot.menu_bar_fallback_only,
            tooltip: snapshot.menu_bar_tooltip.clone(),
        };
        if self.last_render.as_ref() == Some(&next_key) {
            return false;
        }

        if let Some(button) = self.item.button(mtm) {
            let attributed = self.build_attributed_title(snapshot);
            button.setAttributedTitle(&attributed);
            let tooltip = NSString::from_str(&snapshot.menu_bar_tooltip);
            button.setToolTip(Some(&tooltip));
            button.sizeToFit();
        }

        self.last_render = Some(next_key);
        true
    }

    pub fn debug_inspect(&self, mtm: MainThreadMarker) -> Option<StatusBarDebugInfo> {
        let button = self.item.button(mtm)?;
        let title = button.attributedTitle();
        let frame = button.frame();
        Some(StatusBarDebugInfo {
            title_len: title.length(),
            width_pt: frame.size.width,
            height_pt: frame.size.height,
        })
    }

    /// Explicitly remove the status item from the system Menu Bar (used on shutdown/smoke test).
    pub fn remove(&self) {
        NSStatusBar::systemStatusBar().removeStatusItem(&self.item);
    }

    fn build_attributed_title(&self, snapshot: &UiSnapshot) -> Retained<NSMutableAttributedString> {
        let combined = NSMutableAttributedString::new();

        if snapshot.menu_bar_fallback_only {
            let fallback =
                make_colored_run(MISSING_VALUE, &self.font, &NSColor::secondaryLabelColor());
            combined.appendAttributedString(&fallback);
            return combined;
        }

        for (idx, seg) in snapshot.menu_bar_segments.iter().enumerate() {
            if idx > 0 {
                let gap = make_colored_run(SEGMENT_SEPARATOR, &self.font, &NSColor::clearColor());
                combined.appendAttributedString(&gap);
            }
            let color = provider_dynamic_color(seg.provider, seg.stale);
            let run = make_colored_run(&seg.text, &self.font, &color);
            combined.appendAttributedString(&run);
        }

        combined
    }
}

fn make_colored_run(text: &str, font: &NSFont, color: &NSColor) -> Retained<NSAttributedString> {
    let ns_text = NSString::from_str(text);
    let font_key = unsafe { NSFontAttributeName };
    let color_key = unsafe { NSForegroundColorAttributeName };

    let font_obj: &AnyObject = font;
    let color_obj: &AnyObject = color;
    let attrs = NSDictionary::<NSString, AnyObject>::from_slices(
        &[font_key, color_key],
        &[font_obj, color_obj],
    );

    unsafe {
        NSAttributedString::initWithString_attributes(
            NSAttributedString::alloc(),
            &ns_text,
            Some(&attrs),
        )
    }
}
