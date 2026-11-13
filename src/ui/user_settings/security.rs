//! My Account > Password and Authentication, Account Standing and Account Removal with their
//! dialogs (SETTINGS_NOTES §1 Account, FEATURES_NOTES §0.2 / §3.2): Change Password; the
//! authenticator app (password -> QR code and key from the OS RNG -> the app's code -> backup
//! codes, shown once with Copy and Save…) and Remove; SMS backup; security keys (list, rename,
//! remove; adding needs WebAuthn, so the official app); View Backup Codes (password -> emailed
//! key -> codes, Generate New Codes); and the 2FA re-check prompt (`AccountState::mfa`) that
//! Devices and Authorized Apps show too. Disabling or deleting the account only opens
//! discord.com. Typed secrets live in pre-sized `Zeroizing` buffers without undo history and
//! go out as `Secret`s; a dialog's state is dropped, and so wiped, when it closes and with the
//! layer (`AccountForm` reset).
//! OWNERSHIP: account-security engineer.

use super::super::{field_label, finish_op, op_line, op_state, primary};
use super::open_account_page;
use crate::api::account::{BackupCode, CodesNonce, MfaKind, otpauth_uri, totp_secret};
use crate::auth::Secret;
use crate::backend::{AccountCommand, Command, MfaPrompt, OpKind};
use crate::model::{Id, Presence, User};
use crate::state::account::OpState;
use crate::ui::messages::{Pal, icon_button};
use crate::ui::{UiCx, bold, dialog, dialog_buttons, dialog_closable, dialog_footer, dialog_text};
use egui::emath::GuiRounding as _;
use egui::{Align, Color32, Frame, Layout, Margin, Rect, RichText, Sense, Stroke, TextEdit, Vec2, vec2};
use qrcodegen::{QrCode, QrCodeEcc};
use zeroize::Zeroizing;

const PASSWORD_MAX: usize = 128;
/// Private user flag: SMS 2FA is on.
const MFA_SMS: u64 = 1 << 4;
/// The writes these dialogs wait on (stale answers are dropped when one opens or closes).
const KINDS: [OpKind; 6] = [OpKind::ChangePassword, OpKind::CheckTotpPassword, OpKind::EnableTotp, OpKind::SetSmsMfa, OpKind::BackupCodesChallenge, OpKind::ViewBackupCodes];

type Buf = Zeroizing<String>;

/// A pre-sized buffer: typing within its limit never reallocates, so no unwiped copies.
fn buf() -> Buf {
    Zeroizing::new(String::with_capacity(4 * PASSWORD_MAX))
}

/// A zeroized copy for a command; the form keeps its buffer for another try.
fn secret(b: &Buf) -> Secret {
    Secret::new(String::clone(b))
}

/// Password and Authentication dialogs and the 2FA prompt (in `AccountForm`).
#[derive(Default)]
pub(in crate::ui::user_settings) struct Form {
    dialog: Dialog,
    /// `FetchSecurityKeys` went out in this opening.
    keys_fetched: bool,
    /// The 2FA prompt's method (None = closed) and typed code.
    mfa_kind: Option<MfaKind>,
    mfa_code: Buf,
    /// `RUSTCORD_NAV_DEMO=account:…` was applied in this opening.
    demo: bool,
}

#[derive(Default)]
enum Dialog {
    #[default]
    None,
    Password { current: Buf, new: Buf, confirm: Buf },
    /// Authenticator step 1: the password.
    TotpPassword { password: Buf },
    /// Step 2: scan the QR code (or type the key), then the app's code.
    TotpScan { password: Secret, key: Secret, qr: Option<QrCode>, code: Buf },
    Sms { enable: bool, password: Buf },
    RenameKey { id: Id, name: String },
    /// View Backup Codes step 1: the password.
    CodesPassword { password: Buf },
    /// Step 2: the key Discord emailed.
    CodesKey { nonce: CodesNonce, key: Buf },
    /// Backup codes, shown once; `regen` = (emailed key, regenerate nonce) for new codes.
    Codes { codes: Vec<BackupCode>, regen: Option<(Secret, Secret)> },
    /// Disable (false) or Delete (true) Account: done on discord.com.
    Removal { delete: bool },
}

/// One of these dialogs (or the 2FA prompt) is open: Esc is theirs.
pub(super) fn dialog_open(cx: &UiCx<'_>) -> bool {
    !matches!(cx.view.user_settings_ui.account.sec.dialog, Dialog::None) || cx.state.account.mfa.is_some()
}

/// Password and Authentication, Account Standing, Account Removal (Discord's order under the
/// account card) and their dialogs.
pub(super) fn show(ui: &mut egui::Ui, cx: &mut UiCx<'_>, pal: &Pal, me: &User) {
    demo(cx);
    let (ok, warn) = (crate::ui::presence_color(Presence::Online, &cx.theme.colors), crate::ui::presence_color(Presence::Idle, &cx.theme.colors));
    let mut open = None;
    crate::ui::section(ui, cx.theme, "Password and Authentication");
    let totp = me.mfa_enabled;
    if totp {
        chip(ui, pal, Some("✔"), "Multi-factor authentication enabled", ok);
        ui.add_space(8.0);
    }
    if ui.add(primary(cx.theme, "Change Password")).clicked() {
        open = Some(Dialog::Password { current: buf(), new: buf(), confirm: buf() });
    }
    let (text, button) = if totp {
        ("Signing in asks for a code from your app.", "Remove Authenticator App")
    } else {
        ("Protect your account with a code from an app when you sign in.", "Enable Authenticator App")
    };
    if row(ui, pal, "Authenticator App", text, button, totp, Ok(())).clicked() {
        if totp {
            let body = "Signing in stops asking for a code, and your backup codes stop working.";
            cx.view.confirm("Remove authenticator app?", body, "Remove", Command::Account(AccountCommand::DisableTotp));
        } else {
            open = Some(Dialog::TotpPassword { password: buf() });
        }
    }
    if totp {
        let sms = me.flags & MFA_SMS != 0;
        let allowed = if sms || me.phone.is_some() { Ok(()) } else { Err("Add a phone number first.") };
        let button = if sms { "Remove SMS Authentication" } else { "Enable SMS Authentication" };
        if row(ui, pal, "SMS Backup Authentication", "A text message to your phone can stand in for the app.", button, sms, allowed).clicked() {
            open = Some(Dialog::Sms { enable: !sms, password: buf() });
        }
    }
    open = security_keys(ui, cx, pal).or(open);
    if totp && row(ui, pal, "Backup Codes", "One-time codes for when you can't use the app.", "View Backup Codes", false, Ok(())).clicked() {
        open = Some(Dialog::CodesPassword { password: buf() });
    }
    standing(ui, cx, pal, ok, warn);
    open = removal(ui, cx, pal).or(open);

    let form = &mut cx.view.user_settings_ui.account.sec;
    // Codes that arrived after their dialog closed are still shown, once.
    if matches!(form.dialog, Dialog::None)
        && let Some(codes) = cx.state.account.backup_codes.take()
    {
        form.dialog = Dialog::Codes { codes, regen: None };
    }
    if let Some(d) = open {
        reset(cx);
        cx.view.user_settings_ui.account.sec.dialog = d;
    }
    dialogs(ui, cx, me);
}

/// Forget finished answers of these dialogs (pending ones stay until their answer).
fn reset(cx: &mut UiCx<'_>) {
    for kind in KINDS {
        finish_op(cx, kind);
    }
    cx.state.account.backup_codes = None;
    cx.state.account.codes_nonce = None;
}

/// Title and one muted line on the left, a button on the right (`danger`: red outline;
/// `Err`: disabled, the reason as its tooltip).
fn row(ui: &mut egui::Ui, pal: &Pal, title: &str, text: &str, button: &str, danger: bool, allowed: Result<(), &str>) -> egui::Response {
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        crate::ui::button_row(ui);
        let w = (ui.available_width() - 230.0).max(120.0);
        ui.allocate_ui_with_layout(vec2(w, 0.0), Layout::top_down(Align::Min), |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(RichText::new(title).font(bold(pal.size)).color(pal.text));
            ui.add(egui::Label::new(RichText::new(text).size(pal.meta()).color(pal.muted)).wrap());
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let b = if danger {
                egui::Button::new(RichText::new(button).color(pal.danger)).fill(Color32::TRANSPARENT).stroke(Stroke::new(1.0, pal.danger))
            } else {
                egui::Button::new(button)
            };
            let r = ui.add_enabled(allowed.is_ok(), b.min_size(vec2(64.0, 32.0)));
            match allowed {
                Ok(()) => r,
                Err(why) => r.on_disabled_hover_text(why),
            }
        })
        .inner
    })
    .inner
}

/// A rounded status chip: `color` text (after a monochrome `icon`) on a tint of it.
fn chip(ui: &mut egui::Ui, pal: &Pal, icon: Option<&str>, text: &str, color: Color32) {
    Frame::new().fill(color.gamma_multiply(0.15)).corner_radius(pal.radius).inner_margin(Margin::symmetric(10, 5)).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            if let Some(icon) = icon {
                ui.label(RichText::new(icon).family(crate::theme::icon_family()).color(color));
            }
            ui.label(RichText::new(text).color(color));
        });
    });
}

/// Security Keys: the list (fetched once per opening) with Rename and Remove; registering one
/// needs WebAuthn, which only the official app has.
fn security_keys(ui: &mut egui::Ui, cx: &mut UiCx<'_>, pal: &Pal) -> Option<Dialog> {
    if !std::mem::replace(&mut cx.view.user_settings_ui.account.sec.keys_fetched, true) {
        cx.state.account.security_keys = None;
        cx.cmd.send(Command::Account(AccountCommand::FetchSecurityKeys));
    }
    let tip = Err("Registering a key needs WebAuthn: use the official app.");
    row(ui, pal, "Security Keys", "Sign in with a hardware key or passkey.", "Register a Security Key", false, tip);
    let mut open = None;
    match &cx.state.account.security_keys {
        None => {
            ui.add(egui::Spinner::new().size(14.0).color(pal.muted));
        }
        Some(Err(e)) => {
            ui.add(egui::Label::new(RichText::new(e).size(pal.meta()).color(pal.muted)).wrap());
        }
        Some(Ok(keys)) => {
            for k in keys {
                Frame::new().fill(pal.surface).corner_radius(pal.card_radius()).inner_margin(Margin::symmetric(12, 6)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("🔑").family(crate::theme::icon_family()).color(pal.muted));
                        ui.add(egui::Label::new(RichText::new(&k.name).color(pal.text)).truncate());
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if icon_button(ui, pal, "🗑", "Remove", 28.0, false, pal.danger).clicked() {
                                let body = format!("{} can no longer be used to sign in.", k.name);
                                cx.view.confirm("Remove security key?", body, "Remove", Command::Account(AccountCommand::DeleteSecurityKey { id: k.id }));
                            }
                            if icon_button(ui, pal, "✏", "Rename", 28.0, false, pal.text).clicked() {
                                open = Some(Dialog::RenameKey { id: k.id, name: k.name.clone() });
                            }
                        });
                    });
                });
                ui.add_space(4.0);
            }
        }
    }
    open
}

/// Account Standing (`FetchStanding` once per opening of the page, read-only).
fn standing(ui: &mut egui::Ui, cx: &mut UiCx<'_>, pal: &Pal, ok: Color32, warn: Color32) {
    crate::ui::section(ui, cx.theme, "Account Standing");
    super::super::lists::demo(&mut cx.state.account);
    super::super::lists::fetch_once(cx, super::Page::MyAccount, AccountCommand::FetchStanding, |a| a.standing = None);
    match &cx.state.account.standing {
        None => {
            ui.add(egui::Spinner::new().size(16.0).color(pal.muted));
        }
        Some(Err(e)) => {
            ui.add(egui::Label::new(RichText::new(e).size(pal.meta()).color(pal.danger)).wrap().selectable(true));
        }
        Some(Ok(state)) => {
            let (text, color) = match *state {
                0..=100 => ("All good: your account is in good standing", ok),
                101..=200 => ("Limited: some features are restricted", warn),
                201..=300 => ("Very limited: many features are restricted", warn),
                301..=400 => ("At risk: further violations may suspend it", pal.danger),
                _ => ("Suspended", pal.danger),
            };
            chip(ui, pal, (*state <= 100).then_some("✔"), text, color);
        }
    }
}

/// Account Removal: Disable (neutral) and Delete (red); both only explain and open discord.com.
fn removal(ui: &mut egui::Ui, cx: &mut UiCx<'_>, pal: &Pal) -> Option<Dialog> {
    crate::ui::section(ui, cx.theme, "Account Removal");
    crate::ui::hint(ui, cx.theme, "Disabling your account means you can recover it at any time after taking this action.");
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        if ui.add(egui::Button::new("Disable Account").min_size(vec2(80.0, 32.0))).clicked() {
            return Some(Dialog::Removal { delete: false });
        }
        let delete = egui::Button::new(RichText::new("Delete Account").color(Color32::WHITE)).fill(pal.danger).min_size(vec2(80.0, 32.0));
        ui.add(delete).clicked().then_some(Dialog::Removal { delete: true })
    })
    .inner
}

// ---------------------------------------------------------------------------
// Dialogs
// ---------------------------------------------------------------------------

/// The open dialog, taken out of the form for the frame and put back unless it closed (then
/// dropped: its buffers wipe themselves).
fn dialogs(ui: &mut egui::Ui, cx: &mut UiCx<'_>, me: &User) {
    let mut d = std::mem::take(&mut cx.view.user_settings_ui.account.sec.dialog);
    if matches!(d, Dialog::None) {
        return;
    }
    // Esc closes the dialog, not the layer (the global confirm above it goes first).
    let esc = !cx.view.dialog_open() && cx.state.account.mfa.is_none() && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
    if !esc && draw(ui, cx, me, &mut d) {
        cx.view.user_settings_ui.account.sec.dialog = d;
    } else {
        drop(d);
        reset(cx);
    }
}

/// A secret text field without undo history (it would keep plain copies). `max` characters.
fn secret_field(ui: &mut egui::Ui, salt: &str, b: &mut Buf, password: bool, max: usize, hint: &str) -> egui::Response {
    let id = egui::Id::unique(salt);
    let r = ui.add(TextEdit::singleline(&mut **b).id(id).password(password).char_limit(max).hint_text(hint).desired_width(f32::INFINITY));
    crate::ui::login::forget_history(ui.ctx(), id);
    r
}

fn entered(r: &egui::Response) -> bool {
    r.lost_focus() && r.ctx.input(|i| i.key_pressed(egui::Key::Enter))
}

/// `begin` + send (nothing while the same write is still on its way).
fn send(cx: &mut UiCx<'_>, cmd: AccountCommand) {
    if cx.state.account.begin(cmd.op()) {
        cx.cmd.send(Command::Account(cmd));
    }
}

/// Draws `d`; false = close it. Moves to the next step by replacing `*d`.
fn draw(ui: &mut egui::Ui, cx: &mut UiCx<'_>, me: &User, d: &mut Dialog) -> bool {
    let ctx = ui.ctx().clone();
    let pal = Pal::new(cx.theme);
    let theme = cx.theme.clone();
    let (resp, go) = match d {
        Dialog::None => return false,
        Dialog::Password { current, new, confirm } => {
            let st = op_state(cx, OpKind::ChangePassword);
            if matches!(st, Some(OpState::Done(_))) {
                cx.view.toast("Your password was changed.", false);
                return false;
            }
            let pending = st == Some(OpState::Pending);
            let n = new.chars().count();
            let valid = !current.is_empty() && (8..=72).contains(&n) && **new == **confirm;
            let mut enter = false;
            let resp = dialog(&ctx, &theme, "rc_change_password", 440.0, "Change your password", |ui| {
                dialog_text(ui, &theme, "Enter your current password and a new one. Your other devices will be logged out.");
                ui.add_enabled_ui(!pending, |ui| {
                    field_label(ui, &pal, "Current Password");
                    enter |= entered(&secret_field(ui, "rc_pw_current", current, true, PASSWORD_MAX, ""));
                    field_label(ui, &pal, "New Password");
                    enter |= entered(&secret_field(ui, "rc_pw_new", new, true, 72, ""));
                    field_label(ui, &pal, "Confirm New Password");
                    enter |= entered(&secret_field(ui, "rc_pw_confirm", confirm, true, 72, ""));
                });
                let problem = if (1..8).contains(&n) {
                    Some("At least 8 characters.")
                } else if !confirm.is_empty() && **new != **confirm {
                    Some("The new passwords don't match.")
                } else {
                    None
                };
                if let Some(p) = problem {
                    ui.label(RichText::new(p).size(pal.meta()).color(pal.danger));
                }
                op_line(ui, &pal, st.as_ref(), "Changing…");
                dialog_buttons(ui, &theme, "Done", false, valid && !pending)
            });
            let go = (resp.inner == Some(true) || enter && valid && !pending).then(|| AccountCommand::ChangePassword { password: secret(current), new_password: secret(new) });
            (resp, go)
        }
        Dialog::TotpPassword { password } => {
            let st = op_state(cx, OpKind::CheckTotpPassword);
            if matches!(st, Some(OpState::Done(_))) {
                finish_op(cx, OpKind::CheckTotpPassword);
                let Some(key) = totp_secret() else {
                    cx.view.toast("Could not create a key: the system's random generator failed.", true);
                    return false;
                };
                let qr = QrCode::encode_text(otpauth_uri(&me.username, &key).expose(), QrCodeEcc::Medium).ok();
                *d = Dialog::TotpScan { password: secret(password), key, qr, code: buf() };
                return true;
            }
            let pending = st == Some(OpState::Pending);
            let mut enter = false;
            let resp = dialog(&ctx, &theme, "rc_totp_password", 440.0, "Enable Authenticator App", |ui| {
                dialog_text(ui, &theme, "Enter your password to continue.");
                ui.add_enabled_ui(!pending, |ui| enter = entered(&secret_field(ui, "rc_totp_pw", password, true, PASSWORD_MAX, "Password")));
                op_line(ui, &pal, st.as_ref(), "Checking…");
                dialog_buttons(ui, &theme, "Continue", false, !password.is_empty() && !pending)
            });
            let go = (resp.inner == Some(true) || enter && !password.is_empty()).then(|| AccountCommand::CheckTotpPassword { password: secret(password) });
            (resp, go)
        }
        Dialog::TotpScan { password, key, qr, code } => {
            let st = op_state(cx, OpKind::EnableTotp);
            if let Some(codes) = cx.state.account.backup_codes.take() {
                finish_op(cx, OpKind::EnableTotp);
                cx.view.toast("Authenticator app enabled.", false);
                *d = Dialog::Codes { codes, regen: None };
                return true;
            }
            let pending = st == Some(OpState::Pending);
            let ready = code.chars().filter(char::is_ascii_digit).count() == 6;
            let mut enter = false;
            let resp = dialog(&ctx, &theme, "rc_totp_scan", 520.0, "Enable Authenticator App", |ui| {
                ui.horizontal_top(|ui| {
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(164.0), Sense::hover());
                    if let Some(qr) = qr {
                        paint_qr(ui, rect, qr);
                    }
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 6.0;
                        dialog_text(ui, &theme, "Scan the QR code with an authenticator app, or enter this key:");
                        let shown: Zeroizing<String> = Zeroizing::new(key.expose().as_bytes().chunks(4).map(|c| std::str::from_utf8(c).unwrap_or_default()).collect::<Vec<_>>().join(" "));
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new(shown.as_str()).monospace().color(pal.text));
                            if ui.add(egui::Button::new("Copy").small()).on_hover_text("Copy the key").clicked() {
                                ui.ctx().copy_text(key.expose().to_owned());
                            }
                        });
                        ui.add_space(6.0);
                        field_label(ui, &pal, "Code from the app");
                        ui.add_enabled_ui(!pending, |ui| enter = entered(&secret_field(ui, "rc_totp_code", code, false, 7, "123456")));
                    });
                });
                op_line(ui, &pal, st.as_ref(), "Activating…");
                dialog_buttons(ui, &theme, "Activate", false, ready && !pending)
            });
            let go = (resp.inner == Some(true) || enter && ready && !pending)
                .then(|| AccountCommand::EnableTotp { password: password.clone(), secret: key.clone(), code: secret(code) });
            (resp, go)
        }
        Dialog::Sms { enable, password } => {
            let st = op_state(cx, OpKind::SetSmsMfa);
            if matches!(st, Some(OpState::Done(_))) {
                cx.view.toast(if *enable { "SMS authentication turned on." } else { "SMS authentication removed." }, false);
                return false;
            }
            let pending = st == Some(OpState::Pending);
            let (title, action) = if *enable { ("Enable SMS Authentication", "Enable") } else { ("Remove SMS Authentication", "Remove") };
            let mut enter = false;
            let resp = dialog(&ctx, &theme, "rc_sms", 440.0, title, |ui| {
                dialog_text(ui, &theme, "Enter your password to continue.");
                ui.add_enabled_ui(!pending, |ui| enter = entered(&secret_field(ui, "rc_sms_pw", password, true, PASSWORD_MAX, "Password")));
                op_line(ui, &pal, st.as_ref(), "Saving…");
                dialog_buttons(ui, &theme, action, !*enable, !password.is_empty() && !pending)
            });
            let go = (resp.inner == Some(true) || enter && !password.is_empty()).then(|| AccountCommand::SetSmsMfa { enable: *enable, password: secret(password) });
            (resp, go)
        }
        Dialog::RenameKey { id, name } => {
            let mut enter = false;
            let resp = dialog(&ctx, &theme, "rc_rename_key", 440.0, "Rename security key", |ui| {
                let r = ui.add(TextEdit::singleline(name).char_limit(32).desired_width(f32::INFINITY));
                enter = entered(&r);
                dialog_buttons(ui, &theme, "Save", false, !name.trim().is_empty())
            });
            if resp.inner == Some(true) || enter && !name.trim().is_empty() {
                cx.cmd.send(Command::Account(AccountCommand::RenameSecurityKey { id: *id, name: name.trim().to_owned() }));
                return false;
            }
            (resp, None)
        }
        Dialog::CodesPassword { password } => {
            let st = op_state(cx, OpKind::BackupCodesChallenge);
            if let Some(nonce) = cx.state.account.codes_nonce.take() {
                finish_op(cx, OpKind::BackupCodesChallenge);
                *d = Dialog::CodesKey { nonce, key: buf() };
                return true;
            }
            let pending = st == Some(OpState::Pending);
            let mut enter = false;
            let resp = dialog(&ctx, &theme, "rc_codes_pw", 440.0, "View Backup Codes", |ui| {
                dialog_text(ui, &theme, "Enter your password. Discord then emails you a key to see your codes.");
                ui.add_enabled_ui(!pending, |ui| enter = entered(&secret_field(ui, "rc_codes_pw", password, true, PASSWORD_MAX, "Password")));
                op_line(ui, &pal, st.as_ref(), "Sending…");
                dialog_buttons(ui, &theme, "Send Key", false, !password.is_empty() && !pending)
            });
            let go = (resp.inner == Some(true) || enter && !password.is_empty()).then(|| AccountCommand::BackupCodesChallenge { password: secret(password) });
            (resp, go)
        }
        Dialog::CodesKey { nonce, key } => {
            let st = op_state(cx, OpKind::ViewBackupCodes);
            if let Some(codes) = cx.state.account.backup_codes.take() {
                finish_op(cx, OpKind::ViewBackupCodes);
                *d = Dialog::Codes { codes, regen: Some((secret(key), nonce.regenerate_nonce.clone())) };
                return true;
            }
            let pending = st == Some(OpState::Pending);
            let ready = key.trim().len() >= 6;
            let mut enter = false;
            let resp = dialog(&ctx, &theme, "rc_codes_key", 440.0, "View Backup Codes", |ui| {
                dialog_text(ui, &theme, "Enter the verification key Discord emailed you.");
                ui.add_enabled_ui(!pending, |ui| enter = entered(&secret_field(ui, "rc_codes_key", key, false, 16, "Key")));
                op_line(ui, &pal, st.as_ref(), "Checking…");
                dialog_buttons(ui, &theme, "Verify", false, ready && !pending)
            });
            let go = (resp.inner == Some(true) || enter && ready).then(|| AccountCommand::ViewBackupCodes { key: secret(key), nonce: nonce.nonce.clone(), regenerate: false });
            (resp, go)
        }
        Dialog::Codes { codes, regen } => {
            // New codes (Generate New Codes went through the confirm): the nonce is used up.
            if let Some(new) = cx.state.account.backup_codes.take() {
                finish_op(cx, OpKind::ViewBackupCodes);
                (*codes, *regen) = (new, None);
            }
            let mut ask_regen = false;
            let resp = dialog(&ctx, &theme, "rc_backup_codes", 480.0, "Backup Codes", |ui| {
                dialog_text(ui, &theme, "Each code works once when you can't use your authenticator app. Keep them somewhere safe.");
                codes_grid(ui, &pal, codes);
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new("Copy").min_size(vec2(64.0, 32.0))).clicked() {
                        ui.ctx().copy_text(codes_text(codes).to_string());
                    }
                    if ui.add(egui::Button::new("Save…").min_size(vec2(64.0, 32.0))).on_hover_text("Save as a text file").clicked() {
                        save_codes(cx, codes);
                    }
                    if regen.is_some() {
                        ask_regen = ui.add(egui::Button::new(RichText::new("Generate New Codes").color(pal.danger)).frame(false)).clicked();
                    }
                });
                ui.add_space(8.0);
                // Shown once: nothing to cancel, only Done.
                dialog_footer(ui, |ui| ui.add(primary(&theme, "Done").min_size(vec2(96.0, 36.0))).clicked().then_some(true))
            });
            if ask_regen && let Some((key, nonce)) = regen {
                let cmd = AccountCommand::ViewBackupCodes { key: key.clone(), nonce: nonce.clone(), regenerate: true };
                cx.view.confirm("Generate new backup codes?", "Your current backup codes stop working.", "Generate", Command::Account(cmd));
            }
            if resp.inner.is_some() {
                return false;
            }
            (resp, None)
        }
        Dialog::Removal { delete } => {
            let title = if *delete { "Delete Account" } else { "Disable Account" };
            let resp = dialog(&ctx, &theme, "rc_removal", 440.0, title, |ui| {
                let what = if *delete { "Deleting your account can't be undone." } else { "A disabled account can be recovered by logging in again." };
                dialog_text(ui, &theme, what);
                dialog_text(ui, &theme, "This is done on discord.com – we'll open it in your browser.");
                dialog_buttons(ui, &theme, "Open discord.com ↗", *delete, true)
            });
            if resp.inner == Some(true) {
                open_account_page(ui);
            }
            if resp.inner.is_some() {
                return false;
            }
            (resp, None)
        }
    };
    if let Some(cmd) = go {
        send(cx, cmd);
    }
    !(resp.inner == Some(false) || resp.should_close())
}

/// The codes in two monospace columns as `xxxx-xxxx`; used ones dimmed and struck through.
fn codes_grid(ui: &mut egui::Ui, pal: &Pal, codes: &[BackupCode]) {
    Frame::new().fill(pal.bg).corner_radius(pal.card_radius()).inner_margin(12).show(ui, |ui| {
        ui.set_width(ui.available_width());
        egui::Grid::new(egui::Id::unique("rc_codes_grid")).num_columns(2).spacing(vec2(32.0, 6.0)).show(ui, |ui| {
            for (i, c) in codes.iter().enumerate() {
                let text = RichText::new(dashed(&c.code).as_str()).monospace().size(pal.size + 1.0);
                ui.label(if c.consumed { text.strikethrough().color(pal.muted) } else { text.color(pal.text) });
                if i % 2 == 1 {
                    ui.end_row();
                }
            }
        });
    });
}

/// `abcd1234` -> `abcd-1234` (8-character codes; others unchanged).
fn dashed(code: &Secret) -> Zeroizing<String> {
    let c = code.expose();
    Zeroizing::new(if c.len() == 8 && c.is_ascii() { format!("{}-{}", &c[..4], &c[4..]) } else { c.to_owned() })
}

/// One unused code per line (Copy / Save…).
fn codes_text(codes: &[BackupCode]) -> Zeroizing<String> {
    let mut out = Zeroizing::new(String::with_capacity(codes.len() * 11));
    for c in codes.iter().filter(|c| !c.consumed) {
        out.push_str(&dashed(&c.code));
        out.push('\n');
    }
    out
}

/// Save… : a file the user picks (on this click only).
fn save_codes(cx: &mut UiCx<'_>, codes: &[BackupCode]) {
    let Some(path) = rfd::FileDialog::new().set_file_name("discord_backup_codes.txt").add_filter("Text", &["txt"]).save_file() else { return };
    if let Err(e) = std::fs::write(&path, codes_text(codes).as_bytes()) {
        cx.view.toast(format!("Could not save the codes: {e}"), true);
    }
}

/// Dark modules on a white rounded square (its margin is the quiet zone), snapped to whole
/// pixels so the code stays crisp; one rect per run of dark modules.
fn paint_qr(ui: &egui::Ui, rect: Rect, code: &QrCode) {
    let ppp = ui.ctx().pixels_per_point();
    let n = code.size();
    let module = ((rect.width() * ppp) / (n + 8) as f32).floor().max(1.0) / ppp;
    let painter = ui.painter();
    painter.rect_filled(rect.round_to_pixels(ppp), 8.0, Color32::WHITE);
    let origin = (rect.center() - Vec2::splat(module * n as f32 / 2.0)).round_to_pixels(ppp);
    for y in 0..n {
        let mut x = 0;
        while x < n {
            let start = x;
            while x < n && code.get_module(x, y) {
                x += 1;
            }
            if x > start {
                let min = origin + vec2(start as f32, y as f32) * module;
                painter.rect_filled(Rect::from_min_size(min, vec2((x - start) as f32, 1.0) * module), 0.0, Color32::BLACK);
            }
            x += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// 2FA re-check prompt
// ---------------------------------------------------------------------------

fn method_label(k: MfaKind) -> &'static str {
    match k {
        MfaKind::Totp => "Authenticator",
        MfaKind::Backup => "Backup Code",
        MfaKind::Sms => "SMS",
        MfaKind::Password => "Password",
    }
}

/// The prompt of an action waiting for its 2FA re-check (`AccountState::mfa`): a method per
/// tab, the code (or password), Confirm -> `FinishMfa` (the backend then retries the action
/// once), Cancel / Esc -> `CancelMfa`. Drawn on every settings page (`user_settings::show`).
pub(in crate::ui::user_settings) fn mfa_dialog(ui: &mut egui::Ui, cx: &mut UiCx<'_>) {
    demo(cx);
    let form = &mut cx.view.user_settings_ui.account.sec;
    let Some(prompt) = cx.state.account.mfa.clone().filter(|p| !p.methods.is_empty()) else {
        // Passed, expired or cancelled: drop what was typed.
        if form.mfa_kind.take().is_some() {
            form.mfa_code = Buf::default();
        }
        return;
    };
    if form.mfa_kind.is_none_or(|k| !prompt.methods.contains(&k)) {
        (form.mfa_kind, form.mfa_code) = (Some(prompt.methods[0]), buf());
    }
    let kind = form.mfa_kind.unwrap_or(MfaKind::Totp);
    let (ctx, theme, pal) = (ui.ctx().clone(), cx.theme.clone(), Pal::new(cx.theme));
    let esc = !cx.view.dialog_open() && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
    let (mut pick, mut sms, mut enter) = (None, false, false);
    let code = &mut cx.view.user_settings_ui.account.sec.mfa_code;
    let ready = match kind {
        MfaKind::Password => !code.is_empty(),
        MfaKind::Backup => code.chars().filter(char::is_ascii_alphanumeric).count() == 8,
        MfaKind::Totp | MfaKind::Sms => code.chars().filter(char::is_ascii_digit).count() == 6,
    };
    let need_sms = kind == MfaKind::Sms && prompt.sms_phone.is_none();
    // No × while busy: in flight it can't be cancelled (see below).
    let resp = dialog_closable(&ctx, &theme, "rc_mfa_prompt", 440.0, "Confirm it's you", !prompt.busy, |ui| {
        let text = match kind {
            MfaKind::Totp => "Enter the 6-digit code from your authenticator app.".to_owned(),
            MfaKind::Backup => "Enter one of your 8-character backup codes.".to_owned(),
            MfaKind::Sms => prompt.sms_phone.as_ref().map_or_else(|| "Discord texts a code to your phone.".to_owned(), |p| format!("Enter the code sent to {p}.")),
            MfaKind::Password => "Enter your password to continue.".to_owned(),
        };
        dialog_text(ui, &theme, &text);
        if prompt.methods.len() > 1 {
            // The login 2FA screen's switcher (same choice, same control).
            let options: Vec<_> = prompt.methods.iter().map(|&m| (m, method_label(m))).collect();
            let mut picked = kind;
            crate::ui::login::segmented(ui, &theme, &mut picked, &options);
            pick = (picked != kind).then_some(picked);
        }
        ui.add_enabled_ui(!prompt.busy, |ui| {
            if need_sms {
                sms = ui.add(primary(&theme, "Send Code")).clicked();
            } else {
                let (password, max, hint) = match kind {
                    MfaKind::Password => (true, PASSWORD_MAX, "Password"),
                    MfaKind::Backup => (false, 9, "abcd-1234"),
                    _ => (false, 7, "123456"),
                };
                let r = secret_field(ui, "rc_mfa_code", code, password, max, hint);
                if !prompt.busy && r.ctx.memory(|m| m.focused().is_none()) {
                    r.request_focus();
                }
                enter = entered(&r);
            }
        });
        if prompt.busy {
            ui.add(egui::Spinner::new().size(14.0).color(pal.muted));
        }
        if let Some(e) = &prompt.error {
            ui.add(egui::Label::new(RichText::new(e).size(pal.meta()).color(pal.danger)).wrap());
        }
        dialog_buttons(ui, &theme, "Confirm", false, ready && !need_sms && !prompt.busy)
    });
    let form = &mut cx.view.user_settings_ui.account.sec;
    if let Some(m) = pick {
        (form.mfa_kind, form.mfa_code) = (Some(m), buf());
    }
    let finish = resp.inner == Some(true) || enter && ready && !need_sms && !prompt.busy;
    if finish {
        let code = Secret::new(std::mem::replace(&mut *form.mfa_code, String::with_capacity(4 * PASSWORD_MAX)));
        cx.cmd.send(Command::Account(AccountCommand::FinishMfa { kind, code }));
        if let Some(p) = cx.state.account.mfa.as_mut() {
            (p.busy, p.error) = (true, None);
        }
    } else if sms {
        cx.cmd.send(Command::Account(AccountCommand::MfaSms));
        if let Some(p) = cx.state.account.mfa.as_mut() {
            p.busy = true;
        }
    } else if !prompt.busy && (esc || resp.inner == Some(false) || resp.should_close()) {
        // In flight the backend owns the action; its Mfa(None|Some) answer ends `busy`.
        cancel_mfa(cx, &prompt);
    }
}

/// Cancel: the backend drops the action (and its ticket); its progress entry goes too.
fn cancel_mfa(cx: &mut UiCx<'_>, prompt: &MfaPrompt) {
    cx.cmd.send(Command::Account(AccountCommand::CancelMfa));
    cx.state.account.mfa = None;
    if cx.state.account.ops.get(&prompt.op) == Some(&OpState::Pending) {
        cx.state.account.ops.remove(&prompt.op);
    }
    let form = &mut cx.view.user_settings_ui.account.sec;
    (form.mfa_kind, form.mfa_code) = (None, Buf::default());
}

/// `RUSTCORD_NAV_DEMO=account:<dialog>`: that dialog with sample data, sending nothing:
/// password, totp, scan, sms, codes-key, codes, mfa, removal (and `settings` sample keys).
fn demo(cx: &mut UiCx<'_>) {
    use crate::api::account::SecurityKey;
    if crate::ui::sidebar::nav_demo("settings") && !cx.state.account.security_keys.as_ref().is_some_and(Result::is_ok) {
        let key = |id, name: &str| SecurityKey { id: Id(id), name: name.into() };
        cx.state.account.security_keys = Some(Ok(vec![key(1, "YubiKey 5C"), key(2, "Phone passkey")]));
    }
    let Some(which) = crate::ui::sidebar::nav_demo_value("account") else { return };
    let form = &mut cx.view.user_settings_ui.account.sec;
    if std::mem::replace(&mut form.demo, true) {
        return;
    }
    let sample = |s: &str| Secret::new(s.to_owned());
    let codes = || ["k3v9q2xa", "p0m4z8rt", "b7c2d1fe", "q9w8e7r6", "z1x2c3v4", "h5j6k7l8", "n9m8b7v6", "a1s2d3f4"].map(|c| BackupCode { code: sample(c), consumed: c.starts_with('q') }).to_vec();
    form.dialog = match which {
        "password" => Dialog::Password { current: buf(), new: buf(), confirm: buf() },
        "totp" => Dialog::TotpPassword { password: buf() },
        "scan" => {
            let key = totp_secret().unwrap_or_else(|| sample("JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP"));
            let qr = QrCode::encode_text(otpauth_uri("ferris", &key).expose(), QrCodeEcc::Medium).ok();
            Dialog::TotpScan { password: sample(""), key, qr, code: buf() }
        }
        "sms" => Dialog::Sms { enable: true, password: buf() },
        "codes-key" => Dialog::CodesKey { nonce: CodesNonce { nonce: sample(""), regenerate_nonce: sample("") }, key: buf() },
        "codes" => Dialog::Codes { codes: codes(), regen: Some((sample(""), sample(""))) },
        "removal" => Dialog::Removal { delete: true },
        "mfa" => {
            let op = AccountCommand::LogoutSessions { id_hashes: Vec::new() }.op();
            let methods = vec![MfaKind::Totp, MfaKind::Backup, MfaKind::Sms];
            cx.state.account.mfa = Some(MfaPrompt { op, methods, sms_phone: None, error: Some("Invalid two-factor code".into()), busy: false });
            Dialog::None
        }
        _ => Dialog::None,
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_dashed_and_used_ones_left_out() {
        let code = |c: &str, consumed| BackupCode { code: Secret::new(c.into()), consumed };
        assert_eq!(dashed(&Secret::new("abcd1234".into())).as_str(), "abcd-1234");
        assert_eq!(dashed(&Secret::new("short".into())).as_str(), "short");
        let text = codes_text(&[code("abcd1234", false), code("used0000", true), code("wxyz9876", false)]);
        assert_eq!(text.as_str(), "abcd-1234\nwxyz-9876\n");
    }

    #[test]
    fn typed_secrets_are_copied_into_secrets() {
        let mut b = buf();
        b.push_str("hunter22");
        let cap = b.capacity();
        let s = secret(&b);
        assert_eq!((s.expose(), b.as_str()), ("hunter22", "hunter22"), "the form keeps its text for another try");
        assert_eq!(b.capacity(), cap);
        assert_eq!(format!("{s:?}"), "Secret(<redacted>)");
    }
}
