use std::collections::BTreeMap;
use std::ptr::NonNull;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, MainThreadMarker};
use objc2_app_kit::{
    NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSColor, NSFont,
    NSFontAttributeName, NSFontWeightSemibold, NSForegroundColorAttributeName, NSMenu, NSStatusBar,
    NSStatusItem, NSTextAlignment, NSVariableStatusItemLength,
};
use objc2_foundation::{
    NSArray, NSAttributedString, NSDictionary, NSMutableAttributedString, NSString,
};

use crate::config::{ColorTheme, HexColorOverride};
use crate::model::{MISSING_VALUE, MenuBarSegment, Provider};
use crate::ui::view_model::{
    CRITICAL_COLORS, ProviderColorPair, UiSnapshot, resolve_provider_colors,
};

const MENU_BAR_FONT_SIZE: f64 = 11.5;
const SEGMENT_SEPARATOR: &str = "  ";
const STALE_ALPHA: f64 = 0.65;
/// Extra horizontal padding beyond macOS's fixed-length `NSStatusItem` natural width (`0.0` = clean ~4pt per side).
pub const MENU_BAR_EXTRA_PAD_X: f64 = 0.0;

/// Dynamic `NSColor` adapting automatically between Dark Mode and Light Mode for any color pair.
pub fn dynamic_color_for_pair(pair: ProviderColorPair, stale: bool) -> Retained<NSColor> {
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

/// Dynamic `NSColor` for a provider respecting `ColorTheme` and optional hex overrides.
pub fn provider_dynamic_color(
    provider: Provider,
    theme: ColorTheme,
    overrides: &BTreeMap<String, HexColorOverride>,
    stale: bool,
) -> Retained<NSColor> {
    let pair = resolve_provider_colors(provider, theme, overrides);
    dynamic_color_for_pair(pair, stale)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RenderKey {
    segments: Vec<MenuBarSegment>,
    fallback_only: bool,
    tooltip: String,
    color_theme: ColorTheme,
    provider_colors: BTreeMap<String, HexColorOverride>,
}

/// Snapshot of real AppKit button state used by `--ui-smoke-test`.
#[derive(Debug, Clone)]
pub struct StatusBarDebugInfo {
    pub title_len: usize,
    pub width_pt: f64,
    pub height_pt: f64,
    pub item_length_pt: f64,
}

/// Owns the single macOS `NSStatusItem` sized via explicit fixed length for balanced horizontal padding.
pub struct StatusBarView {
    item: Retained<NSStatusItem>,
    font: Retained<NSFont>,
    last_render: Option<RenderKey>,
}

impl StatusBarView {
    pub fn new(mtm: MainThreadMarker) -> Self {
        let status_bar = NSStatusBar::systemStatusBar();
        let item = status_bar.statusItemWithLength(NSVariableStatusItemLength);
        if let Some(button) = item.button(mtm) {
            button.setAlignment(NSTextAlignment::Center);
        }
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
            color_theme: snapshot.color_theme,
            provider_colors: snapshot.provider_colors.clone(),
        };
        if self.last_render.as_ref() == Some(&next_key) {
            return false;
        }

        if let Some(button) = self.item.button(mtm) {
            self.item.setLength(NSVariableStatusItemLength);
            let attributed = self.build_attributed_title(snapshot);
            button.setAttributedTitle(&attributed);
            let tooltip = NSString::from_str(&snapshot.menu_bar_tooltip);
            button.setToolTip(Some(&tooltip));
            button.sizeToFit();
            let natural_w = button.frame().size.width;
            self.item.setLength(natural_w + 2.0 * MENU_BAR_EXTRA_PAD_X);
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
            item_length_pt: self.item.length(),
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

            let base_color = provider_dynamic_color(
                seg.provider,
                snapshot.color_theme,
                &snapshot.provider_colors,
                seg.stale,
            );
            let crit_color = (seg.five_hour_critical || seg.weekly_critical)
                .then(|| dynamic_color_for_pair(CRITICAL_COLORS, seg.stale));

            let five_color = if seg.five_hour_critical {
                crit_color.as_deref().unwrap_or(&base_color)
            } else {
                &base_color
            };
            let five_run = make_colored_run(&seg.five_hour_text, &self.font, five_color);
            combined.appendAttributedString(&five_run);

            if let Some(ref weekly_text) = seg.weekly_text {
                let slash_run = make_colored_run("/", &self.font, &base_color);
                combined.appendAttributedString(&slash_run);

                let weekly_color = if seg.weekly_critical {
                    crit_color.as_deref().unwrap_or(&base_color)
                } else {
                    &base_color
                };
                let weekly_run = make_colored_run(weekly_text, &self.font, weekly_color);
                combined.appendAttributedString(&weekly_run);
            }
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
