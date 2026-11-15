//! The captcha window: Discord's hCaptcha shown to the user, who solves it (docs/AUTH_NOTES.md
//! section 0). Windows and macOS only; elsewhere `supported()` is false and the login screen keeps its
//! plain text.
//!
//! The window is a helper process, `rustcord --captcha`: its own winit window with a wry webview, so
//! a webview crash cannot take Rustcord down and its browser processes and profile go away with it.
//! The parent sends the sitekey (and rqdata) on its stdin and reads the widget's token from its stdout;
//! the helper never sees the password, a ticket, a cookie or the fingerprint.
//!
//! The page is local HTML that embeds hCaptcha's standard `api.js` with `host=discord.com`, served
//! at the origin `https://discord.com/` inside the webview (what hCaptcha's own mobile SDKs do for
//! their app's host). The webview never contacts Discord; it talks to hCaptcha only.

use crate::api::auth::CaptchaChallenge;
use crate::auth::Secret;

/// What the captcha window ended with.
#[derive(Debug)]
pub enum CaptchaReply {
    /// The widget's token.
    #[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))] // only the window makes one
    Solved(Secret),
    /// The user closed the window, or it timed out.
    Cancelled,
    /// No window could be opened (no WebView2, or an unsupported platform).
    Unavailable,
}

/// True where the window exists.
pub const fn supported() -> bool {
    cfg!(any(windows, target_os = "macos"))
}

#[cfg(not(any(windows, target_os = "macos")))]
pub async fn solve(_: &CaptchaChallenge) -> CaptchaReply {
    CaptchaReply::Unavailable
}

#[cfg(any(windows, target_os = "macos"))]
pub use imp::{helper_main, solve};

#[cfg(any(windows, target_os = "macos"))]
mod imp {
    use super::{CaptchaChallenge, CaptchaReply, Secret};
    use serde_json::{Value, json};
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    /// The helper's exit code when no webview could be created.
    const EXIT_NO_WEBVIEW: i32 = 3;
    /// A captcha nobody answers is given up after this.
    const TIMEOUT: Duration = Duration::from_secs(600);
    const TITLE: &str = "Confirm you are human";
    const ORIGIN: &str = "https://discord.com/";

    /// The widget: standard hCaptcha `api.js`, `host=discord.com` (the widget cannot see a hostname
    /// inside a webview), and the token goes back through `window.ipc`. hCaptcha shows its own errors.
    const PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><meta name="color-scheme" content="dark light"><title>Confirm you are human</title>
<style>
html,body{margin:0;background:#fff;color:#1e1f22;font:14px "Segoe UI",system-ui,-apple-system,sans-serif}
@media(prefers-color-scheme:dark){html,body{background:#2b2d31;color:#dbdee1}}
body{display:flex;flex-direction:column;align-items:center;padding:28px 16px}
#t{text-align:center;margin:0 0 20px;max-width:330px}
</style></head>
<body><p id="t">Discord wants to make sure you're a person. Select the box below.</p><div id="w"></div>
<script>
const P=/*PARAMS*/null;
function ready(){
  const id=hcaptcha.render('w',{sitekey:P.sitekey,theme:matchMedia('(prefers-color-scheme: dark)').matches?'dark':'light',
    callback:t=>window.ipc.postMessage(JSON.stringify({token:t}))});
  if(P.rqdata)hcaptcha.setData(id,{rqdata:P.rqdata});
}
const s=document.createElement('script');
s.src='https://js.hcaptcha.com/1/api.js?render=explicit&onload=ready&recaptchacompat=off&host=discord.com';
s.async=true;
s.onerror=()=>{document.getElementById('t').textContent="Couldn't load the captcha. Check your connection, then close this window and try again."};
document.head.appendChild(s);
</script></body></html>"#;

    /// The webview profile; deleted before each run and after the helper exits.
    fn profile_dir() -> PathBuf {
        crate::config::paths().cache_dir.join("captcha-webview")
    }

    /// Hands the page its sitekey and rqdata. `<` is escaped so rqdata cannot close the script tag.
    fn page_html(sitekey: &str, rqdata: Option<&str>) -> String {
        let params = json!({ "sitekey": sitekey, "rqdata": rqdata }).to_string().replace('<', "\\u003c");
        PAGE.replace("/*PARAMS*/null", &params)
    }

    /// Same rule as `api::auth`: what can safely go into the page.
    fn sitekey_ok(k: &str) -> bool {
        !k.is_empty() && k.len() <= 64 && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    }

    /// `{"token": "..."}` with a printable-ASCII token of 1 to 8192 bytes.
    fn token_from_json(s: &str) -> Option<String> {
        let v: Value = serde_json::from_str(s).ok()?;
        let t = v.get("token")?.as_str()?;
        ((1..=8192).contains(&t.len()) && t.bytes().all(|b| (0x21..=0x7E).contains(&b))).then(|| t.to_owned())
    }

    /// The page itself and hCaptcha's own hosts; the webview goes nowhere else.
    fn allowed_nav(u: &str) -> bool {
        if u == ORIGIN || u.starts_with("about:") {
            return true;
        }
        let Some(rest) = u.strip_prefix("https://") else { return false };
        let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
        !host.contains('@') && (host == "hcaptcha.com" || host.ends_with(".hcaptcha.com"))
    }

    // ----- parent side -----

    /// Removes the profile once the helper is gone; its browser processes may still write for a moment.
    struct Cleanup(PathBuf);

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let dir = self.0.clone();
            std::thread::spawn(move || {
                for _ in 0..30 {
                    if std::fs::remove_dir_all(&dir).is_ok() && !dir.exists() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            });
        }
    }

    /// Opens the window and waits for the user. Aborting the task (Cancel, a new attempt, logout) kills
    /// the helper: it is `kill_on_drop`.
    pub async fn solve(ch: &CaptchaChallenge) -> CaptchaReply {
        let dir = profile_dir();
        let _ = std::fs::remove_dir_all(&dir); // a crash's leftover
        // Declared before the child: dropped after it.
        let _cleanup = Cleanup(dir);
        let Ok(exe) = std::env::current_exe() else { return CaptchaReply::Unavailable };
        let spawned = tokio::process::Command::new(exe)
            .arg("--captcha")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let Ok(mut child) = spawned else { return CaptchaReply::Unavailable };
        if let Some(mut stdin) = child.stdin.take() {
            let line = format!("{}\n", json!({ "sitekey": ch.sitekey, "rqdata": ch.rqdata }));
            if stdin.write_all(line.as_bytes()).await.is_err() {
                return CaptchaReply::Unavailable;
            }
            // Dropped here: the helper reads one line.
        }
        let Some(stdout) = child.stdout.take() else { return CaptchaReply::Unavailable };
        let mut line = String::new();
        match tokio::time::timeout(TIMEOUT, BufReader::new(stdout).read_line(&mut line)).await {
            Ok(Ok(n)) if n > 0 => token_from_json(&line).map_or(CaptchaReply::Cancelled, |t| CaptchaReply::Solved(Secret::new(t))),
            // The window closed without a token; a helper that could not build a webview says so by its exit code.
            Ok(_) => match tokio::time::timeout(Duration::from_secs(3), child.wait()).await {
                Ok(Ok(status)) if status.code() == Some(EXIT_NO_WEBVIEW) => CaptchaReply::Unavailable,
                _ => CaptchaReply::Cancelled,
            },
            Err(_) => CaptchaReply::Cancelled,
        }
    }

    // ----- helper side -----

    /// `rustcord --captcha`: reads `{"sitekey", "rqdata"}` from stdin, shows the widget, prints
    /// `{"token": ...}` and exits. Closing the window exits without a line.
    pub fn helper_main() {
        let mut line = String::new();
        let input: Value = std::io::stdin().read_line(&mut line).ok().and_then(|_| serde_json::from_str(&line).ok()).unwrap_or(Value::Null);
        let sitekey = input.get("sitekey").and_then(Value::as_str).filter(|k| sitekey_ok(k));
        let Some(sitekey) = sitekey else {
            eprintln!("rustcord --captcha: no valid sitekey on stdin");
            std::process::exit(2);
        };
        let rqdata = input.get("rqdata").and_then(Value::as_str).filter(|d| !d.is_empty());
        let page = page_html(sitekey, rqdata);
        std::process::exit(window::run(page));
    }

    mod window {
        use super::{EXIT_NO_WEBVIEW, TITLE, allowed_nav, profile_dir, token_from_json};
        use std::io::Write as _;
        use winit::application::ApplicationHandler;
        use winit::dpi::LogicalSize;
        use winit::event::WindowEvent;
        use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
        use winit::window::{Window, WindowId};

        struct Helper {
            page: String,
            proxy: EventLoopProxy<String>,
            context: wry::WebContext,
            // The webview before the window: it must go first.
            webview: Option<wry::WebView>,
            window: Option<Window>,
            code: i32,
        }

        /// Windows: wry serves the custom protocol `discord` at `https://discord.*`, so `discord://com/`
        /// is the page at `https://discord.com/`; every other request of that webview is a 404.
        #[cfg(windows)]
        pub(super) fn serve(page: &str, uri: &wry::http::Uri) -> wry::http::Response<std::borrow::Cow<'static, [u8]>> {
            let ok = *uri == "discord://com/";
            let body: std::borrow::Cow<'static, [u8]> = if ok { page.as_bytes().to_vec().into() } else { Default::default() };
            wry::http::Response::builder()
                .status(if ok { 200 } else { 404 })
                .header("Content-Type", "text/html; charset=utf-8")
                .body(body)
                .expect("static response parts")
        }

        impl Helper {
            fn build(&mut self, window: &Window) -> Result<wry::WebView, wry::Error> {
                let proxy = self.proxy.clone();
                let builder = wry::WebViewBuilder::new_with_web_context(&mut self.context)
                    .with_incognito(true)
                    .with_devtools(false)
                    .with_navigation_handler(|u| allowed_nav(&u))
                    .with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
                    .with_ipc_handler(move |r| {
                        if let Some(t) = token_from_json(r.body()) {
                            let _ = proxy.send_event(t);
                        }
                    });
                #[cfg(windows)]
                let builder = {
                    use wry::WebViewBuilderExtWindows as _;
                    let page = self.page.clone();
                    builder.with_custom_protocol("discord".into(), move |_, req| serve(&page, req.uri())).with_https_scheme(true).with_url("discord://com/")
                };
                let webview = builder.build(window)?;
                #[cfg(target_os = "macos")]
                {
                    // wry loads HTML without a base URL; the widget needs the discord.com origin.
                    use super::ORIGIN;
                    use objc2_foundation::{NSString, NSURL};
                    use wry::WebViewExtMacOS as _;
                    let base = NSURL::URLWithString(&NSString::from_str(ORIGIN));
                    // SAFETY: plain WKWebView call on the main thread with live objects.
                    unsafe { webview.webview().loadHTMLString_baseURL(&NSString::from_str(&self.page), base.as_deref()) };
                }
                Ok(webview)
            }
        }

        impl ApplicationHandler<String> for Helper {
            fn resumed(&mut self, el: &ActiveEventLoop) {
                if self.window.is_some() {
                    return;
                }
                let attrs = Window::default_attributes()
                    .with_title(TITLE)
                    .with_inner_size(LogicalSize::new(420.0, 640.0))
                    .with_min_inner_size(LogicalSize::new(340.0, 480.0));
                #[cfg(windows)]
                let attrs = attrs.with_window_icon(icon());
                let window = match el.create_window(attrs) {
                    Ok(w) => w,
                    Err(e) => {
                        eprintln!("rustcord --captcha: no window ({e})");
                        self.code = EXIT_NO_WEBVIEW;
                        return el.exit();
                    }
                };
                center(&window);
                match self.build(&window) {
                    Ok(wv) => self.webview = Some(wv),
                    Err(e) => {
                        eprintln!("rustcord --captcha: no webview ({e})");
                        self.code = EXIT_NO_WEBVIEW;
                        return el.exit();
                    }
                }
                window.focus_window();
                self.window = Some(window);
            }

            fn user_event(&mut self, el: &ActiveEventLoop, token: String) {
                let mut out = std::io::stdout();
                let _ = writeln!(out, "{}", serde_json::json!({ "token": token }));
                let _ = out.flush();
                el.exit();
            }

            fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
                if matches!(event, WindowEvent::CloseRequested) {
                    el.exit();
                }
            }
        }

        #[cfg(windows)]
        fn icon() -> Option<winit::window::Icon> {
            let img = image::load_from_memory(include_bytes!("../assets/icons/rustcord-64.png")).ok()?.to_rgba8();
            let (w, h) = img.dimensions();
            winit::window::Icon::from_rgba(img.into_raw(), w, h).ok()
        }

        fn center(window: &Window) {
            if let Some(m) = window.current_monitor().or_else(|| window.primary_monitor()) {
                let (mp, ms, ws) = (m.position(), m.size(), window.outer_size());
                let x = mp.x + (ms.width.saturating_sub(ws.width) / 2) as i32;
                let y = mp.y + (ms.height.saturating_sub(ws.height) / 2) as i32;
                window.set_outer_position(winit::dpi::PhysicalPosition::new(x, y));
            }
        }

        /// Runs the window; returns the exit code (0 also when it was closed).
        pub(super) fn run(page: String) -> i32 {
            let mut builder = EventLoop::<String>::with_user_event();
            #[cfg(target_os = "macos")]
            {
                // No Dock icon for the helper.
                use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS as _};
                builder.with_activation_policy(ActivationPolicy::Accessory);
            }
            let Ok(el) = builder.build() else {
                eprintln!("rustcord --captcha: no event loop");
                return EXIT_NO_WEBVIEW;
            };
            let mut app = Helper {
                page,
                proxy: el.create_proxy(),
                context: wry::WebContext::new(Some(profile_dir())),
                webview: None,
                window: None,
                code: 0,
            };
            if el.run_app(&mut app).is_err() {
                return EXIT_NO_WEBVIEW;
            }
            // The webview and its browser process end before the profile is deleted.
            let code = app.code;
            drop(app);
            code
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_page_cannot_be_broken_out_of() {
            let html = page_html("10000000-ffff-ffff-ffff-000000000001", Some("</script><script>x"));
            assert!(!html.contains("</script><script>"));
            assert!(html.contains("10000000-ffff-ffff-ffff-000000000001") && html.contains("host=discord.com"));
            assert!(!html.contains("/*PARAMS*/"));
            let start = html.find("const P=").unwrap() + "const P=".len();
            let end = html[start..].find(";\nfunction ready").unwrap();
            let params: Value = serde_json::from_str(&html[start..start + end]).unwrap();
            assert_eq!(params, json!({ "sitekey": "10000000-ffff-ffff-ffff-000000000001", "rqdata": "</script><script>x" }));
            assert!(page_html("k", None).contains(r#""rqdata":null"#));
        }

        #[test]
        fn reads_the_token_line() {
            assert_eq!(token_from_json(r#"{"token":"P1_eyJ0"}"#).as_deref(), Some("P1_eyJ0"));
            assert!(token_from_json(&format!(r#"{{"token":"{}"}}"#, "a".repeat(8193))).is_none());
            assert!(token_from_json(&format!(r#"{{"token":"{}"}}"#, "a".repeat(8192))).is_some());
            assert!(token_from_json(r#"{"token":"two words"}"#).is_none());
            assert!(token_from_json(r#"{"token":"tokén"}"#).is_none());
            assert!(token_from_json(r#"{"token":""}"#).is_none());
            assert!(token_from_json(r#"{"other":"x"}"#).is_none() && token_from_json("not json").is_none());
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
        fn sitekeys_are_checked() {
            assert!(sitekey_ok("10000000-ffff-ffff-ffff-000000000001"));
            assert!(!sitekey_ok("") && !sitekey_ok("</script>") && !sitekey_ok(&"a".repeat(65)));
        }

        #[cfg(windows)]
        #[test]
        fn only_the_page_is_served() {
            let at = |u: &str| super::window::serve("<p>", &u.parse().unwrap());
            let ok = at("discord://com/");
            assert_eq!((ok.status().as_u16(), ok.body().as_ref()), (200, b"<p>".as_slice()));
            assert_eq!(at("discord://com/favicon.ico").status().as_u16(), 404);
            assert_eq!(at("discord://gg/x").status().as_u16(), 404);
        }
    }
}
