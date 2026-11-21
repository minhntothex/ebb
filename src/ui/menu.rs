use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, sel};
use objc2_app_kit::{
    NSColor, NSControlStateValueOff, NSControlStateValueOn, NSFont, NSFontWeightRegular,
    NSFontWeightSemibold, NSLineBreakMode, NSMenu, NSMenuItem, NSTextAlignment, NSTextField,
    NSView,
};
use objc2_foundation::{NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};

use crate::config::{CRITICAL_THRESHOLD_PRESETS, WARNING_THRESHOLD_PRESETS};
use crate::ui::status_bar::provider_dynamic_color;
use crate::ui::view_model::{
    INTERVAL_PRESETS, ProviderRowViewModel, UiSnapshot, format_interval_label,
};

const MENU_WIDTH_PT: f64 = 264.0;
const OUTER_PAD_X: f64 = 14.0;
const DOT_WIDTH_PT: f64 = 14.0;
/// Left X coordinate shared by provider name (`Claude`/`Codex`) and `Reset in` for exact vertical alignment.
const PROVIDER_TEXT_X: f64 = OUTER_PAD_X + DOT_WIDTH_PT;
const RIGHT_EDGE_X: f64 = MENU_WIDTH_PT - OUTER_PAD_X;

const HEADER_ROW_HEIGHT: f64 = 24.0;
const PROVIDER_ROW_HEIGHT: f64 = 22.0;
const RESET_ROW_HEIGHT: f64 = 18.0;

const DROPDOWN_FONT_SIZE: f64 = 13.0;
const SUBLINE_FONT_SIZE: f64 = 11.0;

/// Actions emitted by the native `NSMenu` dropdown on the AppKit main thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    RefreshNow,
    SetIntervalSeconds(u64),
    ToggleShowClaude,
    ToggleShowCodex,
    ToggleShowResetTime,
    ToggleAlerts,
    ToggleNotifyOnRefresh,
    SetWarningThreshold(u8),
    SetCriticalThreshold(u8),
    ToggleLaunchAtLogin,
    OpenConfigFolder,
    CheckForUpdates,
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

        #[unsafe(method(onToggleShowResetTime:))]
        fn on_toggle_show_reset_time(&self, _sender: Option<&AnyObject>) {
            self.dispatch(MenuAction::ToggleShowResetTime);
        }

        #[unsafe(method(onToggleAlerts:))]
        fn on_toggle_alerts(&self, _sender: Option<&AnyObject>) {
            self.dispatch(MenuAction::ToggleAlerts);
        }

        #[unsafe(method(onToggleNotifyOnRefresh:))]
        fn on_toggle_notify_on_refresh(&self, _sender: Option<&AnyObject>) {
            self.dispatch(MenuAction::ToggleNotifyOnRefresh);
        }

        #[unsafe(method(onSetWarning30:))]
        fn on_set_warning_30(&self, _sender: Option<&AnyObject>) {
            self.dispatch(MenuAction::SetWarningThreshold(30));
        }

        #[unsafe(method(onSetWarning25:))]
        fn on_set_warning_25(&self, _sender: Option<&AnyObject>) {
            self.dispatch(MenuAction::SetWarningThreshold(25));
        }

        #[unsafe(method(onSetWarning20:))]
        fn on_set_warning_20(&self, _sender: Option<&AnyObject>) {
            self.dispatch(MenuAction::SetWarningThreshold(20));
        }

        #[unsafe(method(onSetWarning15:))]
        fn on_set_warning_15(&self, _sender: Option<&AnyObject>) {
            self.dispatch(MenuAction::SetWarningThreshold(15));
        }

        #[unsafe(method(onSetWarning10:))]
        fn on_set_warning_10(&self, _sender: Option<&AnyObject>) {
            self.dispatch(MenuAction::SetWarningThreshold(10));
        }

        #[unsafe(method(onSetCritical10:))]
        fn on_set_critical_10(&self, _sender: Option<&AnyObject>) {
            self.dispatch(MenuAction::SetCriticalThreshold(10));
        }

        #[unsafe(method(onSetCritical5:))]
        fn on_set_critical_5(&self, _sender: Option<&AnyObject>) {
            self.dispatch(MenuAction::SetCriticalThreshold(5));
        }

        #[unsafe(method(onSetCritical3:))]
        fn on_set_critical_3(&self, _sender: Option<&AnyObject>) {
            self.dispatch(MenuAction::SetCriticalThreshold(3));
        }

        #[unsafe(method(onToggleLaunchAtLogin:))]
        fn on_toggle_launch_at_login(&self, _sender: Option<&AnyObject>) {
            self.dispatch(MenuAction::ToggleLaunchAtLogin);
        }

        #[unsafe(method(onOpenConfigFolder:))]
        fn on_open_config_folder(&self, _sender: Option<&AnyObject>) {
            self.dispatch(MenuAction::OpenConfigFolder);
        }

        #[unsafe(method(onCheckForUpdates:))]
        fn on_check_for_updates(&self, _sender: Option<&AnyObject>) {
            self.dispatch(MenuAction::CheckForUpdates);
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

/// Reused native `NSView` holding flush-left and flush-right `NSTextField` labels for crisp alignment.
struct HeaderRowHandles {
    _item: Retained<NSMenuItem>,
    _view: Retained<NSView>,
    right_label: Retained<NSTextField>,
}

struct ProviderRowHandles {
    item: Retained<NSMenuItem>,
    view: Retained<NSView>,
    dot_label: Retained<NSTextField>,
    name_label: Retained<NSTextField>,
    value_label: Retained<NSTextField>,
}

struct ResetRowHandles {
    item: Retained<NSMenuItem>,
    _view: Retained<NSView>,
    left_label: Retained<NSTextField>,
    value_label: Retained<NSTextField>,
}

/// Debug snapshot of native `NSMenu` structure used by `--ui-smoke-test`.
#[derive(Debug, Clone)]
pub struct MenuDebugInfo {
    pub top_level_items: isize,
    pub header_right_text: String,
    pub claude_row_text: String,
    pub claude_reset_text: Option<String>,
    pub codex_row_text: String,
    pub codex_reset_text: Option<String>,
    pub provider_name_x: f64,
    pub reset_label_x: f64,
    pub provider_value_right_x: f64,
    pub reset_value_right_x: f64,
}

/// Reused native `NSMenu` dropdown matching the compact Ebb specification.
pub struct DropdownMenu {
    menu: Retained<NSMenu>,
    _target: Retained<MenuTarget>,
    header_row: HeaderRowHandles,
    claude_row: ProviderRowHandles,
    claude_reset_row: ResetRowHandles,
    codex_row: ProviderRowHandles,
    codex_reset_row: ResetRowHandles,
    refresh_item: Retained<NSMenuItem>,
    interval_items: Vec<(u64, Retained<NSMenuItem>)>,
    show_claude_item: Retained<NSMenuItem>,
    show_codex_item: Retained<NSMenuItem>,
    show_reset_time_item: Retained<NSMenuItem>,
    alerts_enabled_item: Retained<NSMenuItem>,
    notify_on_refresh_item: Retained<NSMenuItem>,
    warning_threshold_items: Vec<(u8, Retained<NSMenuItem>)>,
    critical_threshold_items: Vec<(u8, Retained<NSMenuItem>)>,
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
        menu.setMinimumWidth(MENU_WIDTH_PT);

        // Header row: `Ebb` (left) + `Updated ...` (flush-right)
        let header_row = make_header_row(mtm);
        menu.addItem(&header_row._item);
        menu.addItem(&NSMenuItem::separatorItem(mtm));

        // Provider rows (Claude -> Codex) with aligned `Reset in` sub-rows
        let claude_row = make_provider_row(mtm, "Claude");
        let claude_reset_row = make_reset_row(mtm);
        let codex_row = make_provider_row(mtm, "Codex");
        let codex_reset_row = make_reset_row(mtm);

        menu.addItem(&claude_row.item);
        menu.addItem(&claude_reset_row.item);
        menu.addItem(&codex_row.item);
        menu.addItem(&codex_reset_row.item);

        menu.addItem(&NSMenuItem::separatorItem(mtm));

        // Refresh Now
        let refresh_item = make_action_item(mtm, "Refresh Now", "r", &target, sel!(onRefreshNow:));
        menu.addItem(&refresh_item);

        // Settings submenu
        let settings_item = make_static_item(mtm, "Settings");
        settings_item.setEnabled(true);
        let settings_menu = NSMenu::initWithTitle(mtm.alloc(), &NSString::from_str("Settings"));
        settings_menu.setAutoenablesItems(false);

        // Refresh Interval submenu
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

        // Display toggles
        let show_claude_item =
            make_action_item(mtm, "Show Claude", "", &target, sel!(onToggleClaude:));
        let show_codex_item =
            make_action_item(mtm, "Show Codex", "", &target, sel!(onToggleCodex:));
        let show_reset_time_item = make_action_item(
            mtm,
            "Show Reset Times",
            "",
            &target,
            sel!(onToggleShowResetTime:),
        );
        settings_menu.addItem(&show_claude_item);
        settings_menu.addItem(&show_codex_item);
        settings_menu.addItem(&show_reset_time_item);
        settings_menu.addItem(&NSMenuItem::separatorItem(mtm));

        // Low Quota Alerts submenu (BL-002)
        let alerts_parent = make_static_item(mtm, "Low Quota Alerts");
        alerts_parent.setEnabled(true);
        let alerts_menu =
            NSMenu::initWithTitle(mtm.alloc(), &NSString::from_str("Low Quota Alerts"));
        alerts_menu.setAutoenablesItems(false);

        let alerts_enabled_item =
            make_action_item(mtm, "Enabled", "", &target, sel!(onToggleAlerts:));
        let notify_on_refresh_item = make_action_item(
            mtm,
            "Notify on Reset",
            "",
            &target,
            sel!(onToggleNotifyOnRefresh:),
        );
        alerts_menu.addItem(&alerts_enabled_item);
        alerts_menu.addItem(&notify_on_refresh_item);
        alerts_menu.addItem(&NSMenuItem::separatorItem(mtm));

        let warning_parent = make_static_item(mtm, "Warning Threshold");
        warning_parent.setEnabled(true);
        let warning_menu =
            NSMenu::initWithTitle(mtm.alloc(), &NSString::from_str("Warning Threshold"));
        warning_menu.setAutoenablesItems(false);

        let warning_selectors = [
            sel!(onSetWarning30:),
            sel!(onSetWarning25:),
            sel!(onSetWarning20:),
            sel!(onSetWarning15:),
            sel!(onSetWarning10:),
        ];
        let mut warning_threshold_items = Vec::with_capacity(WARNING_THRESHOLD_PRESETS.len());
        for (&pct, sel) in WARNING_THRESHOLD_PRESETS.iter().zip(warning_selectors) {
            let item = make_action_item(mtm, &format!("{pct}%"), "", &target, sel);
            warning_menu.addItem(&item);
            warning_threshold_items.push((pct, item));
        }
        warning_parent.setSubmenu(Some(&warning_menu));
        alerts_menu.addItem(&warning_parent);

        let critical_parent = make_static_item(mtm, "Critical Threshold");
        critical_parent.setEnabled(true);
        let critical_menu =
            NSMenu::initWithTitle(mtm.alloc(), &NSString::from_str("Critical Threshold"));
        critical_menu.setAutoenablesItems(false);

        let critical_selectors = [
            sel!(onSetCritical10:),
            sel!(onSetCritical5:),
            sel!(onSetCritical3:),
        ];
        let mut critical_threshold_items = Vec::with_capacity(CRITICAL_THRESHOLD_PRESETS.len());
        for (&pct, sel) in CRITICAL_THRESHOLD_PRESETS.iter().zip(critical_selectors) {
            let item = make_action_item(mtm, &format!("{pct}%"), "", &target, sel);
            critical_menu.addItem(&item);
            critical_threshold_items.push((pct, item));
        }
        critical_parent.setSubmenu(Some(&critical_menu));
        alerts_menu.addItem(&critical_parent);

        alerts_parent.setSubmenu(Some(&alerts_menu));
        settings_menu.addItem(&alerts_parent);
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

        let check_updates_item = make_action_item(
            mtm,
            "Check for Updates…",
            "",
            &target,
            sel!(onCheckForUpdates:),
        );
        settings_menu.addItem(&check_updates_item);

        settings_item.setSubmenu(Some(&settings_menu));
        menu.addItem(&settings_item);

        // Quit
        let quit_item = make_action_item(mtm, "Quit", "q", &target, sel!(onQuit:));
        menu.addItem(&quit_item);

        Self {
            menu,
            _target: target,
            header_row,
            claude_row,
            claude_reset_row,
            codex_row,
            codex_reset_row,
            refresh_item,
            interval_items,
            show_claude_item,
            show_codex_item,
            show_reset_time_item,
            alerts_enabled_item,
            notify_on_refresh_item,
            warning_threshold_items,
            critical_threshold_items,
            launch_at_login_item,
            last_snapshot: None,
        }
    }

    pub fn ns_menu(&self) -> &NSMenu {
        &self.menu
    }

    /// Update reused `NSMenuItem`s and custom `NSView`s in-place strictly when `UiSnapshot` changed.
    /// Returns `true` if AppKit was mutated.
    pub fn apply(&mut self, snapshot: &UiSnapshot) -> bool {
        if self.last_snapshot.as_ref() == Some(snapshot) {
            return false;
        }

        let status_text = snapshot.status_line.as_deref().unwrap_or("");
        self.header_row
            .right_label
            .setStringValue(&NSString::from_str(status_text));

        update_provider_row(
            &self.claude_row,
            &self.claude_reset_row,
            &snapshot.claude_row,
        );
        update_provider_row(&self.codex_row, &self.codex_reset_row, &snapshot.codex_row);

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
        self.show_reset_time_item
            .setState(control_state(snapshot.show_reset_time));

        self.alerts_enabled_item
            .setState(control_state(snapshot.alerts_enabled));
        self.notify_on_refresh_item
            .setState(control_state(snapshot.notify_on_refresh));
        self.notify_on_refresh_item
            .setEnabled(snapshot.alerts_enabled);
        for &(pct, ref item) in &self.warning_threshold_items {
            item.setState(control_state(pct == snapshot.alert_warning_threshold));
            item.setEnabled(snapshot.alerts_enabled);
        }
        for &(pct, ref item) in &self.critical_threshold_items {
            item.setState(control_state(pct == snapshot.alert_critical_threshold));
            item.setEnabled(snapshot.alerts_enabled && pct < snapshot.alert_warning_threshold);
        }

        self.launch_at_login_item
            .setState(control_state(snapshot.launch_at_login));

        self.last_snapshot = Some(snapshot.clone());
        true
    }

    pub fn debug_inspect(&self) -> MenuDebugInfo {
        let header_right_text = self.header_row.right_label.stringValue().to_string();
        let claude_row_text = format!(
            "● {}\t{}",
            self.claude_row.name_label.stringValue(),
            self.claude_row.value_label.stringValue()
        );
        let claude_reset_text = if self.claude_reset_row.item.isHidden() {
            None
        } else {
            Some(format!(
                "{}\t{}",
                self.claude_reset_row.left_label.stringValue(),
                self.claude_reset_row.value_label.stringValue()
            ))
        };
        let codex_row_text = format!(
            "● {}\t{}",
            self.codex_row.name_label.stringValue(),
            self.codex_row.value_label.stringValue()
        );
        let codex_reset_text = if self.codex_reset_row.item.isHidden() {
            None
        } else {
            Some(format!(
                "{}\t{}",
                self.codex_reset_row.left_label.stringValue(),
                self.codex_reset_row.value_label.stringValue()
            ))
        };

        let p_name_frame = self.claude_row.name_label.frame();
        let r_left_frame = self.claude_reset_row.left_label.frame();
        let p_val_frame = self.claude_row.value_label.frame();
        let r_val_frame = self.claude_reset_row.value_label.frame();

        MenuDebugInfo {
            top_level_items: self.menu.numberOfItems(),
            header_right_text,
            claude_row_text,
            claude_reset_text,
            codex_row_text,
            codex_reset_text,
            provider_name_x: p_name_frame.origin.x,
            reset_label_x: r_left_frame.origin.x,
            provider_value_right_x: p_val_frame.origin.x + p_val_frame.size.width,
            reset_value_right_x: r_val_frame.origin.x + r_val_frame.size.width,
        }
    }
}

fn make_header_row(mtm: MainThreadMarker) -> HeaderRowHandles {
    let view = NSView::initWithFrame(
        mtm.alloc(),
        NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(MENU_WIDTH_PT, HEADER_ROW_HEIGHT),
        ),
    );

    let left_w = 56.0;
    let left_label = make_label(
        mtm,
        "Ebb",
        NSRect::new(NSPoint::new(OUTER_PAD_X, 3.0), NSSize::new(left_w, 18.0)),
        &NSFont::systemFontOfSize_weight(DROPDOWN_FONT_SIZE, unsafe { NSFontWeightSemibold }),
        &NSColor::labelColor(),
        NSTextAlignment::Left,
    );

    let right_x = OUTER_PAD_X + left_w;
    let right_w = RIGHT_EDGE_X - right_x;
    let right_label = make_label(
        mtm,
        "",
        NSRect::new(NSPoint::new(right_x, 4.0), NSSize::new(right_w, 15.0)),
        &NSFont::menuFontOfSize(SUBLINE_FONT_SIZE),
        &NSColor::secondaryLabelColor(),
        NSTextAlignment::Right,
    );

    view.addSubview(&left_label);
    view.addSubview(&right_label);

    let item = make_static_item(mtm, "Ebb");
    item.setView(Some(&view));

    HeaderRowHandles {
        _item: item,
        _view: view,
        right_label,
    }
}

fn make_provider_row(mtm: MainThreadMarker, name: &str) -> ProviderRowHandles {
    let view = NSView::initWithFrame(
        mtm.alloc(),
        NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(MENU_WIDTH_PT, PROVIDER_ROW_HEIGHT),
        ),
    );

    let dot_label = make_label(
        mtm,
        "●",
        NSRect::new(
            NSPoint::new(OUTER_PAD_X, 2.0),
            NSSize::new(DOT_WIDTH_PT, 18.0),
        ),
        &NSFont::menuFontOfSize(DROPDOWN_FONT_SIZE),
        &NSColor::secondaryLabelColor(),
        NSTextAlignment::Left,
    );

    let name_w = 96.0;
    let name_label = make_label(
        mtm,
        name,
        NSRect::new(
            NSPoint::new(PROVIDER_TEXT_X, 2.0),
            NSSize::new(name_w, 18.0),
        ),
        &NSFont::menuFontOfSize(DROPDOWN_FONT_SIZE),
        &NSColor::labelColor(),
        NSTextAlignment::Left,
    );

    let val_x = PROVIDER_TEXT_X + name_w;
    let val_w = RIGHT_EDGE_X - val_x;
    let value_font = unsafe {
        NSFont::monospacedDigitSystemFontOfSize_weight(DROPDOWN_FONT_SIZE, NSFontWeightRegular)
    };
    let value_label = make_label(
        mtm,
        "– / –",
        NSRect::new(NSPoint::new(val_x, 2.0), NSSize::new(val_w, 18.0)),
        &value_font,
        &NSColor::labelColor(),
        NSTextAlignment::Right,
    );

    view.addSubview(&dot_label);
    view.addSubview(&name_label);
    view.addSubview(&value_label);

    let item = make_static_item(mtm, "");
    item.setView(Some(&view));

    ProviderRowHandles {
        item,
        view,
        dot_label,
        name_label,
        value_label,
    }
}

fn make_reset_row(mtm: MainThreadMarker) -> ResetRowHandles {
    let view = NSView::initWithFrame(
        mtm.alloc(),
        NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(MENU_WIDTH_PT, RESET_ROW_HEIGHT),
        ),
    );

    let left_w = 64.0;
    let left_label = make_label(
        mtm,
        "Reset in",
        NSRect::new(
            NSPoint::new(PROVIDER_TEXT_X, 2.0),
            NSSize::new(left_w, 15.0),
        ),
        &NSFont::menuFontOfSize(SUBLINE_FONT_SIZE),
        &NSColor::secondaryLabelColor(),
        NSTextAlignment::Left,
    );

    let val_x = PROVIDER_TEXT_X + left_w;
    let val_w = RIGHT_EDGE_X - val_x;
    let val_font = unsafe {
        NSFont::monospacedDigitSystemFontOfSize_weight(SUBLINE_FONT_SIZE, NSFontWeightRegular)
    };
    let value_label = make_label(
        mtm,
        "",
        NSRect::new(NSPoint::new(val_x, 2.0), NSSize::new(val_w, 15.0)),
        &val_font,
        &NSColor::secondaryLabelColor(),
        NSTextAlignment::Right,
    );

    view.addSubview(&left_label);
    view.addSubview(&value_label);

    let item = make_static_item(mtm, "");
    item.setView(Some(&view));
    item.setHidden(true);

    ResetRowHandles {
        item,
        _view: view,
        left_label,
        value_label,
    }
}

fn make_label(
    mtm: MainThreadMarker,
    text: &str,
    frame: NSRect,
    font: &NSFont,
    color: &NSColor,
    alignment: NSTextAlignment,
) -> Retained<NSTextField> {
    let field = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    field.setFrame(frame);
    field.setFont(Some(font));
    field.setTextColor(Some(color));
    field.setAlignment(alignment);
    field.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    field
}

fn update_provider_row(
    handles: &ProviderRowHandles,
    reset_handles: &ResetRowHandles,
    row: &ProviderRowViewModel,
) {
    let dot_color = provider_dynamic_color(row.provider, row.stale);
    handles.dot_label.setTextColor(Some(&dot_color));

    let value_color = if row.stale {
        NSColor::secondaryLabelColor()
    } else {
        NSColor::labelColor()
    };
    handles.value_label.setTextColor(Some(&value_color));
    handles
        .value_label
        .setStringValue(&NSString::from_str(&row.value_text));

    let tooltip = NSString::from_str(&row.tooltip);
    handles.view.setToolTip(Some(&tooltip));
    handles.item.setToolTip(Some(&tooltip));
    handles.item.setHidden(!row.visible);

    match (row.visible, row.reset_value_text.as_deref()) {
        (true, Some(values)) => {
            reset_handles
                .value_label
                .setStringValue(&NSString::from_str(values));
            reset_handles.item.setHidden(false);
        }
        _ => {
            reset_handles.item.setHidden(true);
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
