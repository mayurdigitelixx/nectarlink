// SPDX-License-Identifier: GPL-3.0-or-later
//! Public UserNotifications posting for the running app.
//!
//! The delegate is `UNUserNotificationCenter.delegate` only. This file does
//! not replace `NSApplication`'s delegate and does not run its own run loop.
//! Completion handlers are invoked before any user callback, and no lock is
//! held across a framework call or that callback.

#![allow(unsafe_code)]

use std::{
    cell::RefCell,
    collections::HashMap,
    sync::{
        Mutex, Once, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, ProtocolObject};
use objc2::{AnyThread, define_class, msg_send};
use objc2_foundation::{
    NSArray, NSBundle, NSDictionary, NSError, NSObject, NSObjectProtocol, NSSet, NSString,
};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotification, UNNotificationAction,
    UNNotificationActionOptions, UNNotificationCategory, UNNotificationCategoryOptions,
    UNNotificationInterruptionLevel, UNNotificationPresentationOptions, UNNotificationRequest,
    UNNotificationResponse, UNNotificationSettings, UNNotificationSound, UNTextInputNotificationAction,
    UNTextInputNotificationResponse, UNUserNotificationCenter, UNUserNotificationCenterDelegate,
};

use super::{
    BannerText, NotificationAuthorization, Toast, ToastEvent, admit_post, app_bundle_executable, banner_text,
    begin_authorization_read, begin_authorization_request, emit, end_authorization_request,
    finish_authorization_read, notification_identifier, pending,
};

const MAX_REMOVALS: usize = 256;
const MAX_CATEGORIES: usize = 256;
const MAX_NATIVE_ACTIONS: usize = 4;
const MAX_COPY_CODE_ACTIONS: usize = 256;
pub(super) const LOCAL_COPY_CODE_PREFIX: &str = "nl:local:copy_otp:";
const USER_INFO_DEVICE: &str = "device";
const USER_INFO_KEY: &str = "key";
const USER_INFO_ACTIONS: &str = "actions";
const USER_INFO_REPLY: &str = "reply";

thread_local! {
    /// Objective-C notification categories never leave the existing main-queue scope.
    static CATEGORIES: RefCell<HashMap<String, Retained<UNNotificationCategory>>> = RefCell::new(HashMap::new());
}

/// Rust-only, process-memory OTP snapshots. The opaque action token is the key;
/// plaintext is never placed in a notification request or action argument.
type CopyCodeSnapshots = HashMap<(String, String, String), String>;
static COPY_CODE_ACTIONS: OnceLock<Mutex<CopyCodeSnapshots>> = OnceLock::new();
static NEXT_COPY_CODE_ACTION: AtomicU64 = AtomicU64::new(1);

fn copy_code_actions() -> std::sync::MutexGuard<'static, HashMap<(String, String, String), String>> {
    COPY_CODE_ACTIONS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

pub(super) fn register_copy_code_action(device: &str, key: &str, code: &str) -> Option<String> {
    if code.trim().is_empty() {
        return None;
    }
    let sequence = NEXT_COPY_CODE_ACTION.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let action = format!("{LOCAL_COPY_CODE_PREFIX}{:x}-{:x}-{:x}", std::process::id(), nanos, sequence);
    let owner = (device.to_owned(), key.to_owned());
    let mut actions = copy_code_actions();
    actions.retain(|(saved_device, saved_key, _), _| (saved_device, saved_key) != (&owner.0, &owner.1));
    if actions.len() >= MAX_COPY_CODE_ACTIONS {
        return None;
    }
    actions.insert((owner.0, owner.1, action.clone()), code.to_owned());
    Some(action)
}

/// The same unconsumed token may be used by a silent replacement, never reminted.
pub(super) fn existing_copy_code_action(device: &str, key: &str, code: &str) -> Option<String> {
    copy_code_actions().iter().find_map(|((saved_device, saved_key, action), saved_code)| {
        (saved_device == device && saved_key == key && saved_code == code).then(|| action.clone())
    })
}

pub(super) fn invalidate_copy_code_actions(device: &str, key: &str) {
    copy_code_actions().retain(|(saved_device, saved_key, _), _| saved_device != device || saved_key != key);
}

/// Keeps copy-code actions for this notification when a stored snapshot still
/// equals `code`. Otherwise drops every entry for that notification. Does not
/// mint a new action, so a silent snapshot cannot post another banner.
pub(super) fn retain_copy_code_if_unchanged(device: &str, key: &str, code: Option<&str>) -> bool {
    let mut actions = copy_code_actions();
    let unchanged = code.is_some_and(|code| {
        !code.is_empty()
            && actions.iter().any(|((saved_device, saved_key, _), saved)| {
                saved_device == device && saved_key == key && saved == code
            })
    });
    if unchanged {
        return true;
    }
    actions.retain(|(saved_device, saved_key, _), _| saved_device != device || saved_key != key);
    false
}

pub(super) fn invalidate_device_copy_code_actions(device: &str) {
    copy_code_actions().retain(|(saved_device, _, _), _| saved_device != device);
}

pub(super) fn take_copy_code_action(device: &str, key: &str, action: &str) -> Option<String> {
    if !action.starts_with(LOCAL_COPY_CODE_PREFIX) {
        return None;
    }
    copy_code_actions().remove(&(device.to_owned(), key.to_owned(), action.to_owned()))
}

struct DelegateIvars {
    _unused: u8,
}

impl std::fmt::Debug for DelegateIvars {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DelegateIvars").finish_non_exhaustive()
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = AnyThread]
    #[name = "DesktopUserNotificationDelegate"]
    #[ivars = DelegateIvars]
    struct CenterDelegate;

    unsafe impl NSObjectProtocol for CenterDelegate {}

    unsafe impl UNUserNotificationCenterDelegate for CenterDelegate {
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn user_notification_center_will_present_notification(
            &self,
            _center: &UNUserNotificationCenter,
            notification: &UNNotification,
            completion_handler: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            let options =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| presentation_options(notification)))
                    .unwrap_or(
                        UNNotificationPresentationOptions::Banner | UNNotificationPresentationOptions::List,
                    );
            completion_handler.call((options,));
        }

        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn user_notification_center_did_receive_response(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            completion_handler: &block2::DynBlock<dyn Fn()>,
        ) {
            let event = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| response_event(response)))
                .ok()
                .flatten();
            completion_handler.call(());
            // Delegate callbacks are not guaranteed to be on Qt's thread.
            // `on_toast` runs on the main queue after this handler returns.
            if let Some(event) = event {
                on_main(move || {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| emit(event)));
                });
            }
        }
    }
);

impl CenterDelegate {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(DelegateIvars { _unused: 0 });
        unsafe { msg_send![super(this), init] }
    }
}

static DELEGATE: OnceLock<Retained<CenterDelegate>> = OnceLock::new();

/// Every entry point checks this before touching `UNUserNotificationCenter`.
/// Unbundled, nothing is posted and authorization stays unknown.
fn bundled() -> bool {
    static BUNDLED: OnceLock<bool> = OnceLock::new();
    *BUNDLED.get_or_init(|| {
        let exe = std::env::current_exe().is_ok_and(|exe| app_bundle_executable(&exe));
        let bundle = NSBundle::mainBundle();
        let app = bundle.bundlePath().to_string().ends_with(".app");
        let identity = bundle.bundleIdentifier().is_some_and(|id| id.length() > 0);
        let bundled = exe && app && identity;
        if !bundled {
            tracing::info!("not running from an app bundle; system notifications are off");
        }
        bundled
    })
}

pub(super) fn install_delegate() {
    if !bundled() {
        return;
    }
    on_main(install_delegate_now);
}

pub(super) fn refresh_authorization() {
    if !bundled() {
        return;
    }
    let generation = begin_authorization_read();
    on_main(move || {
        let center = UNUserNotificationCenter::currentNotificationCenter();
        let block = RcBlock::new(move |settings: std::ptr::NonNull<UNNotificationSettings>| {
            // SAFETY: the framework passes a live settings object for this call.
            let settings = unsafe { settings.as_ref() };
            finish_authorization_read(generation, authorization_from_settings(settings));
        });
        center.getNotificationSettingsWithCompletionHandler(&block);
    });
}

pub(super) fn request_authorization() {
    if !bundled() || begin_authorization_request() {
        return;
    }
    on_main(|| {
        let center = UNUserNotificationCenter::currentNotificationCenter();
        let block = RcBlock::new(move |_granted: Bool, error: *mut NSError| {
            if !error.is_null() {
                // SAFETY: a non-null error is borrowed for this callback. Only the
                // numeric code is read; the description can contain a name.
                let code = unsafe { &*error }.code();
                tracing::warn!(code, "notification authorization was not changed");
            }
            end_authorization_request();
            refresh_authorization();
        });
        center.requestAuthorizationWithOptions_completionHandler(
            UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
            &block,
        );
    });
}

pub(super) fn post(toast: Toast) {
    if !bundled() {
        return;
    }
    let banner = banner_text(&toast);
    let silent = toast.silent;
    let ticket = pending().begin(&banner.device, &banner.identifier);
    let Some(ticket) = ticket else {
        tracing::warn!("notification was not posted; too many are waiting");
        return;
    };
    let generation = begin_authorization_read();
    on_main(move || {
        install_delegate_now();
        let center = UNUserNotificationCenter::currentNotificationCenter();
        let block = RcBlock::new(move |settings: std::ptr::NonNull<UNNotificationSettings>| {
            // SAFETY: the framework passes a live settings object for this call.
            let settings = unsafe { settings.as_ref() };
            let auth = authorization_from_settings(settings);
            finish_authorization_read(generation, auth);
            if !admit_post(auth) {
                pending().refuse(&banner.identifier);
                tracing::debug!(status = auth.label(), "notification was not posted");
                return;
            }
            let banner = banner.clone();
            on_main(move || deliver(&banner, silent, ticket));
        });
        center.getNotificationSettingsWithCompletionHandler(&block);
    });
}

pub(super) fn remove(device: &str, key: &str) {
    if !bundled() {
        return;
    }
    let identifier = notification_identifier(device, key);
    invalidate_copy_code_actions(device, key);
    pending().cancel(&identifier);
    on_main(move || withdraw(&[NSString::from_str(&identifier)]));
}

pub(super) fn remove_device(device: &str) {
    if !bundled() {
        return;
    }
    let device = device.to_string();
    invalidate_device_copy_code_actions(&device);
    let cut = pending().cancel_device(&device, 2);
    on_main(move || {
        let center = UNUserNotificationCenter::currentNotificationCenter();
        let delivered_device = device.clone();
        let delivered = RcBlock::new(move |notes: std::ptr::NonNull<NSArray<UNNotification>>| {
            // SAFETY: the framework passes a live array for this call.
            let notes = unsafe { notes.as_ref() };
            let ids = delivered_identifiers(notes, &delivered_device);
            on_main(move || sweep(&ids, cut));
        });
        center.getDeliveredNotificationsWithCompletionHandler(&delivered);
        let pending_device = device;
        let queued = RcBlock::new(move |notes: std::ptr::NonNull<NSArray<UNNotificationRequest>>| {
            // SAFETY: the framework passes a live array for this call.
            let notes = unsafe { notes.as_ref() };
            let ids = pending_identifiers(notes, &pending_device);
            on_main(move || sweep(&ids, cut));
        });
        center.getPendingNotificationRequestsWithCompletionHandler(&queued);
    });
}

/// Runs on the main queue, so no add can land between the check and the withdraw.
fn sweep(identifiers: &[String], cut: u64) {
    let ids: Vec<String> = {
        let state = pending();
        identifiers.iter().filter(|id| state.sweep_may_withdraw(id, cut)).cloned().collect()
    };
    let ids: Vec<Retained<NSString>> = ids.iter().map(|id| NSString::from_str(id)).collect();
    withdraw(&ids);
    pending().end_sweep();
}

pub(super) fn unregister() {
    if !bundled() {
        return;
    }
    on_main(|| {
        let center = UNUserNotificationCenter::currentNotificationCenter();
        center.removeAllDeliveredNotifications();
        center.removeAllPendingNotificationRequests();
        clear_categories();
        invalidate_all_copy_code_actions();
    });
}

fn install_delegate_now() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let center = UNUserNotificationCenter::currentNotificationCenter();
        let delegate = CenterDelegate::new();
        let as_delegate = ProtocolObject::<dyn UNUserNotificationCenterDelegate>::from_ref(&*delegate);
        center.setDelegate(Some(as_delegate));
        let _ = DELEGATE.set(delegate);
    });
}

fn on_main(work: impl FnOnce() + Send + 'static) {
    if MainThreadMarker::new().is_some() {
        work();
        return;
    }
    DispatchQueue::main().exec_async(work);
}

fn authorization_from_settings(settings: &UNNotificationSettings) -> NotificationAuthorization {
    NotificationAuthorization::from_system(settings.authorizationStatus().0)
}

/// Runs on the main queue, so admitting and adding cannot interleave with a withdraw.
fn deliver(banner: &BannerText, silent: bool, ticket: u64) {
    if !pending().admit(&banner.identifier, ticket) {
        tracing::debug!("notification was replaced or removed before it was shown");
        return;
    }
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(&banner.title));
    content.setSubtitle(&NSString::from_str(&banner.subtitle));
    content.setBody(&NSString::from_str(&banner.body));
    if !silent {
        content.setSound(Some(&UNNotificationSound::defaultSound()));
    }
    if banner.quiet {
        content.setInterruptionLevel(UNNotificationInterruptionLevel::Passive);
    }
    let category_id = update_category(banner);
    if let Some(category_id) = &category_id {
        content.setCategoryIdentifier(&NSString::from_str(category_id));
    }
    let device_key = NSString::from_str(USER_INFO_DEVICE);
    let key_key = NSString::from_str(USER_INFO_KEY);
    let actions_key = NSString::from_str(USER_INFO_ACTIONS);
    let reply_key = NSString::from_str(USER_INFO_REPLY);
    let device_value = NSString::from_str(&banner.device);
    let key_value = NSString::from_str(&banner.key);
    let action_ids = if category_id.is_some() {
        serde_json::to_string(&native_action_ids(banner)).unwrap_or_else(|_| "[]".to_owned())
    } else {
        "[]".to_owned()
    };
    let action_value = NSString::from_str(&action_ids);
    let reply_id = banner.reply.as_ref().map_or("", |(id, _)| id.as_str());
    let reply_is_presented =
        category_id.is_some() && native_action_ids(banner).iter().any(|id| id == reply_id);
    let reply_value = NSString::from_str(if reply_is_presented { reply_id } else { "" });
    let info = NSDictionary::<NSString, NSString>::from_slices(
        &[&*device_key, &*key_key, &*actions_key, &*reply_key],
        &[&*device_value, &*key_value, &*action_value, &*reply_value],
    );
    // SAFETY: every entry is a string, which is the userInfo type this poster writes.
    unsafe { content.setUserInfo(&*(Retained::as_ptr(&info).cast::<NSDictionary>())) };
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
        &NSString::from_str(&banner.identifier),
        &content,
        None,
    );
    let center = UNUserNotificationCenter::currentNotificationCenter();
    let identifier = banner.identifier.clone();
    let done = RcBlock::new(move |error: *mut NSError| {
        let shown = error.is_null();
        if !shown {
            // SAFETY: a non-null error is borrowed for this callback. Only the code is read.
            let code = unsafe { &*error }.code();
            tracing::warn!(code, "notification wasn't shown");
        }
        let identifier = identifier.clone();
        on_main(move || {
            let withdraw_now = pending().added(&identifier, ticket, shown);
            if withdraw_now {
                withdraw(&[NSString::from_str(&identifier)]);
            }
        });
    });
    center.addNotificationRequest_withCompletionHandler(&request, Some(&done));
}

fn presentation_options(notification: &UNNotification) -> UNNotificationPresentationOptions {
    let content = notification.request().content();
    if content.interruptionLevel() == UNNotificationInterruptionLevel::Passive {
        return UNNotificationPresentationOptions::List;
    }
    let mut options = UNNotificationPresentationOptions::Banner | UNNotificationPresentationOptions::List;
    if content.sound().is_some() {
        options |= UNNotificationPresentationOptions::Sound;
    }
    options
}

fn response_event(response: &UNNotificationResponse) -> Option<ToastEvent> {
    let action = response.actionIdentifier();
    let request = response.notification().request();
    let device = user_info_field(&request.content().userInfo(), USER_INFO_DEVICE)?;
    let key = user_info_field(&request.content().userInfo(), USER_INFO_KEY)?;
    if action.isEqualToString(default_action_identifier()) {
        return Some(ToastEvent::Opened { device, key });
    }
    if action.isEqualToString(dismiss_action_identifier()) {
        return Some(ToastEvent::Dismissed { device, key });
    }
    let info = request.content().userInfo();
    let allowed: Vec<String> = serde_json::from_str(&user_info_field(&info, USER_INFO_ACTIONS)?).ok()?;
    let action = action.to_string();
    if !allowed.iter().any(|allowed| allowed == &action) {
        return None;
    }
    if user_info_field(&info, USER_INFO_REPLY).as_deref() == Some(action.as_str()) {
        // A reply is accepted only from the native text-input response created
        // for this notification's declared RemoteInput action.
        let object = unsafe { &*(response as *const UNNotificationResponse).cast::<AnyObject>() };
        let input = object.downcast_ref::<UNTextInputNotificationResponse>()?;
        let text = input.userText().to_string().trim().to_owned();
        if text.is_empty() {
            return None;
        }
        return Some(ToastEvent::Reply { device, key, action, text });
    }
    Some(ToastEvent::Action { device, key, action })
}

fn native_action_ids(banner: &BannerText) -> Vec<String> {
    if banner.call {
        return Vec::new();
    }
    let action_limit = if banner.reply.is_some() { MAX_NATIVE_ACTIONS - 1 } else { MAX_NATIVE_ACTIONS };
    let mut ids: Vec<String> = banner.actions.iter().take(action_limit).map(|(id, _)| id.clone()).collect();
    if let Some((id, _)) = &banner.reply {
        ids.push(id.clone());
    }
    ids
}

/// Registers a bounded set of per-notification categories and returns its ID.
/// The action IDs remain opaque; the response is checked against the exact
/// notification's saved action list before it reaches the shared sender.
fn update_category(banner: &BannerText) -> Option<String> {
    if banner.call {
        remove_categories(std::slice::from_ref(&banner.identifier));
        return None;
    }
    let mut native_actions: Vec<Retained<UNNotificationAction>> = Vec::new();
    let action_limit = if banner.reply.is_some() { MAX_NATIVE_ACTIONS - 1 } else { MAX_NATIVE_ACTIONS };
    for (id, title) in banner.actions.iter().take(action_limit) {
        let options = if id.starts_with(LOCAL_COPY_CODE_PREFIX) {
            UNNotificationActionOptions::AuthenticationRequired
        } else {
            UNNotificationActionOptions::empty()
        };
        native_actions.push(UNNotificationAction::actionWithIdentifier_title_options(
            &NSString::from_str(id),
            &NSString::from_str(title),
            options,
        ));
    }
    if let Some((id, title)) = &banner.reply {
        let reply_action = UNTextInputNotificationAction::actionWithIdentifier_title_options_textInputButtonTitle_textInputPlaceholder(
            &NSString::from_str(id),
            &NSString::from_str(title),
            UNNotificationActionOptions::empty(),
            &NSString::from_str(title),
            &NSString::from_str(title),
        );
        native_actions.push(reply_action.into_super());
    }
    if native_actions.is_empty() {
        remove_categories(std::slice::from_ref(&banner.identifier));
        return None;
    }
    let category_id = format!("nl-actions:{}", banner.identifier);
    let actions = NSArray::from_retained_slice(&native_actions);
    let category = UNNotificationCategory::categoryWithIdentifier_actions_intentIdentifiers_options(
        &NSString::from_str(&category_id),
        &actions,
        &NSArray::<NSString>::new(),
        UNNotificationCategoryOptions::CustomDismissAction,
    );
    let all = CATEGORIES.with(|registered| {
        let mut registered = registered.borrow_mut();
        if !registered.contains_key(&banner.identifier) && registered.len() >= MAX_CATEGORIES {
            return None;
        }
        registered.insert(banner.identifier.clone(), category);
        Some(registered.values().cloned().collect::<Vec<_>>())
    })?;
    set_registered_categories(all);
    Some(category_id)
}

fn remove_categories(identifiers: &[String]) {
    if identifiers.is_empty() {
        return;
    }
    let all = CATEGORIES.with(|registered| {
        let mut registered = registered.borrow_mut();
        for identifier in identifiers {
            registered.remove(identifier);
        }
        registered.values().cloned().collect::<Vec<_>>()
    });
    set_registered_categories(all);
}

fn set_registered_categories(categories: Vec<Retained<UNNotificationCategory>>) {
    let categories = NSSet::from_retained_slice(&categories);
    UNUserNotificationCenter::currentNotificationCenter().setNotificationCategories(&categories);
}

fn clear_categories() {
    CATEGORIES.with(|registered| registered.borrow_mut().clear());
    set_registered_categories(Vec::new());
}

fn invalidate_all_copy_code_actions() {
    copy_code_actions().clear();
}

fn default_action_identifier() -> &'static NSString {
    // SAFETY: the framework exports this `NSString` for the life of the process.
    unsafe { objc2_user_notifications::UNNotificationDefaultActionIdentifier }
}

fn dismiss_action_identifier() -> &'static NSString {
    // SAFETY: the framework exports this `NSString` for the life of the process.
    unsafe { objc2_user_notifications::UNNotificationDismissActionIdentifier }
}

fn user_info_field(info: &NSDictionary, field: &str) -> Option<String> {
    let key = NSString::from_str(field);
    // SAFETY: `key` is an Objective-C object, which is what `objectForKey:` accepts.
    let key_obj = unsafe { &*Retained::as_ptr(&key).cast::<AnyObject>() };
    let value = info.objectForKey(key_obj)?;
    let text = value.downcast::<NSString>().ok()?.to_string();
    (!text.is_empty()).then_some(text)
}

fn delivered_identifiers(notes: &NSArray<UNNotification>, device: &str) -> Vec<String> {
    matching_identifiers(notes.count(), |index| {
        matching_identifier(&notes.objectAtIndex(index).request(), device)
    })
}

fn pending_identifiers(notes: &NSArray<UNNotificationRequest>, device: &str) -> Vec<String> {
    matching_identifiers(notes.count(), |index| matching_identifier(&notes.objectAtIndex(index), device))
}

/// Scans the whole system list and keeps at most `MAX_REMOVALS` matches.
fn matching_identifiers(count: usize, matching: impl Fn(usize) -> Option<String>) -> Vec<String> {
    let mut ids = Vec::new();
    for index in 0..count {
        if ids.len() == MAX_REMOVALS {
            tracing::warn!("notification removal stopped at the bounded identifier list");
            break;
        }
        if let Some(identifier) = matching(index) {
            ids.push(identifier);
        }
    }
    ids
}

fn matching_identifier(request: &UNNotificationRequest, device: &str) -> Option<String> {
    let owner = user_info_field(&request.content().userInfo(), USER_INFO_DEVICE)?;
    (owner == device).then(|| request.identifier().to_string())
}

fn withdraw(identifiers: &[Retained<NSString>]) {
    if identifiers.is_empty() {
        return;
    }
    let array = NSArray::from_retained_slice(identifiers);
    let center = UNUserNotificationCenter::currentNotificationCenter();
    center.removeDeliveredNotificationsWithIdentifiers(&array);
    center.removePendingNotificationRequestsWithIdentifiers(&array);
    let identifiers: Vec<String> = identifiers.iter().map(|identifier| identifier.to_string()).collect();
    remove_categories(&identifiers);
}
