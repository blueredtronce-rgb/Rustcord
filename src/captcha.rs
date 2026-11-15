//! Discord's hCaptcha inside the login card, under the field of the request it answers; the user
//! solves it (docs/AUTH_NOTES.md section 0). Windows and macOS only; elsewhere `supported()` is false
//! and the login screen keeps its plain text.
//!
//! The widget is a wry webview made a child of Rustcord's own window (`build_as_child`) and laid over
//! the slot the card leaves for it. A native child always sits above the GL surface and egui cannot
//! clip it, so it hides whenever its slot is not wholly in view. While hCaptcha's challenge is open it
//! covers the page below the title bar, and shrinks back to the slot after.
//!
//! The page is local HTML that embeds hCaptcha's standard `api.js` with `host=discord.com`, served
//! at the origin `https://discord.com/` inside the webview (what hCaptcha's own mobile SDKs do for
//! their app's host). The webview never contacts Discord; it talks to hCaptcha only, and it gets the
//! sitekey and rqdata, never the password, a ticket, a cookie or the fingerprint.

use crate::auth::Secret;

/// What the captcha ended with (`Command::Captcha`).
#[derive(Debug)]
pub enum CaptchaReply {
    /// The widget's token.
    #[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))] // only the webview makes one
    Solved(Secret),
    /// Cancel in the card, or nobody answered for 10 minutes.
    Cancelled,
    /// No webview could be made (no WebView2 runtime).
    #[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
    Unavailable,
}

/// True where the card can show the widget.
pub const fn supported() -> bool {
    cfg!(any(windows, target_os = "macos"))
}

/// The webview profiles, one folder per captcha.
fn profiles() -> std::path::PathBuf {
    crate::config::paths().cache_dir.join("captcha-webview")
}

/// At start, before any captcha: removes what a quit or crash with the captcha open left behind.
pub fn clean_profiles() {
    let _ = std::fs::remove_dir_all(profiles());
}

/// Stub: no webview here, so the backend never asks the card for a captcha.
#[cfg(not(any(windows, target_os = "macos")))]
#[derive(Default)]
pub struct Inline;

#[cfg(not(any(windows, target_os = "macos")))]
impl Inline {
    pub fn sync(&mut self, _: &egui::Context, _: &eframe::Frame, _: &mut crate::ui::ViewState, _: &crate::theme::Theme, _: &crate::backend::CommandSender) {}
}

#[cfg(any(windows, target_os = "macos"))]
pub use imp::Inline;

#[cfg(any(windows, target_os = "macos"))]
mod imp {
    use super::{CaptchaReply, Secret, profiles};
    use crate::backend::{Command, CommandSender};
    use crate::theme::Theme;
    use crate::ui::ViewState;
    use serde_json::{Value, json};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::Duration;
    use wry::dpi::{PhysicalPosition, PhysicalSize};

    #[cfg(target_os = "macos")]
    const ORIGIN: &str = "https://discord.com/";

    /// The widget sits at the top left (`place` moves it while a challenge is open); its token and the
    /// challenge's opening and closing go back through `window.ipc`. hCaptcha shows its own errors.
    const PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><meta name="color-scheme" content="dark light"><title>Captcha</title>
<style>
html,body{margin:0;height:100%;overflow:hidden;background:var(--card);color:var(--muted);font:14px "Segoe UI",system-ui,-apple-system,sans-serif}
body.open{overflow:auto;background:var(--backdrop)}
#w,#t{position:absolute;left:0;top:0;margin:0}
</style></head>
<body><div id="w"></div><p id="t" hidden>Couldn't load the captcha. Check your connection, then select Cancel and try again.</p>
<script>
const P=/*PARAMS*/null;
document.documentElement.style.cssText='--card:'+P.card+';--backdrop:'+P.backdrop+';--muted:'+P.muted;
const post=m=>window.ipc.postMessage(JSON.stringify(m));
const shut=()=>post({open:false});
function place(x,y,open){const w=document.getElementById('w');w.style.left=x+'px';w.style.top=y+'px';document.body.classList.toggle('open',open);dispatchEvent(new Event('resize'));}
function ready(){
  const id=hcaptcha.render('w',{sitekey:P.sitekey,theme:P.theme,callback:t=>post({token:t}),
    'open-callback':()=>post({open:true}),'close-callback':shut,'chalexpired-callback':shut,'error-callback':shut});
  if(P.rqdata)hcaptcha.setData(id,{rqdata:P.rqdata});
}
const s=document.createElement('script');
s.src='https://js.hcaptcha.com/1/api.js?render=explicit&onload=ready&recaptchacompat=off&host=discord.com';
s.async=true;
s.onerror=()=>{document.getElementById('t').hidden=false};
document.head.appendChild(s);
</script></body></html>"#;

    /// Hands the page its parameters. `<` is escaped so rqdata cannot close the script tag.
    fn page_html(params: &Value) -> String {
        PAGE.replace("/*PARAMS*/null", &params.to_string().replace('<', "\\u003c"))
    }

    /// `{"token": "..."}` with a printable-ASCII token of 1 to 8192 bytes.
    fn token_from_json(s: &str) -> Option<String> {
        let v: Value = serde_json::from_str(s).ok()?;
        let t = v.get("token")?.as_str()?;
        ((1..=8192).contains(&t.len()) && t.bytes().all(|b| (0x21..=0x7E).contains(&b))).then(|| t.to_owned())
    }

    /// The page itself and hCaptcha's own hosts; the webview goes nowhere else.
    fn allowed_nav(u: &str) -> bool {
        if u == "https://discord.com/" || u.starts_with("about:") {
            return true;
        }
        let Some(rest) = u.strip_prefix("https://") else { return false };
        let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
        !host.contains('@') && (host == "hcaptcha.com" || host.ends_with(".hcaptcha.com"))
    }

    /// Windows: wry serves the custom protocol `discord` at `https://discord.*`, so `discord://com/`
    /// is the page at `https://discord.com/`; every other request of that webview is a 404.
    #[cfg(windows)]
    fn serve(page: &str, uri: &wry::http::Uri) -> wry::http::Response<std::borrow::Cow<'static, [u8]>> {
        let ok = *uri == "discord://com/";
        let body: std::borrow::Cow<'static, [u8]> = if ok { page.as_bytes().to_vec().into() } else { Default::default() };
        wry::http::Response::builder()
            .status(if ok { 200 } else { 404 })
            .header("Content-Type", "text/html; charset=utf-8")
            .body(body)
            .expect("static response parts")
    }

    /// `r` (egui points) in the window's physical pixels, both edges rounded: x, y, width, height.
    fn physical(r: egui::Rect, ppp: f32) -> [i32; 4] {
        let (min, max) = ((r.min.to_vec2() * ppp).round(), (r.max.to_vec2() * ppp).round());
        [min.x as i32, min.y as i32, (max.x - min.x) as i32, (max.y - min.y) as i32]
    }

    fn bounds([x, y, w, h]: [i32; 4]) -> wry::Rect {
        wry::Rect { position: PhysicalPosition::new(x, y).into(), size: PhysicalSize::new(w.max(1) as u32, h.max(1) as u32).into() }
    }

    /// The webview over the card. Fields drop in order: the webview before its context.
    struct Shown {
        webview: wry::WebView,
        _context: wry::WebContext,
        profile: PathBuf,
        /// What the page posted (`window.ipc`), read once per pass.
        msgs: Arc<Mutex<Vec<String>>>,
        /// hCaptcha's challenge is open: the webview covers the page below the title bar.
        open: bool,
        /// Applied to the webview (only changes go out): physical bounds (None: hidden), the
        /// widget's offset and open flag, the zoom.
        bounds: Option<[i32; 4]>,
        place: (i32, i32, bool),
        zoom: f32,
    }

    impl Shown {
        fn new(ctx: &egui::Context, frame: &eframe::Frame, theme: &Theme, sitekey: &str, rqdata: Option<&str>, slot: egui::Rect) -> wry::Result<Self> {
            static N: AtomicU32 = AtomicU32::new(0);
            // Its own folder: the browser process of an earlier one may still hold that one.
            let profile = profiles().join(N.fetch_add(1, Ordering::Relaxed).to_string());
            let mut context = wry::WebContext::new(Some(profile.clone()));
            let tk = theme.tokens();
            let hex = |c: egui::Color32| format!("#{:02x}{:02x}{:02x}", c.r(), c.g(), c.b());
            let card = tk.raised;
            let page = page_html(&json!({
                "sitekey": sitekey,
                "rqdata": rqdata,
                "theme": if theme.dark { "dark" } else { "light" },
                "card": hex(card),
                // The page under the open challenge: the login page with the modal scrim over it.
                "backdrop": hex(theme.colors.guild_bar.to_color32().blend(tk.scrim)),
                "muted": hex(theme.colors.text_muted.to_color32()),
            }));
            let msgs: Arc<Mutex<Vec<String>>> = Default::default();
            let (queue, repaint) = (msgs.clone(), ctx.clone());
            let at = physical(slot, ctx.pixels_per_point());
            let builder = wry::WebViewBuilder::new_with_web_context(&mut context)
                .with_incognito(true)
                .with_devtools(false)
                .with_background_color((card.r(), card.g(), card.b(), 255))
                .with_bounds(bounds(at))
                .with_navigation_handler(|u| allowed_nav(&u))
                .with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
                .with_ipc_handler(move |r| {
                    queue.lock().unwrap_or_else(PoisonError::into_inner).push(r.into_body());
                    repaint.request_repaint();
                });
            #[cfg(windows)]
            let builder = {
                use wry::WebViewBuilderExtWindows as _;
                builder.with_custom_protocol("discord".into(), move |_, req| serve(&page, req.uri())).with_https_scheme(true).with_url("discord://com/")
            };
            let webview = builder.build_as_child(frame)?;
            #[cfg(target_os = "macos")]
            {
                // wry loads HTML without a base URL; the widget needs the discord.com origin.
                use objc2_foundation::{NSString, NSURL};
                use wry::WebViewExtMacOS as _;
                let base = NSURL::URLWithString(&NSString::from_str(ORIGIN));
                // SAFETY: plain WKWebView call on the main thread with live objects.
                unsafe { webview.webview().loadHTMLString_baseURL(&NSString::from_str(&page), base.as_deref()) };
            }
            // CSS pixels = egui points, also under Rustcord's own zoom.
            let zoom = ctx.zoom_factor();
            let _ = webview.zoom(f64::from(zoom));
            // Test hook RUSTCORD_CAPTCHA_DEMO=open: the open layout without a challenge (the test sitekey never asks one).
            let open = std::env::var("RUSTCORD_CAPTCHA_DEMO").is_ok_and(|v| v == "open");
            Ok(Self { webview, _context: context, profile, msgs, open, bounds: Some(at), place: (0, 0, false), zoom })
        }
    }

    impl Drop for Shown {
        /// Keyboard focus back to egui; the folder goes once the browser process lets go of it (it
        /// exits about 1 s after its last webview, measured).
        fn drop(&mut self) {
            let _ = self.webview.focus_parent();
            let dir = self.profile.clone();
            std::thread::spawn(move || {
                for _ in 0..20 {
                    std::thread::sleep(Duration::from_millis(250));
                    let _ = std::fs::remove_dir_all(&dir);
                }
            });
        }
    }

    /// Discord's captcha inside the login card (`ViewState::login_captcha`).
    #[derive(Default)]
    pub struct Inline(Option<Shown>);

    impl Inline {
        /// Once per pass, after the screen drew: makes, moves, hides or closes the webview to match the
        /// card, and hands the widget's token (or a missing webview) to the backend.
        pub fn sync(&mut self, ctx: &egui::Context, frame: &eframe::Frame, view: &mut ViewState, theme: &Theme, cmd: &CommandSender) {
            let slot = view.login_captcha_slot.take();
            let Some((sitekey, rqdata)) = &view.login_captcha else {
                self.0 = None;
                return;
            };
            if self.0.is_none() {
                // Made where the card drew its slot, and only when all of it is in view.
                let Some((at, true)) = slot else { return };
                match Shown::new(ctx, frame, theme, sitekey, rqdata.as_deref(), at) {
                    Ok(s) => self.0 = Some(s),
                    Err(e) => {
                        eprintln!("rustcord: no captcha webview ({e})");
                        view.login_captcha = None;
                        return cmd.send(Command::Captcha(CaptchaReply::Unavailable));
                    }
                }
            }
            let Some(s) = self.0.as_mut() else { return };
            let msgs = std::mem::take(&mut *s.msgs.lock().unwrap_or_else(PoisonError::into_inner));
            for m in msgs {
                if let Some(token) = token_from_json(&m) {
                    view.login_captcha = None;
                    self.0 = None;
                    return cmd.send(Command::Captcha(CaptchaReply::Solved(Secret::new(token))));
                }
                if let Some(open) = serde_json::from_str::<Value>(&m).ok().and_then(|v| v.get("open")?.as_bool()) {
                    s.open = open;
                }
            }
            if ctx.zoom_factor() != s.zoom {
                s.zoom = ctx.zoom_factor();
                let _ = s.webview.zoom(f64::from(s.zoom));
            }
            // Not drawn this pass (another screen), or the small widget partly scrolled out: hidden, still loaded.
            let Some((slot, _)) = slot.filter(|&(_, whole)| whole || s.open) else {
                if s.bounds.take().is_some() {
                    let _ = s.webview.set_visible(false);
                }
                return;
            };
            let page = ctx.content_rect();
            let area = if s.open { egui::Rect::from_min_max(egui::pos2(page.min.x, page.min.y + crate::ui::TITLE_BAR_H), page.max) } else { slot };
            // The widget stays over its slot while the webview covers the page.
            let offset = slot.min - area.min;
            let place = (offset.x.round() as i32, offset.y.round() as i32, s.open);
            if place != s.place {
                s.place = place;
                let _ = s.webview.evaluate_script(&format!("place({},{},{})", place.0, place.1, place.2));
            }
            let at = physical(area, ctx.pixels_per_point());
            if s.bounds != Some(at) {
                if s.bounds.is_none() {
                    let _ = s.webview.set_visible(true);
                }
                s.bounds = Some(at);
                let _ = s.webview.set_bounds(bounds(at));
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_page_cannot_be_broken_out_of() {
            let input = json!({ "sitekey": "10000000-ffff-ffff-ffff-000000000001", "rqdata": "</script><script>x" });
            let html = page_html(&input);
            assert!(!html.contains("</script><script>"));
            assert!(html.contains("10000000-ffff-ffff-ffff-000000000001") && html.contains("host=discord.com"));
            assert!(!html.contains("/*PARAMS*/"));
            let start = html.find("const P=").unwrap() + "const P=".len();
            let end = html[start..].find(";\ndocument.").unwrap();
            let params: Value = serde_json::from_str(&html[start..start + end]).unwrap();
            assert_eq!(params, input);
        }

        #[test]
        fn reads_the_token_line() {
            assert_eq!(token_from_json(r#"{"token":"P1_eyJ0"}"#).as_deref(), Some("P1_eyJ0"));
            assert!(token_from_json(&format!(r#"{{"token":"{}"}}"#, "a".repeat(8193))).is_none());
            assert!(token_from_json(&format!(r#"{{"token":"{}"}}"#, "a".repeat(8192))).is_some());
            assert!(token_from_json(r#"{"token":"two words"}"#).is_none());
            assert!(token_from_json(r#"{"token":"tokén"}"#).is_none());
            assert!(token_from_json(r#"{"token":""}"#).is_none());
            assert!(token_from_json(r#"{"open":true}"#).is_none() && token_from_json("not json").is_none());
        }

        #[test]
        fn only_the_page_and_hcaptcha_are_reachable() {
            for ok in ["https://discord.com/", "about:blank", "https://newassets.hcaptcha.com/captcha/v1/x/static/hcaptcha.html#frame=checkbox", "https://hcaptcha.com/"] {
                assert!(allowed_nav(ok), "{ok}");
            }
            for bad in ["https://discord.com/login", "http://hcaptcha.com/", "https://hcaptcha.com.evil.test/", "https://x@evil.test/", "https://evil.test@x.hcaptcha.com/", "javascript:alert(1)", "file:///c:/x"] {
                assert!(!allowed_nav(bad), "{bad}");
            }
        }

        #[test]
        fn bounds_cover_the_slot_in_physical_pixels() {
            let slot = egui::Rect::from_min_size(egui::pos2(60.0, 140.0), egui::vec2(376.0, 78.0));
            assert_eq!(physical(slot, 1.0), [60, 140, 376, 78]);
            assert_eq!(physical(slot, 1.25), [75, 175, 470, 98]);
            // Both edges round, so neighbouring rects never leave a gap.
            assert_eq!(physical(egui::Rect::from_min_max(egui::pos2(0.3, 0.0), egui::pos2(10.3, 1.0)), 1.5), [0, 0, 15, 2]);
        }

        #[cfg(windows)]
        #[test]
        fn only_the_page_is_served() {
            let at = |u: &str| serve("<p>", &u.parse().unwrap());
            let ok = at("discord://com/");
            assert_eq!((ok.status().as_u16(), ok.body().as_ref()), (200, b"<p>".as_slice()));
            assert_eq!(at("discord://com/favicon.ico").status().as_u16(), 404);
            assert_eq!(at("discord://gg/x").status().as_u16(), 404);
        }
    }
}
