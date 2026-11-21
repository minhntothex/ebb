use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, sel};
use objc2_app_kit::{
    NSColor, NSControlStateValueOff, NSControlStateValueOn, NSFont, NSFontAttributeName,
    NSFontWeightRegular, NSForegroundColorAttributeName, NSMenu, NSMenuItem,
    NSMutableParagraphStyle, NSParagraphStyleAttributeName, NSTextAlignment, NSTextTab,
};
use objc2_foundation::{
    NSArray, NSAttributedString, NSDictionary, NSMutableAttributedString, NSObject,
    NSObjectProtocol, NSString,
};

use crate::ui::status_bar::provider_dynamic_color;
use crate::ui::view_model::{
    INTERVAL_PRESETS, ProviderRowViewModel, UiSnapshot, format_interval_label,
};

const DROPDOWN_FONT_SIZE: f64 = 13.0;
const STATUS_FONT_SIZE: f64 = 11.0;
const RIGHT_TAB_LOCATION_PT: f64 = 195.0;

/// Actions emitted by the native `NSMenu` dropdown on the AppKit main thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    RefreshNow,
    SetIntervalSeconds(u64),
    ToggleShowClaude,
    ToggleShowCodex,
    ToggleLaunchAtLogin,
    OpenConfigFolder,
    Quit,
}

type ActionCallback = Box<dyn FnMut(MenuAction)>;

struct MenuTargetIvars {
    callback: RefCell<ActionCallback>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "EbbMenuTarget"]
    #[ivars = MenuTargetIvars]
    struct MenuTarget;

    unsafe impl NSObjectProtocol for MenuTarget {}

    impl MenuTarget {
        #[unsafe(method(onRefreshNow:))]
        fn on_refresh_now(&self, _sender: Option<&AnyObject>) {
            self.dispatch(MenuAction::RefreshNow);
        }

        #[unsafe(method(onSetInterval30:))]
        fn on_set_interval_30(&self, _sender: Option<&AnyObject>) {
            self.dispatch(MenuAction::SetIntervalSeconds(30));
        }

        #[unsafe(method(onSetInterval60:))]
        fn on_set_interval_60(&self, _sender: Option<&AnyObject>) {
            self.dispatch(MenuAction::SetIntervalSeconds(60));
        }

        #[unsafe(method(onSetInterval120:))]
        fn on_set_interval_120(&self, _sender: Option<&AnyObject>) {
            self.dispatch(MenuAction::SetIntervalSeconds(120));
        }

        #[unsafe(method(onSetInterval300:))]
        fn on_set_interval_300(&self, _sender: Option<&AnyObject>) {
            self.dispatch(MenuAction::SetIntervalSeconds(300));
        }

        #[unsafe(method(onToggleClaude:))]
        fn on_toggle_claude(&self, _sender: Option<&AnyObject>) {
            self.dispatch(MenuAction::ToggleShowClaude);
        }

        #[unsafe(method(onToggleCodex:))]
        fn on_toggle_codex(&self, _sender: Option<&AnyObject>) {
            self.dispatch(MenuAction::ToggleShowCodex);
        }

        #[unsafe(method(onToggleLaunchAtLogin:))]
        fn on_toggle_launch_at_login(&self, _sender: Option<&AnyObject>) {
            self.dispatch(MenuAction::ToggleLaunchAtLogin);
        }

        #[unsafe(method(onOpenConfigFolder:))]
        fn on_open_config_folder(&self, _sender: Option<&AnyObject>) {
            self.dispatch(MenuAction::OpenConfigFolder);
        }

        #[unsafe(method(onQuit:))]
        fn on_quit(&self, _sender: Option<&AnyObject>) {
            self.dispatch(MenuAction::Quit);
        }
    }
);

impl MenuTarget {
    fn new(mtm: MainThreadMarker, callback: ActionCallback) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(MenuTargetIvars {
            callback: RefCell::new(callback),
        });
        unsafe { objc2::msg_send![super(this), init] }
    }

    fn dispatch(&self, action: MenuAction) {
        if let Ok(mut cb) = self.ivars().callback.try_borrow_mut() {
            cb(action);
        }
    }
}

/// Debug snapshot of native `NSMenu` structure used by `--ui-smoke-test`.
#[derive(Debug, Clone)]
pub struct MenuDebugInfo {
    pub top_level_items: isize,
    pub claude_row_text: String,
    pub codex_row_text: String,
}

/// Reused native `NSMenu` dropdown matching the compact MVP v1 specification.
pub struct DropdownMenu {
    menu: Retained<NSMenu>,
    _target: Retained<MenuTarget>,
    claude_item: Retained<NSMenuItem>,
    codex_item: Retained<NSMenuItem>,
    status_item: Retained<NSMenuItem>,
    refresh_item: Retained<NSMenuItem>,
    interval_items: Vec<(u64, Retained<NSMenuItem>)>,
    show_claude_item: Retained<NSMenuItem>,
    show_codex_item: Retained<NSMenuItem>,
    launch_at_login_item: Retained<NSMenuItem>,
    last_snapshot: Option<UiSnapshot>,
}

impl DropdownMenu {
    pub fn new<F>(mtm: MainThreadMarker, on_action: F) -> Self
    where
        F: FnMut(MenuAction) + 'static,
    {
        let target = MenuTarget::new(mtm, Box::new(on_action));

        let menu = NSMenu::initWithTitle(mtm.alloc(), &NSString::from_str("Ebb"));
        menu.setAutoenablesItems(false);

        // Header
        let header = make_static_item(mtm, "Ebb");
        menu.addItem(&header);
        menu.addItem(&NSMenuItem::separatorItem(mtm));

        // Provider rows (Claude -> Codex)
        let claude_item = make_static_item(mtm, "");
        let codex_item = make_static_item(mtm, "");
        menu.addItem(&claude_item);
        menu.addItem(&codex_item);

        // Subtle optional freshness/status footer
        let status_item = make_static_item(mtm, "");
        status_item.setHidden(true);
        menu.addItem(&status_item);

        menu.addItem(&NSMenuItem::separatorItem(mtm));

        // Refresh Now
        let refresh_item = make_action_item(mtm, "Refresh Now", "r", &target, sel!(onRefreshNow:));
        menu.addItem(&refresh_item);

        // Settings submenu (lightweight native alternative to a separate preferences window)
        let settings_item = make_static_item(mtm, "Settings");
        settings_item.setEnabled(true);
        let settings_menu = NSMenu::initWithTitle(mtm.alloc(), &NSString::from_str("Settings"));
        settings_menu.setAutoenablesItems(false);

        let interval_parent = make_static_item(mtm, "Refresh Interval");
        interval_parent.setEnabled(true);
        let interval_menu =
            NSMenu::initWithTitle(mtm.alloc(), &NSString::from_str("Refresh Interval"));
        interval_menu.setAutoenablesItems(false);

        let interval_selectors = [
            sel!(onSetInterval30:),
            sel!(onSetInterval60:),
            sel!(onSetInterval120:),
            sel!(onSetInterval300:),
        ];
        let mut interval_items = Vec::with_capacity(INTERVAL_PRESETS.len());
        for (&secs, sel) in INTERVAL_PRESETS.iter().zip(interval_selectors) {
            let label = format_interval_label(secs);
            let item = make_action_item(mtm, &label, "", &target, sel);
            interval_menu.addItem(&item);
            interval_items.push((secs, item));
        }
        interval_parent.setSubmenu(Some(&interval_menu));
        settings_menu.addItem(&interval_parent);
        settings_menu.addItem(&NSMenuItem::separatorItem(mtm));

        let show_claude_item =
            make_action_item(mtm, "Show Claude", "", &target, sel!(onToggleClaude:));
        let show_codex_item =
            make_action_item(mtm, "Show Codex", "", &target, sel!(onToggleCodex:));
        settings_menu.addItem(&show_claude_item);
        settings_menu.addItem(&show_codex_item);
        settings_menu.addItem(&NSMenuItem::separatorItem(mtm));

        let launch_at_login_item = make_action_item(
            mtm,
            "Launch at Login",
            "",
            &target,
            sel!(onToggleLaunchAtLogin:),
        );
        settings_menu.addItem(&launch_at_login_item);
        settings_menu.addItem(&NSMenuItem::separatorItem(mtm));

        let open_folder_item = make_action_item(
            mtm,
            "Open Config Folder",
            "",
            &target,
            sel!(onOpenConfigFolder:),
        );
        settings_menu.addItem(&open_folder_item);

        settings_item.setSubmenu(Some(&settings_menu));
        menu.addItem(&settings_item);

        // Quit
        let quit_item = make_action_item(mtm, "Quit", "q", &target, sel!(onQuit:));
        menu.addItem(&quit_item);

        Self {
            menu,
            _target: target,
            claude_item,
            codex_item,
            status_item,
            refresh_item,
            interval_items,
            show_claude_item,
            show_codex_item,
            launch_at_login_item,
            last_snapshot: None,
        }
    }

    pub fn ns_menu(&self) -> &NSMenu {
        &self.menu
    }

    /// Update reused `NSMenuItem`s in-place strictly when `UiSnapshot` changed.
    /// Returns `true` if AppKit was mutated.
    pub fn apply(&mut self, snapshot: &UiSnapshot) -> bool {
        if self.last_snapshot.as_ref() == Some(snapshot) {
            return false;
        }

        update_provider_row(&self.claude_item, &snapshot.claude_row);
        update_provider_row(&self.codex_item, &snapshot.codex_row);

        match snapshot.status_line.as_deref() {
            Some(line) => {
                let attr = make_status_footer_title(line);
                self.status_item.setAttributedTitle(Some(&attr));
                self.status_item.setHidden(false);
            }
            None => {
                self.status_item.setHidden(true);
            }
        }

        if snapshot.is_refreshing {
            self.refresh_item
                .setTitle(&NSString::from_str("Refreshing…"));
            self.refresh_item.setEnabled(false);
        } else {
            self.refresh_item
                .setTitle(&NSString::from_str("Refresh Now"));
            self.refresh_item.setEnabled(true);
        }

        for (secs, item) in &self.interval_items {
            item.setState(control_state(*secs == snapshot.refresh_interval_seconds));
        }
        self.show_claude_item
            .setState(control_state(snapshot.show_claude));
        self.show_codex_item
            .setState(control_state(snapshot.show_codex));
        self.launch_at_login_item
            .setState(control_state(snapshot.launch_at_login));

        self.last_snapshot = Some(snapshot.clone());
        true
    }

    pub fn debug_inspect(&self) -> MenuDebugInfo {
        let claude_row_text = self
            .claude_item
            .attributedTitle()
            .map(|a| a.string().to_string())
            .unwrap_or_default();
        let codex_row_text = self
            .codex_item
            .attributedTitle()
            .map(|a| a.string().to_string())
            .unwrap_or_default();
        MenuDebugInfo {
            top_level_items: self.menu.numberOfItems(),
            claude_row_text,
            codex_row_text,
        }
    }
}

fn control_state(checked: bool) -> isize {
    if checked {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    }
}

fn make_static_item(mtm: MainThreadMarker, title: &str) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc(),
            &NSString::from_str(title),
            None,
            &NSString::from_str(""),
        )
    };
    item.setEnabled(false);
    item
}

fn make_action_item(
    mtm: MainThreadMarker,
    title: &str,
    key_equivalent: &str,
    target: &MenuTarget,
    action: objc2::runtime::Sel,
) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc(),
            &NSString::from_str(title),
            Some(action),
            &NSString::from_str(key_equivalent),
        )
    };
    let target_obj: &AnyObject = target;
    unsafe {
        item.setTarget(Some(target_obj));
    }
    item.setEnabled(true);
    item
}

fn update_provider_row(item: &NSMenuItem, row: &ProviderRowViewModel) {
    let attr = build_provider_row_attributed_title(row);
    item.setAttributedTitle(Some(&attr));
    item.setToolTip(Some(&NSString::from_str(&row.tooltip)));
    item.setHidden(!row.visible);
}

fn build_provider_row_attributed_title(
    row: &ProviderRowViewModel,
) -> Retained<NSMutableAttributedString> {
    let empty_opts = NSDictionary::new();
    let right_tab = unsafe {
        NSTextTab::initWithTextAlignment_location_options(
            NSTextTab::alloc(),
            NSTextAlignment::Right,
            RIGHT_TAB_LOCATION_PT,
            &empty_opts,
        )
    };
    let paragraph = NSMutableParagraphStyle::new();
    let tab_stops = NSArray::from_retained_slice(&[right_tab]);
    paragraph.setTabStops(Some(&tab_stops));

    let label_font = NSFont::menuFontOfSize(DROPDOWN_FONT_SIZE);
    let value_font = unsafe {
        NSFont::monospacedDigitSystemFontOfSize_weight(DROPDOWN_FONT_SIZE, NSFontWeightRegular)
    };

    let dot_color = provider_dynamic_color(row.provider, row.stale);
    let label_color = NSColor::labelColor();
    let value_color = if row.stale {
        NSColor::secondaryLabelColor()
    } else {
        NSColor::labelColor()
    };

    let combined = NSMutableAttributedString::new();
    combined.appendAttributedString(&make_styled_run("● ", &label_font, &dot_color, &paragraph));
    combined.appendAttributedString(&make_styled_run(
        &format!("{}\t", row.provider.display_name()),
        &label_font,
        &label_color,
        &paragraph,
    ));
    combined.appendAttributedString(&make_styled_run(
        &row.value_text,
        &value_font,
        &value_color,
        &paragraph,
    ));
    combined
}

fn make_status_footer_title(text: &str) -> Retained<NSAttributedString> {
    let font = NSFont::menuFontOfSize(STATUS_FONT_SIZE);
    let color = NSColor::secondaryLabelColor();
    let ns_text = NSString::from_str(text);

    let font_key = unsafe { NSFontAttributeName };
    let color_key = unsafe { NSForegroundColorAttributeName };
    let font_obj: &AnyObject = &font;
    let color_obj: &AnyObject = &color;
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

fn make_styled_run(
    text: &str,
    font: &NSFont,
    color: &NSColor,
    paragraph: &NSMutableParagraphStyle,
) -> Retained<NSAttributedString> {
    let ns_text = NSString::from_str(text);
    let font_key = unsafe { NSFontAttributeName };
    let color_key = unsafe { NSForegroundColorAttributeName };
    let para_key = unsafe { NSParagraphStyleAttributeName };

    let font_obj: &AnyObject = font;
    let color_obj: &AnyObject = color;
    let para_obj: &AnyObject = paragraph;
    let attrs = NSDictionary::<NSString, AnyObject>::from_slices(
        &[font_key, color_key, para_key],
        &[font_obj, color_obj, para_obj],
    );

    unsafe {
        NSAttributedString::initWithString_attributes(
            NSAttributedString::alloc(),
            &ns_text,
            Some(&attrs),
        )
    }
}
