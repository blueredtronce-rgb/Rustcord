# egui / eframe 0.37.0 cheat sheet (glow backend)

Every item below was read from the 0.37.0 crate sources in
`~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/` (egui, eframe, epaint, emath, ecolor,
egui-winit, egui_glow). File hints like `egui/src/ui.rs` point there. If a name you know from
0.29–0.31 is missing here, check the **Removed / renamed** table first.

---

## Removed / renamed (old name → 0.37)

| Old name (0.29–0.31 era; some already changed in 0.31) | 0.37 |
|---|---|
| `App::update(&mut self, ctx, frame)` | `App::ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame)` (required) + optional `App::logic(&mut self, ctx: &egui::Context, frame)` |
| `SidePanel::left(id)`, `TopBottomPanel::top(id)` | `egui::Panel::left/right/top/bottom(id_salt)` — the old types do not exist |
| `panel.show(ctx, ..)` / `show_inside(ui, ..)` | `Panel::show(ui, ..)`, `CentralPanel::show(ui, ..)` (`show_inside` is deprecated, renamed to `show`) |
| `show_animated_inside` / `show_animated_between_inside` | `Panel::show_collapsible(ui, &mut bool, ..)` / `Panel::show_switched(..)` (old names deprecated) |
| `ctx.style()`, `ctx.set_style()`, `ctx.style_mut()` | `ctx.global_style() -> Arc<Style>`, `ctx.set_global_style(..)`, `ctx.global_style_mut(\|s\| ..)`, `ctx.all_styles_mut(\|s\| ..)`, `ctx.style_mut_of(Theme, ..)` |
| `Id::new(x)` | deprecated → `Id::unique(x)` (global) or `IdSalt::new(x)` (local salt) |
| `ui.id()` / `UiBuilder::id()` | deprecated → `ui.scope_id()` / `UiBuilder::scope_id()` |
| `ui.close_menu()` | `ui.close()` (also `ui.close_kind(UiKind)`) |
| `Frame::none()` | `Frame::NONE` or `Frame::new()` (both = no fill, no margin) |
| `Rounding` | `CornerRadius` (u8 per corner: `nw, ne, sw, se`) |
| `Margin` with f32 fields | `Margin { left, right, top, bottom: i8 }` (`MarginF32` exists for floats) |
| `ScrollArea` `state.offset` (field) | private; `state.clamped_offset()` / `state.unclamped_offset()` (`State::offset()` deprecated) |
| `ctx.screen_rect()` | `ctx.content_rect()` (minus safe-area insets) / `ctx.viewport_rect()` |
| `ctx.used_size()` / `ctx.available_rect()` | do not exist → `ctx.globally_used_rect()`, `ctx.content_rect()`, `ui.available_rect_before_wrap()`, `ui.available_size()` |
| `ctx.frame_nr()` | `ctx.cumulative_frame_nr() -> u64`, `ctx.cumulative_pass_nr() -> u64` |
| `Event::Screenshot { image, .. }` | does not exist → callback: `ctx.request_screenshot(\|img: Arc<ColorImage>\| ..)` |
| `DroppedFile { path, bytes, .. }` struct | `trait DroppedFile { fn path(&self) -> &Path; fn bytes(&self) -> Result<Vec<u8>, String>; }`, `raw.dropped_files: Vec<DroppedFileHandle>` (`Arc<dyn DroppedFile + Send + Sync>`) |
| `ImageButton` | does not exist → `Button::image(img)`, `Button::image_and_text(img, text)`, `ui.button((img, "text"))` |
| `menu::bar(ui, ..)` | `egui::MenuBar::new().ui(ui, \|ui\| ..)` |
| `ui.fonts(\|f\| f.layout_job(..))` | layout needs `&mut`: `ui.fonts_mut(\|f\| f.layout_job(job))` or `painter.layout_job(job)` |
| `TextEdit::horizontal_align/vertical_align` | deprecated → `.align(Align2)` |
| `global_theme_preference_switch` | deprecated → `global_theme_preference_buttons` |
| `AtomLayout` | deprecated alias → `WidgetAtom` |
| `egui::WidgetType` | deprecated → `egui::Role` (accesskit) |

---

## 0. Cargo (glow only, no wgpu)

eframe default features (`eframe/Cargo.toml`): `accesskit, clipboard, default_fonts, links, system_fonts,
wayland, web_screen_reader, wgpu, winit/default, x11`. **wgpu is default, glow is not.** If both are on,
`Renderer::default()` picks **Wgpu**. MSRV: `rust-version = "1.95"`.

```toml
eframe = { version = "0.37", default-features = false, features = [
    "glow", "default_fonts", "accesskit", "clipboard", "links", "wayland", "x11",
] }
# optional: "system_fonts" (pulls crate egui_system_font_provider, not in the local registry yet)
# do NOT add "persistence" if you want persistence off
```
- `default-features = false` also drops `winit/default` (only matters on Linux; not checked further).
- Emoji: default fonts no longer ship emoji. Only `egui-icons` is bundled. Monochrome emoji needs the egui feature
  `monochrome_emoji_fonts`; color emoji (COLR/sbix/CBDT) needs the **epaint** feature `color_fonts` plus a color
  emoji font you load yourself; or eframe `system_fonts` (on-demand system fallback, `NativeOptions::system_font_fallback`).
- `egui::hex_color!` needs the egui feature `color-hex`; `Color32::from_hex` always works.

---

## 1. eframe

### run_native
```rust
pub fn run_native(app_name: &str, native_options: NativeOptions, app_creator: AppCreator<'_>) -> eframe::Result
pub type AppCreator<'app> = Box<dyn 'app + FnOnce(&CreationContext<'_>) -> Result<Box<dyn 'app + App>, Box<dyn std::error::Error + Send + Sync>>>;
pub type Result<T = (), E = eframe::Error> = core::result::Result<T, E>;
// also: run_native_ext(app_name, opts, Option<egui::Context>, creator), run_ui_native(app_name, opts, |ui, frame| ..)
```
```rust
fn main() -> eframe::Result {
    eframe::run_native("chat", opts, Box::new(|cc| Ok(Box::new(MyApp::new(cc)))))
}
```
`app_name` = persistence folder name and Wayland app id; also the title if the viewport has none.

### NativeOptions (`eframe/src/epi.rs`) — all pub fields, `Default` provided
| field | type | default |
|---|---|---|
| `viewport` | `egui::ViewportBuilder` | default |
| `multisampling` | `u16` | 0 |
| `depth_buffer` / `stencil_buffer` | `u8` | 0 |
| `renderer` | `eframe::Renderer` (`Glow` / `Wgpu`, by feature) | Glow if only `glow` |
| `run_and_return` | `bool` | true |
| `event_loop_builder` | `Option<EventLoopBuilderHook>` | None |
| `window_builder` | `Option<Box<dyn FnOnce(ViewportBuilder) -> ViewportBuilder>>` | None |
| `centered` | `bool` | false |
| `glow_options` | `egui_glow::GlowConfiguration { vsync: bool, hardware_acceleration: HardwareAcceleration, shader_version: Option<ShaderVersion> }` | vsync true, `Preferred` |
| `persist_window` | `bool` | true (only with `persistence`) |
| `system_font_fallback` | `bool` | true (only with `system_fonts`) |
| `persistence_path` | `Option<PathBuf>` | None |
| `dithering` | `bool` | true |
| `clipboard_shortcuts` | `bool` | true (false = Ctrl+C/X/V arrive as `Event::Key`, TextEdit stops handling them) |

- **vsync** lives in `glow_options.vsync` (no top-level `vsync` field). `HardwareAcceleration::{Required, Preferred, Off}`.
- **Persistence off**: just don't enable the `persistence` feature (`App::save` is then never called). Belt and braces: `persist_window: false` and `fn persist_egui_memory(&self) -> bool { false }`.

```rust
let opts = eframe::NativeOptions {
    viewport: egui::ViewportBuilder::default()
        .with_title("Chat").with_inner_size([1200.0, 800.0]).with_min_inner_size([640.0, 400.0])
        .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")).unwrap())
        .with_app_id("my-chat"),
    renderer: eframe::Renderer::Glow,
    glow_options: egui_glow::GlowConfiguration { vsync: true, ..Default::default() }, // eframe::egui_glow re-export
    persist_window: false,
    ..Default::default()
};
```
### ViewportBuilder (`egui/src/viewport.rs`), all `fn(self, ..) -> Self`
`with_title(impl Into<String>)`, `with_inner_size(impl Into<Vec2>)`, `with_min_inner_size(..)`, `with_max_inner_size(..)`,
`with_position(impl Into<Pos2>)`, `with_icon(impl Into<Arc<IconData>>)`, `with_app_id(impl Into<String>)`,
`with_decorations(bool)`, `with_resizable(bool)`, `with_maximized(bool)`, `with_fullscreen(bool)`, `with_transparent(bool)`,
`with_visible(bool)`, `with_active(bool)`, `with_drag_and_drop(bool)` (Windows only), `with_window_level(WindowLevel)`,
`with_always_on_top()`, `with_taskbar(bool)`, `with_monitor(usize)`, `with_close_button/minimize_button/maximize_button(bool)`.
Default icon is egui's "e"; for the OS default use `IconData::default()`.

### IconData (`egui::IconData`, re-exported as `eframe::egui::IconData`)
```rust
pub struct IconData { pub rgba: Vec<u8> /* unmultiplied */, pub width: u32, pub height: u32 }
eframe::icon_data::from_png_bytes(&[u8]) -> Result<IconData, image::ImageError>   // native only, png
```

### CreationContext<'s>
Fields: `egui_ctx: egui::Context`, `integration_info: IntegrationInfo`, `storage: Option<&dyn Storage>`,
`gl: Option<Arc<glow::Context>>` (glow), `get_proc_address: Option<Arc<dyn Fn(&CStr) -> *const c_void + Send + Sync>>` (glow).
Method: `cc.winit_window() -> Option<&Arc<winit::window::Window>>`. Implements `HasWindowHandle`/`HasDisplayHandle`.
Setup fonts/style here: `cc.egui_ctx.set_fonts(..)`, `cc.egui_ctx.set_theme(..)`.

### App trait
```rust
pub trait App {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut Frame) {}   // before every ui(); ALSO runs while the window is
                                                                       // hidden/minimized if request_repaint() was called. No UI/painting here.
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut Frame);           // REQUIRED. Root Ui: no margin, no background.
    fn save(&mut self, _storage: &mut dyn Storage) {}                 // only with "persistence"
    fn on_exit(&mut self, _gl: Option<&glow::Context>) {}             // this signature when feature "glow" is on
    fn auto_save_interval(&self) -> Duration { 30 s }
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4]       // default Color32::from_rgba_unmultiplied(12,12,12,180).to_normalized_gamma_f32()
    fn persist_egui_memory(&self) -> bool { true }
    fn raw_input_hook(&mut self, _ctx: &egui::Context, _raw_input: &mut egui::RawInput) {}
}
```
- While hidden (minimized/occluded) eframe runs **no** UI pass; only `logic`. Put network polling / notifications in `logic`.
  Force a UI pass while hidden: `ctx.send_viewport_cmd(ViewportCommand::RequestPaintWhileHidden)` (one frame).
- There is no `on_close_event`: check `ctx.input(|i| i.viewport().close_requested())` and send `ViewportCommand::CancelClose`.
- In `ui`, the Context is `ui.ctx()`; `Ui` also `Deref`s to `Context`, so `ui.input(..)`, `ui.request_repaint()` work.

### eframe::Frame methods
`is_web()`, `info() -> &IntegrationInfo` (field `cpu_usage: Option<f32>`), `storage() / storage_mut()`,
`winit_window() -> Option<&Arc<Window>>`, `gl() -> Option<&Arc<glow::Context>>`,
`register_native_glow_texture(glow::Texture) -> TextureId`, `set_traffic_lights_position(..)` (macOS only),
`request_activation_token()` / `take_activation_token() -> Option<String>` (Linux only).
No `close()` / `set_window_title()` → use `ViewportCommand`. Name clash: `eframe::Frame` vs `egui::Frame` (container).

---

## 2. Context

### Repaint / time / frame counters
```rust
ctx.request_repaint();                                   // works from any thread (Context: Clone + Send + Sync)
ctx.request_repaint_after(Duration::from_millis(500));   // smallest pending duration wins
ctx.request_repaint_after_secs(0.5);
ctx.time() -> f64;  ctx.input(|i| (i.time, i.stable_dt, i.unstable_dt, i.predicted_dt));
ctx.cumulative_frame_nr() -> u64;  ctx.cumulative_pass_nr() -> u64;
```

### Style / visuals / theme (`egui/src/context.rs`)
```rust
ctx.set_theme(egui::Theme::Dark);                 // impl Into<ThemePreference>; default is ThemePreference::System!
ctx.global_style() -> Arc<Style>;                 // style of the CURRENT theme
ctx.global_style_mut(|s: &mut Style| ..);         // current theme only
ctx.set_global_style(impl Into<Arc<Style>>);      // current theme only
ctx.all_styles_mut(|s| ..);                       // dark AND light
ctx.style_of(Theme) / ctx.style_mut_of(Theme, |s| ..) / ctx.set_style_of(Theme, style)
ctx.set_visuals(Visuals)  /  ctx.set_visuals_of(Theme, Visuals)
ui.style() -> &Arc<Style>;  ui.style_mut() -> &mut Style;  ui.set_style(..);  ui.visuals_mut();  ui.spacing_mut()
```
Gotcha: with the default `ThemePreference::System`, `set_visuals` only patches the theme active now. For a fixed
dark skin: `ctx.set_theme(Theme::Dark); ctx.set_visuals_of(Theme::Dark, my_visuals);`.

### Zoom
```rust
ctx.zoom_factor() -> f32;  ctx.set_zoom_factor(f32);       // applies next pass
ctx.pixels_per_point() -> f32;  ctx.set_pixels_per_point(f32);  ctx.native_pixels_per_point() -> Option<f32>
```

### Fonts (`epaint/src/text/font_definitions.rs`, `font_data.rs`)
```rust
pub struct FontDefinitions { pub font_data: BTreeMap<String, Arc<FontData>>, pub families: BTreeMap<FontFamily, Vec<String>> }
FontData::from_static(&'static [u8]) / FontData::from_owned(Vec<u8>) / FontData::from_blob(Arc<dyn AsRef<[u8]>+Send+Sync>, index)
FontData { font: Blob, index: u32, tweak: FontTweak }   .tweak(FontTweak { scale, y_offset_factor, y_offset, .. })
ctx.set_fonts(FontDefinitions)        // replaces all; active next pass (or immediately if no text laid out yet)
ctx.add_font(FontInsert)              // keeps existing
FontInsert::new(name: &str, data: FontData, families: Vec<InsertFontFamily>)
InsertFontFamily { family: FontFamily, priority: FontPriority::{Highest, Lowest} }
```
```rust
let mut fonts = egui::FontDefinitions::default();
fonts.font_data.insert("inter".into(), Arc::new(egui::FontData::from_owned(bytes)));   // Arc REQUIRED
fonts.families.get_mut(&egui::FontFamily::Proportional).unwrap().insert(0, "inter".into());
fonts.families.insert(egui::FontFamily::Name("bold".into()), vec!["inter-bold".into()]);
cc.egui_ctx.set_fonts(fonts);
// or additive:
ctx.add_font(egui::FontInsert::new("emoji", egui::FontData::from_owned(b),
    vec![egui::InsertFontFamily { family: egui::FontFamily::Proportional, priority: egui::FontPriority::Lowest }]));
```
Other: `ctx.add_font_provider(Arc<dyn FontProvider>)`, `ctx.set_missing_glyph_policy(..)`.

### Input (`egui/src/input_state/mod.rs`)
```rust
ctx.input(|i: &InputState| ..);  ctx.input_mut(|i| ..);
// InputState fields: raw: RawInput, pointer: PointerState, time: f64, modifiers: Modifiers, keys_down: HashSet<Key>,
//   events: Vec<Event>, focused: bool, pixels_per_point, smooth_scroll_delta: Vec2, stable_dt, ...
i.key_pressed(Key::Enter); i.key_down(k); i.key_released(k); i.num_presses(k)   // modifiers: use i.modifiers, not Key
i.consume_key(Modifiers::COMMAND, Key::K) -> bool;  i.consume_shortcut(&KeyboardShortcut::new(mods, key))
i.modifiers.{alt, ctrl, shift, mac_cmd, command}       // Modifiers::{NONE, ALT, CTRL, SHIFT, MAC_CMD, COMMAND}
i.pointer.hover_pos() / interact_pos() / latest_pos() -> Option<Pos2>; i.pointer.primary_clicked(); secondary_clicked(); any_down()
i.viewport() -> &ViewportInfo  { focused: Option<bool>, minimized, maximized, fullscreen, occluded: Option<bool>,
                                 inner_rect, outer_rect, monitor_size, native_pixels_per_point, title, events }
i.viewport().visible() -> Option<bool>;  i.viewport().close_requested() -> bool
i.raw.focused: bool;  i.focused: bool                 // window has keyboard focus
i.raw.hovered_files: Vec<HoveredFile { path: Option<PathBuf>, mime: String }>
i.raw.dropped_files: Vec<DroppedFileHandle>           // Arc<dyn DroppedFile + Send + Sync>
```
consume/shortcut matching uses `Modifiers::matches_logically`: **extra Shift/Alt are ignored**, so test the more
specific combo first. `key_pressed` ignores modifiers entirely.
```rust
let dropped = ctx.input(|i| i.raw.dropped_files.clone());
for f in dropped { let path = f.path(); let bytes: Result<Vec<u8>, String> = f.bytes(); }
```
Windows: `dropped_files` is always empty if `ViewportBuilder::with_drag_and_drop(false)`.

Event enum (`egui/src/data/input/event.rs`): `Copy, Cut, Paste(String), PasteImage(Arc<ColorImage>)` (clipboard image when no
text; new), `MiddleClickPaste { pos, text }, Text(String), Key { key, physical_key, pressed, repeat, modifiers },
ModifiersChanged, PointerMoved, MouseMoved, PointerButton { pos, button, pressed, modifiers }, PointerGone, Zoom, Rotate,
Ime(ImeEvent), Touch {..}, MouseWheel { unit, delta, phase, source, modifiers }, WindowFocused(bool), AccessKitActionRequest`.
**No `Event::Screenshot`.**

### Viewport commands
```rust
ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!("({n}) Chat")));
ctx.send_viewport_cmd(egui::ViewportCommand::Close);           // CancelClose to veto
ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(egui::UserAttentionType::Informational));
// UserAttentionType::{Critical, Informational, Reset}; also Minimized(bool), Maximized(bool), Visible(bool),
// InnerSize(Vec2), Icon(Option<Arc<IconData>>), RequestCopy/RequestCut/RequestPaste, RequestPaintWhileHidden
```
### Screenshot (callback, not an event)
```rust
ctx.request_screenshot(move |img: Arc<egui::ColorImage>| { tx.send(img).ok(); });  // fn(&self, impl FnOnce(Arc<ColorImage>) + Send + 'static)
// == ctx.send_viewport_cmd(ViewportCommand::Screenshot(egui::ScreenshotCallback::new(cb)))
```
Called once after the next frame is rendered (glow handles it). It does **not** trigger a repaint; call
`ctx.request_repaint()` from the callback if the UI should react.

### Clipboard / URLs / cursor
```rust
ctx.copy_text(String);  ctx.copy_image(ColorImage);
ctx.open_url(egui::OpenUrl::new_tab(url));  // OpenUrl { url: String, new_tab: bool }, OpenUrl::same_tab(url)
ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
ctx.send_cmd(egui::OutputCommand::{CopyText(String), CopyImage(ColorImage), OpenUrl(OpenUrl), ..});
```
Paste of an image arrives as `Event::PasteImage(Arc<ColorImage>)` in `i.events`.

### Ids and temp storage
```rust
Id::unique("settings_modal")          // global; Id::new deprecated
Id::NULL;  id.with(salt) -> Id;  IdSalt::new(x)
ui.make_persistent_id(salt) -> Id     // = ui.scope_id().with(salt), stable
ui.scope_id()  /  ui.unique_id() (unstable)  /  ui.next_auto_id()
ctx.data_mut(|d| d.insert_temp(id, value));               // T: 'static + Any + Clone + Send + Sync
let v: Option<T> = ctx.data(|d| d.get_temp::<T>(id));
ctx.data_mut(|d| *d.get_temp_mut_or_default::<T>(id) += 1);
ctx.data_mut(|d| d.remove_temp::<T>(id));                 // also insert_persisted/get_persisted (need serde)
ctx.memory(|m| m.focused()) ; ctx.memory_mut(|m| m.request_focus(id))
```
### Rects
`ctx.content_rect()`, `ctx.viewport_rect()`, `ctx.globally_used_rect()`, `ctx.is_pointer_over_egui()`,
`ctx.egui_wants_keyboard_input()`, `ctx.text_edit_focused()`, `ctx.any_popup_open()`.

---

## 3. Layout containers

### Panel (`egui/src/containers/panel.rs`)
```rust
egui::Panel::left(id_salt) / right(..) / top(..) / bottom(..)      // id_salt: impl AsIdSalt
  .resizable(bool)            // default true
  .default_size(f32)          // width for left/right, height for top/bottom; INCLUDING frame margin
  .min_size(f32) .max_size(f32) .size_range(impl Into<Rangef>) .exact_size(f32)
  .frame(egui::Frame) .show_separator_line(bool) .drag_to_open(bool)
  .show(ui, |ui| ..) -> InnerResponse<R>
  .show_collapsible(ui, &mut is_expanded, |ui| ..) -> Option<InnerResponse<R>>
Panel::show_switched(ui, &mut is_expanded, collapsed_panel, expanded_panel, |ui, expanded| ..)
```
Defaults: left/right 200 wide, range 96..∞; top/bottom auto height, min 20. Default frame `Frame::side_top_panel(style)`.
Add panels before `CentralPanel`; add `Window`s after panels.
```rust
egui::Panel::left("servers").exact_size(72.0).resizable(false).show(ui, |ui| { .. });
egui::Panel::left("channels").default_size(240.0).size_range(180.0..=400.0).show(ui, |ui| { .. });
egui::Panel::bottom("composer").frame(egui::Frame::new().inner_margin(8)).show(ui, |ui| { .. });
egui::CentralPanel::default().show(ui, |ui| { .. });    // must be last
```
### CentralPanel
`CentralPanel::default()` (frame = `Frame::central_panel(style)`: fill `panel_fill`, margin 8),
`CentralPanel::no_frame()`, `CentralPanel::default_margins()`, `.frame(Frame)`, `.show(ui, ..) -> InnerResponse<R>`.

### Frame (container) (`egui/src/containers/frame.rs`)
```rust
pub struct Frame { pub inner_margin: Margin, pub fill: Color32, pub stroke: Stroke, pub corner_radius: CornerRadius,
                   pub outer_margin: Margin, pub shadow: Shadow }
Frame::NONE  Frame::new()  Frame::group(&style)  Frame::side_top_panel(&style)  Frame::central_panel(&style)
Frame::window(&style)  Frame::menu(&style)  Frame::popup(&style)  Frame::canvas(&style)  Frame::dark_canvas(&style)
.inner_margin(impl Into<Margin>) .outer_margin(impl Into<Margin>) .fill(Color32) .stroke(impl Into<Stroke>)
.corner_radius(impl Into<CornerRadius>) .shadow(Shadow) .multiply_with_opacity(f32)
.show(ui, |ui| ..) -> InnerResponse<R>      // also begin(ui) -> Prepared { content_ui, .. } ... .end(ui)
```
```rust
egui::Frame::new().fill(c).inner_margin(egui::Margin::symmetric(12, 6)).corner_radius(8).stroke((1.0, border)).show(ui, |ui| ..);
```
- `Margin` (i8): `Margin::same(i8)`, `Margin::symmetric(x: i8, y: i8)`, `Margin::ZERO`, struct literal `{left,right,top,bottom}`;
  `From<i8>`, `From<f32>` (rounded), `From<Vec2>`. Integer literals infer `i8` (range -128..=127).
- `CornerRadius` (u8): `CornerRadius::same(u8)`, `CornerRadius::ZERO`, `{nw, ne, sw, se}`; `From<u8>`, `From<f32>` (rounded, max 255).
- `Stroke::new(width, color)`, `Stroke::NONE`, `From<(f32, Color)>`.
- `Shadow { offset: [i8; 2], blur: u8, spread: u8, color }`, `Shadow::NONE`.

### ScrollArea (`egui/src/containers/scroll_area.rs`)
```rust
ScrollArea::vertical() / horizontal() / both() / neither() / new(impl Into<Vec2b>)
  .id_salt(impl AsIdSalt) .auto_shrink(impl Into<Vec2b>)      // e.g. [false; 2] or false
  .stick_to_bottom(bool) .stick_to_right(bool)
  .vertical_scroll_offset(f32) .horizontal_scroll_offset(f32) .scroll_offset(Vec2)   // force offset this frame
  .max_height(f32) .max_width(f32) .min_scrolled_height(f32) .animated(bool) .content_margin(impl Into<Margin>)
  .scroll_bar_visibility(ScrollBarVisibility::{AlwaysHidden, VisibleWhenNeeded, AlwaysVisible})
  .scroll_source(ScrollSource) .wheel_scroll_multiplier(Vec2)
  .show(ui, |ui| ..) -> ScrollAreaOutput<R>
  .show_rows(ui, row_height_sans_spacing: f32, total_rows: usize, |ui, range: Range<usize>| ..)
  .show_viewport(ui, |ui, viewport: Rect| ..)       // viewport is relative to content top-left
pub struct ScrollAreaOutput<R> { pub inner: R, pub id: Id, pub state: scroll_area::State, pub content_size: Vec2, pub inner_rect: Rect }
out.state.clamped_offset() -> Vec2;  out.state.unclamped_offset();  out.state.velocity();  out.state.overscroll()
scroll_area::State::load(ctx, id) -> Option<State>;  state.store(ctx, id)
```
Scroll-to helpers live on Ui/Response: `ui.scroll_to_cursor(Some(Align::BOTTOM))`, `ui.scroll_to_rect(rect, Option<Align>)`,
`response.scroll_to_me(Some(Align::Center))`, `ui.scroll_with_delta(Vec2)`, `*_animation(.., ScrollAnimation)` variants.
```rust
let out = egui::ScrollArea::vertical().id_salt("msgs").auto_shrink(false).stick_to_bottom(true).show(ui, |ui| { .. });
let at_bottom = out.state.clamped_offset().y >= out.content_size.y - out.inner_rect.height() - 1.0;
```

### Window / Modal / Area (take `&Context`, not `&mut Ui`)
```rust
egui::Window::new(title /*IntoAtoms*/).id(Id).open(&mut bool).resizable(impl Into<Vec2b>).collapsible(bool)
  .title_bar(bool).default_size(impl Into<Vec2>).min_size(..).max_size(..).fixed_size(..).default_pos(..)
  .anchor(Align2, impl Into<Vec2>).frame(Frame).scroll(impl Into<Vec2b>).movable(bool).order(Order)
  .show(ui.ctx(), |ui| ..) -> Option<InnerResponse<Option<R>>>
let r = egui::Modal::new(Id::unique("confirm")).backdrop_color(c).frame(f).show(ui.ctx(), |ui| ..);  // -> ModalResponse<T>
if r.should_close() { open = false; }   // fields: response, backdrop_response, inner, is_top_modal, any_popup_open
egui::Area::new(Id::unique("toast")).anchor(Align2::RIGHT_BOTTOM, [-12.0, -12.0]).order(Order::Foreground)
  .show(ui.ctx(), |ui| ..) -> InnerResponse<R>
```
`Order::{Background, Middle, Foreground, Tooltip, Debug}`.

### Popups / menus (`containers/popup.rs`, `containers/menu.rs`)
```rust
response.context_menu(|ui| { if ui.button("Copy").clicked() { ui.close(); } }) -> Option<InnerResponse<()>>
response.context_menu_opened() -> bool
response.container_context_menu(|ui| ..)   // right-click anywhere in a container response (ui.horizontal(..).response),
                                           // even on child widgets that sense clicks; container need not sense clicks
ui.menu_button("More", |ui| ..) -> InnerResponse<Option<R>>       // nests as submenu inside a menu
ui.menu_image_button(img, |ui| ..);  ui.menu_image_text_button(img, "text", |ui| ..)
egui::MenuBar::new().ui(ui, |ui| { ui.menu_button("File", |ui| ..); });
egui::Popup::from_toggle_button_response(&btn).close_behavior(PopupCloseBehavior::CloseOnClickOutside).show(|ui| ..)
egui::Popup::menu(&btn).show(|ui| ..);  egui::Popup::context_menu(&resp).show(|ui| ..)
egui::Popup::new(id, ctx.clone(), anchor /*Rect|Pos2|&Response|PopupAnchor*/, layer_id)
  .open(bool) .open_bool(&mut bool) .open_memory(Option<SetOpenCommand>) .at_pointer() .at_pointer_fixed()
  .align(RectAlign) .gap(f32) .width(f32) .frame(Frame) .layout(Layout) .id(Id) .show(|ui| ..) -> Option<InnerResponse<R>>
Popup::is_id_open(ctx, id) / open_id / toggle_id / close_id / close_all(ctx) / is_any_open(ctx)
```
`PopupCloseBehavior::{CloseOnClick, CloseOnClickOutside, IgnoreClicks}`. Labels don't sense clicks:
`ui.add(Label::new(t).sense(Sense::click())).context_menu(..)`.

### Ui layout helpers (`egui/src/ui.rs`)
```rust
ui.horizontal(|ui| ..)  ui.horizontal_centered  ui.horizontal_top  ui.horizontal_wrapped(|ui| ..)
ui.vertical(|ui| ..)  ui.vertical_centered  ui.vertical_centered_justified  ui.centered_and_justified(|ui| ..)
ui.with_layout(Layout::right_to_left(Align::Center), |ui| ..)    // Layout::{left_to_right, right_to_left, top_down, top_down_justified, bottom_up}(Align)
                                                                  // .with_main_wrap(bool) .with_cross_justify(bool) .with_main_align(Align)
ui.allocate_ui(desired: Vec2, |ui| ..) -> InnerResponse<R>;  ui.allocate_ui_with_layout(size, layout, |ui| ..)
ui.allocate_exact_size(Vec2, Sense) -> (Rect, Response);  ui.allocate_response(Vec2, Sense);  ui.allocate_rect(Rect, Sense)
ui.allocate_painter(Vec2, Sense) -> (Response, Painter)
ui.add_sized(impl Into<Vec2>, widget);  ui.put(Rect, widget);  ui.add_space(f32);  ui.add_enabled(bool, w);  ui.add_visible(bool, w)
ui.scope(|ui| ..);  ui.scope_builder(UiBuilder::new().max_rect(r).layout(l).id_salt(s), |ui| ..);  ui.push_id(salt, |ui| ..)
ui.group(|ui| ..);  ui.indent(salt, |ui| ..);  ui.columns(n, |cols| ..)
ui.available_size()  ui.available_width()  ui.available_rect_before_wrap()  ui.max_rect()  ui.min_rect()  ui.cursor()
ui.set_width/height/min_width/max_width(..);  ui.take_available_space();  ui.text_style_height(&TextStyle::Body)
ui.response() -> Response (this Ui's own response);  ui.close()
egui::Sides::new().height(f32).spacing(f32).shrink_left().truncate().show(ui, |ui| left, |ui| right) -> (L, R)
```
### CollapsingHeader / Grid / ComboBox
```rust
egui::CollapsingHeader::new("Text channels").default_open(true).id_salt(s).show(ui, |ui| ..) -> CollapsingResponse<R>
ui.collapsing("Heading", |ui| ..)
egui::Grid::new("g").num_columns(2).striped(true).spacing([8.0, 4.0]).min_col_width(f32)
    .show(ui, |ui| { ui.label("a"); ui.label("b"); ui.end_row(); });
egui::ComboBox::from_label("Status").selected_text(cur.as_str())     // also ComboBox::new(id_salt, label), from_id_salt(..)
    .width(f32).show_ui(ui, |ui| { ui.selectable_value(&mut cur, v, "Online"); }) -> InnerResponse<Option<R>>
ComboBox::from_id_salt("x").show_index(ui, &mut idx, len, |i| items[i].clone()) -> Response
```

---

## 4. Widgets

Most widget text params are `impl IntoAtoms<'a>`: `&str`, `String`, `RichText`, `LayoutJob`, `Image`, or tuples
`(Image, "text")`. Plain text params are `impl Into<WidgetText>`.

### Label (`egui/src/widgets/label.rs`)
```rust
egui::Label::new(impl Into<WidgetText>)
  .wrap_mode(TextWrapMode::{Extend, Wrap, Truncate}) .wrap() .truncate() .extend()
  .halign(Align) .selectable(bool) .sense(Sense) .show_tooltip_when_elided(bool)
  .layout_in_ui(ui) -> (Pos2, Arc<Galley>, Response)   // layout only, no paint
ui.label(t)  ui.colored_label(color, t)  ui.heading(t)  ui.monospace(t)  ui.code(t)  ui.small(t)  ui.strong(t)  ui.weak(t)
WidgetText::concat(ui, [RichText::new("a"), RichText::new("b").strong()])   // one paragraph from styled pieces (new)
```
Default wrap mode comes from `Style::wrap_mode`, else Wrap in vertical or wrapping-horizontal layouts, Extend in
plain `horizontal`. Selectable by default (`style.interaction.selectable_labels = true`,
`multi_widget_text_select = true`).

### RichText (`egui/src/widget_text.rs`)
`RichText::new(impl Into<String>)` + `.size(f32) .family(FontFamily) .font(FontId) .text_style(TextStyle) .heading() .monospace()
.code() .strong() .weak() .small() .small_raised() .raised() .underline() .strikethrough() .italics()
.color(impl Into<Color32>) .background_color(impl Into<Color32>) .line_height(Option<f32>) .extra_letter_spacing(f32)
.variation(tag, coord) .variations(iter)`; `.append_to(&mut LayoutJob, &Style, FontSelection, Align)`.
`strong()` only changes **color** (stronger text color), it is not bold. Bold = a bold font via `.family(FontFamily::Name("bold".into()))`
or a variable font with `.variation("wght", 700.0)`.

### Hyperlink / Link
```rust
ui.hyperlink(url);  ui.hyperlink_to(label, url);   // opens via ctx.open_url
ui.add(egui::Hyperlink::from_label_and_url(label, url).open_in_new_tab(true));
ui.link(text) -> Response                           // looks like a link, you handle .clicked()
```
### Button (`egui/src/widgets/button.rs`)
```rust
egui::Button::new(impl IntoAtoms<'a>)  Button::selectable(selected, atoms)  Button::image(impl Into<Image<'a>>)
Button::image_and_text(img, impl Into<WidgetText>)  Button::opt_image_and_text(Option<Image>, Option<WidgetText>)
  .frame(bool) .frame_when_inactive(bool) .fill(impl Into<Color32>) .stroke(impl Into<Stroke>) .min_size(Vec2)
  .corner_radius(impl Into<CornerRadius>) .small() .selected(bool) .wrap() .truncate() .sense(Sense)
  .shortcut_text(atoms) .left_text(atoms) .right_text(atoms) .gap(f32) .image_tint_follows_text_color(bool)
ui.button(atoms)  ui.small_button(atoms)  ui.selectable_label(bool, atoms)  ui.selectable_value(&mut v, val, atoms)
ui.toggle_value(&mut bool, atoms)  ui.radio_value(&mut v, val, atoms)
ui.add(egui::Button::image(egui::Image::new(&tex).fit_to_exact_size(egui::vec2(48.0, 48.0)).corner_radius(24)).frame(false))
```
`Button::image` limits the image to the font height unless sized explicitly. There is no `ImageButton`.

### Image (`egui/src/widgets/image.rs`)
```rust
egui::Image::new(impl Into<ImageSource<'a>>)     // &TextureHandle, (TextureId, Vec2), SizedTexture, uri &str, (uri, bytes)
Image::from_texture(impl Into<SizedTexture>)  Image::from_uri(..)  Image::from_bytes(uri, bytes)
  .fit_to_exact_size(Vec2) .fit_to_original_size(f32) .fit_to_fraction(Vec2) .shrink_to_fit()
  .max_size(Vec2) .max_width(f32) .max_height(f32) .maintain_aspect_ratio(bool)
  .corner_radius(impl Into<CornerRadius>) .tint(impl Into<Color32>) .bg_fill(..) .uv(impl Into<Rect>) .rotate(angle, origin)
  .sense(Sense) .alt_text(..) .show_loading_spinner(bool) .texture_options(TextureOptions)
  .paint_at(ui, rect)                         // paint without allocating
egui::load::SizedTexture { id: TextureId, size: Vec2 }  SizedTexture::new(id, size)  SizedTexture::from_handle(&tex)
ui.image(&tex) -> Response;  ui.add(Image::new(&tex).max_size(vec2(400.0, 300.0)).sense(Sense::click()))
```
`From<&TextureHandle>` and `From<(TextureId, Vec2)>` exist for `SizedTexture`. URI/bytes loading needs installed image
loaders (none in this crate set; decode yourself and use `load_texture`). Circle avatar: `.corner_radius(size / 2.0)` (u8, ≤255).

### TextEdit (`egui/src/widgets/text_edit/builder.rs`)
```rust
egui::TextEdit::singleline(&mut dyn TextBuffer)  TextEdit::multiline(&mut dyn TextBuffer)   // String, &str (read-only)
  .hint_text(impl IntoAtoms<'static>) .desired_rows(usize) /*multiline default 4*/ .desired_width(f32) /*INFINITY = fill*/
  .password(bool) .lock_focus(bool) /*true: Tab inserts \t*/ .return_key(impl Into<Option<KeyboardShortcut>>)
  .font(impl Into<FontSelection>) .text_color(Color32) .background_color(Color32) .frame(Frame) .margin(impl Into<Margin>)
  .char_limit(usize) .min_size(Vec2) .align(Align2) .id(Id) .id_salt(s) .interactive(bool) .cursor_at_end(bool)
  .prefix(atoms) .suffix(atoms) .event_filter(EventFilter) .layouter(&mut dyn FnMut(&Ui, &dyn TextBuffer, f32) -> Arc<Galley>)
  .show(ui) -> TextEditOutput
pub struct TextEditOutput { pub response: WidgetAtomResponse /* .response: Response */, pub galley: Arc<Galley>,
    pub galley_pos: Pos2, pub text_clip_rect: Rect, pub state: TextEditState, pub cursor_range: Option<CCursorRange> }
ui.text_edit_singleline(&mut s) -> Response;  ui.text_edit_multiline(&mut s) -> Response;  ui.add(TextEdit::..) -> Response
response.request_focus();  response.has_focus();  response.lost_focus();  response.changed()
```
Default `return_key` = `Enter` with no modifiers. Matching uses `matches_logically`, so **Shift+Enter also matches**
(both insert `\n` in multiline; singleline loses focus). `Event::Text("\n")` is ignored; newlines come only from `return_key`.

**Enter sends, Shift+Enter newline** (verified against the event loop at builder.rs ~1315):
```rust
let out = egui::TextEdit::multiline(&mut self.draft)
    .hint_text("Message #general").desired_rows(1).desired_width(f32::INFINITY)
    .return_key(egui::KeyboardShortcut::new(egui::Modifiers::SHIFT, egui::Key::Enter)) // only Shift+Enter → '\n'
    .show(ui);
let resp = &out.response.response;
if resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift) {
    send(std::mem::take(&mut self.draft));   // plain Enter inserted nothing
    resp.request_focus();
}
```
Set the cursor: `let mut st = out.state; st.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(n)))); st.store(ui.ctx(), resp.id);`
(`egui::text::{CCursor, CCursorRange}`; `CCursorRange::two(a, b)`, `select_all_str(&s)`). Clear undo: `TextEditState::clear_undoer()`.

### Small widgets
```rust
ui.checkbox(&mut b, atoms);  egui::Checkbox::new(&mut b, atoms).indeterminate(bool);  Checkbox::without_text(&mut b)
ui.add(egui::Slider::new(&mut v, 0.0..=1.0).text("Volume").step_by(0.05).show_value(true).suffix("%"));
ui.add(egui::DragValue::new(&mut v).speed(0.1).range(0..=100).prefix("x: "));
ui.color_edit_button_srgba(&mut color32) -> Response;  ui.color_edit_button_srgb(&mut [u8; 3])
egui::color_picker::color_edit_button_srgba(ui, &mut c, egui::color_picker::Alpha::{Opaque, OnlyBlend, BlendOrAdditive})
egui::color_picker::color_picker_color32(ui, &mut c, alpha) -> bool          // inline picker
ui.separator();  ui.add(egui::Separator::default().spacing(12.0).horizontal() /*.vertical() .grow(f) .shrink(f)*/)
ui.spinner();  ui.add(egui::Spinner::new().size(16.0).color(c))
ui.add(egui::ProgressBar::new(0.4))
```
### Response (`egui/src/response.rs`)
Fields: `ctx, layer_id, id: Id, rect: Rect, interact_rect: Rect, sense: Sense, flags`.
Methods: `clicked() secondary_clicked() middle_clicked() double_clicked() triple_clicked() clicked_by(PointerButton)
long_touched() clicked_elsewhere() hovered() contains_pointer() highlighted() has_focus() gained_focus() lost_focus()
request_focus() surrender_focus() changed() mark_changed() dragged() drag_started() drag_stopped() drag_delta()
interact_pointer_pos() hover_pos() container_hovered() container_clicked() container_secondary_clicked()`;
builders (by value): `on_hover_text(t) on_hover_ui(|ui| ..) on_hover_text_at_pointer(t) on_disabled_hover_text(t)
on_hover_cursor(CursorIcon) on_hover_and_drag_cursor(CursorIcon) highlight()`; `interact(Sense) -> Response`,
`context_menu(..)`, `container_context_menu(..)`, `scroll_to_me(Option<Align>)`, `union(other)` (also `|`, `|=`),
`dnd_set_drag_payload / dnd_hover_payload / dnd_release_payload`.
`ui.interact(rect, id, sense) -> Response` for custom hit areas.

### Sense (bitflags)
`Sense::hover()`, `Sense::click()`, `Sense::drag()`, `Sense::click_and_drag()`, `Sense::focusable_noninteractive()`;
consts `Sense::HOVER, CLICK, DRAG, FOCUSABLE` combinable with `|`.

---

## 5. Text and painting

### LayoutJob / TextFormat (`epaint/src/text/text_layout_types.rs`; `egui::text::{LayoutJob, TextFormat, ..}`)
```rust
pub struct LayoutJob { pub text: String, pub sections: Vec<LayoutSection>, pub wrap: TextWrapping,
    pub first_row_min_height: f32, pub break_on_newline: bool, pub halign: Align, pub justify: bool,
    pub round_output_to_gui: bool, pub keep_trailing_whitespace: bool }        // Default available
pub struct LayoutSection { pub leading_space: f32, pub byte_range: ByteRange /* Range<ByteIndex>, newtype! */, pub format: TextFormat }
pub struct TextWrapping { pub max_width: f32, pub max_rows: usize, pub break_anywhere: bool, pub overflow_character: Option<char> }
pub struct TextFormat { pub font_id: FontId, pub extra_letter_spacing: f32, pub line_height: Option<f32>, pub color: Color32,
    pub background: Color32, pub expand_bg: f32, pub coords: VariationCoords, pub italics: bool,
    pub underline: Stroke, pub strikethrough: Stroke, pub valign: Align /* default BOTTOM */ }
LayoutJob::simple(text: String, FontId, Color32, wrap_width: f32)  ::simple_singleline(text, FontId, Color32)
LayoutJob::single_section(text, TextFormat)  ::simple_format(text, TextFormat)
job.append(&str, leading_space: f32, TextFormat)        // preferred way to build sections
TextFormat::simple(FontId, Color32);  TextFormat { font_id, color, ..Default::default() }  // default color GRAY
```
```rust
let mut job = egui::text::LayoutJob::default();
job.append("bob ", 0.0, egui::TextFormat { font_id: egui::FontId::proportional(15.0), color: name_color, ..Default::default() });
job.append("hi", 0.0, egui::TextFormat { italics: true, underline: egui::Stroke::new(1.0, c), ..Default::default() });
job.wrap.max_width = ui.available_width();
ui.label(job);                                                       // LayoutJob: Into<WidgetText>
```
### Galley
```rust
let g: Arc<Galley> = ui.fonts_mut(|f| f.layout_job(job));     // fonts() is &FontsView: layout/row_height need fonts_mut
let g = ui.painter().layout(text: String, FontId, Color32, wrap_width: f32);
let g = ui.painter().layout_no_wrap(text, FontId, Color32);  let g = ui.painter().layout_job(job);
g.size() -> Vec2;  g.rect;  g.rows: Vec<PlacedRow>;  g.elided: bool;  g.job: Arc<LayoutJob>;  g.intrinsic_size()
FontsView: layout_job(&mut, LayoutJob), layout(&mut, ..), layout_no_wrap, row_height(&mut, &FontId), glyph_width, has_glyph, families()
```
### Painter (`egui/src/painter.rs`)
```rust
let p = ui.painter();  /  ui.painter_at(rect);  /  ctx.layer_painter(LayerId::new(Order::Foreground, id))
p.rect_filled(rect, impl Into<CornerRadius>, impl Into<Color32>) -> ShapeIdx
p.rect_stroke(rect, cr, impl Into<Stroke>, StrokeKind::{Inside, Middle, Outside})        // StrokeKind is REQUIRED
p.rect(rect, cr, fill, stroke, StrokeKind)
p.circle_filled(center: Pos2, radius: f32, fill);  p.circle_stroke(c, r, stroke);  p.circle(c, r, fill, stroke)
p.line_segment([a, b], stroke);  p.hline(x_range, y, stroke);  p.vline(x, y_range, stroke)
p.text(pos, Align2::LEFT_TOP, impl ToString, FontId, Color32) -> Rect
p.image(texture_id: TextureId, rect, uv: Rect, tint: Color32)   // uv: Rect::from_min_max(pos2(0.,0.), pos2(1.,1.)), tint WHITE
p.galley(pos, Arc<Galley>, fallback_color);  p.galley_with_override_text_color(pos, galley, color)
p.add(impl Into<Shape>) -> ShapeIdx;  p.extend(shapes);  p.set(idx, shape)
```
`Shape`: `Shape::rect_filled(rect, cr, fill)`, `rect_stroke(rect, cr, stroke, kind)`, `circle_filled`, `circle_stroke`,
`line_segment`, `line(Vec<Pos2>, PathStroke)`, `dashed_line(..)`, `galley(pos, galley, color)`, `image(tex, rect, uv, tint)`,
`gradient_rect(rect, Direction, [from, to])`, `Shape::text(&mut FontsView, pos, anchor, text, FontId, color)`.

### Color32 (`ecolor/src/color32.rs`)
`Color32::from_rgb(r,g,b)` (const), `from_rgba_unmultiplied(r,g,b,a)` (non-const; const twin `from_rgba_unmultiplied_const`),
`from_rgba_premultiplied(..)` (const), `from_gray(l)`, `from_black_alpha(a)`, `from_white_alpha(a)`,
`Color32::from_hex("#5865f2") -> Result<Color32, ParseHexColorError>` (3/4/6/8 digits, `#` required), `c.to_hex()`,
`gamma_multiply(f32)`, `linear_multiply(f32)`, `lerp_to_gamma(other, t)`, `blend(on_top)`, `to_normalized_gamma_f32()`,
`r() g() b() a()`; consts `TRANSPARENT BLACK WHITE GRAY DARK_GRAY LIGHT_GRAY RED GREEN BLUE YELLOW ... PLACEHOLDER`.

### Fonts / text styles
```rust
FontId::new(size, FontFamily)  FontId::proportional(14.0)  FontId::monospace(13.0)   // fields: size, family
FontFamily::Proportional | FontFamily::Monospace | FontFamily::Name(Arc<str>)
TextStyle::{Small, Body, Monospace, Button, Heading, Name(Arc<str>)}
ctx.all_styles_mut(|s| { s.text_styles.insert(egui::TextStyle::Body, egui::FontId::proportional(15.0)); });
```
### Style / Visuals / Spacing (`egui/src/style.rs`)
- `Style { override_text_style, override_font_id, override_text_valign, text_styles: BTreeMap<TextStyle, FontId>,
  wrap_mode: Option<TextWrapMode>, spacing: Spacing, interaction: Interaction, visuals: Visuals, animation_time,
  url_in_tooltip, scroll_animation, compact_menu_style, .. }`
- `Visuals { dark_mode, override_text_color: Option<Color32>, weak_text_alpha, weak_text_color: Option<Color32>,
  widgets: Widgets, selection: Selection { bg_fill, stroke }, hyperlink_color, faint_bg_color, extreme_bg_color,
  text_edit_bg_color: Option<Color32>, code_bg_color, warn_fg_color, error_fg_color, window_corner_radius: CornerRadius,
  window_shadow, window_fill, window_stroke, menu_corner_radius: CornerRadius, panel_fill, popup_shadow, text_cursor,
  button_frame, collapsing_header_frame, indent_has_left_vline, striped, interact_cursor: Option<CursorIcon>,
  image_loading_spinners, disabled_alpha, .. }`; `Visuals::dark()`, `Visuals::light()`; methods `text_color()`,
  `weak_text_color()`, `window_fill()`. (`clip_rect_margin` is deprecated, no effect.)
- `Widgets { noninteractive, inactive, hovered, active, open: WidgetVisuals }`
- `WidgetVisuals { bg_fill, weak_bg_fill /* button bg */, bg_stroke, corner_radius: CornerRadius, fg_stroke /* text */, expansion }`
- `Spacing { item_spacing: Vec2, window_margin: Margin, button_padding: Vec2, menu_margin: Margin, indent, interact_size: Vec2,
  slider_width, combo_width, text_edit_width, icon_width, icon_spacing, tooltip_width, menu_width, combo_height,
  scroll: ScrollStyle, .. }`; `ScrollStyle::{solid(), thin(), floating()}`, fields `floating, bar_width, handle_min_length,
  bar_inner_margin, bar_outer_margin, floating_width, content_margin, ..`
- `Interaction { selectable_labels, multi_widget_text_select, tooltip_delay, show_tooltips_only_when_still, .. }`
- Built-in widgets (Button, TextEdit, Checkbox, Separator) read their colors from `visuals.widgets.*` through the default
  theme provider (`egui/src/theme/default_style.rs`); `ctx.add_widget_theme::<ButtonStyle>(..)` can replace that.

```rust
ctx.set_theme(egui::Theme::Dark);
ctx.style_mut_of(egui::Theme::Dark, |s| {
    s.visuals.panel_fill = egui::Color32::from_rgb(49, 51, 56);
    s.visuals.widgets.inactive.weak_bg_fill = egui::Color32::from_rgb(64, 66, 73);
    s.visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(4);
    s.spacing.item_spacing = egui::vec2(8.0, 6.0);
    s.spacing.scroll = egui::style::ScrollStyle::thin();
});
```

---

## 6. Textures

```rust
let img = egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba);     // asserts w*h*4 == rgba.len()
// also ColorImage::new(size, Vec<Color32>), from_rgba_premultiplied, from_rgb, from_gray, filled(size, color), example()
// ColorImage { size: [usize; 2], source_size: Vec2, pixels: Vec<Color32> }
let tex: egui::TextureHandle = ctx.load_texture("avatar:123", img, egui::TextureOptions::LINEAR);
// fn load_texture(&self, name: impl Into<String>, image: impl Into<ImageData>, options: TextureOptions) -> TextureHandle
tex.id() -> TextureId;  tex.size() -> [usize; 2];  tex.size_vec2() -> Vec2;  tex.aspect_ratio();  tex.byte_size();  tex.name()
tex.set(new_image, TextureOptions::LINEAR);          // replace contents (&mut self)
tex.set_partial(pos: [usize; 2], image, options);
ctx.load_texture_cached(name, id: Id, image, options) -> TextureHandle   // safe every frame; evicted after 1 unused frame
```
- `TextureHandle` is cheap to `Clone`; when the **last** handle is dropped the texture is freed (GPU memory released).
  Keep handles in your app state (e.g. an LRU of avatars); never call `load_texture` every frame.
- `TextureOptions { magnification, minification: TextureFilter, wrap_mode: TextureWrapMode, mipmap_mode: Option<TextureFilter> }`;
  consts `LINEAR` (= `Default`), `NEAREST`, `LINEAR_REPEAT`, `LINEAR_MIRRORED_REPEAT`, `NEAREST_REPEAT`, `NEAREST_MIRRORED_REPEAT`;
  `.with_mipmap_mode(Some(TextureFilter::Linear))`.
- `ImageData` has one variant `Color(Arc<ColorImage>)`; `From<ColorImage>` and `From<Arc<ColorImage>>`.
- `load_texture` debug-asserts each side ≤ `ctx.input(|i| i.max_texture_side)`.
- Decoding PNG/JPEG/GIF is up to you (eframe's own `image` dep is png-only and internal): decode → `from_rgba_unmultiplied` → `load_texture`.

---

## Multiple viewports (the game overlay, ui/overlay)
- `ctx.show_viewport_deferred(id, builder, cb)` must be called every root pass while the window
  should exist; a viewport not declared in a full root pass is dropped with its window. A
  *logic-only* root pass (minimized, in the tray) creates nothing: send
  `ViewportCommand::RequestPaintWhileHidden` to the root until the viewport appears in
  `ctx.input(|i| i.raw.viewports)`. Any child viewport (even a hidden one) keeps the root running
  unpainted full passes on each event, so destroy a pane instead of hiding it.
- A context created before eframe (`Context::default()` in `main.rs`) has `embed_viewports ==
  true`, which turns every viewport into an egui `Window` inside the root. eframe sets it false only
  on a context it creates itself: call `set_embed_viewports(false)`.
- `App::clear_color` is one value for every viewport; transparent windows need `[0.0; 4]`.
- `ctx.request_repaint*()` without an id targets the viewport being run. From another thread pass
  `ViewportId::ROOT` (`request_repaint_of`, `request_repaint_after_for`).
- `App::raw_input_hook` runs for every viewport's pass: check `raw.viewport_id`.
- winit: `with_mouse_passthrough(true)` makes the window `WS_EX_LAYERED`, which shows nothing until
  `SetLayeredWindowAttributes(hwnd, 0, 255, LWA_ALPHA)`. An undecorated window gets a DWM shadow, a
  grey top line and the Win11 border: `SetWindowRgn` to the client rect removes them. winit
  rewrites `GWL_EXSTYLE` on every show and hit-test change: re-add `WS_EX_TOOLWINDOW |
  WS_EX_NOACTIVATE` and clear `WS_EX_APPWINDOW` each pass.

---

## 7. Label inside `horizontal_wrapped` (inline text flow)

From `Ui::horizontal_wrapped` and `Label::layout_in_ui` (`egui/src/widgets/label.rs` ~L189–228):
- `horizontal_wrapped` = `Layout::left_to_right(Align::Center).with_main_wrap(true)`, initial height `spacing.interact_size.y`.
  `Ui::wrap_mode()` is then `Wrap` (unless `Style::wrap_mode` is set).
- When wrap mode is `Wrap` and the layout is left-to-right + main_wrap, the Label is laid out against the **full row
  width** with `first_section.leading_space = width already used on the current row`. Result: the text **starts right
  after the previous widget on the same row and continues at the left edge of the next rows**. Each row is allocated
  separately (`allocate_rect` per row), so the next widget continues after the end of the label's last row.
  The returned `Response` is the union of all rows.
- `halign` is forced to `Min` and `justify = false` in this mode. `first_row_min_height = cursor.height()`.
- `.truncate()` or `.extend()` (or a `WidgetText::Galley`) skips this path: the label becomes one rectangular block
  that wraps to a new row as a whole.
- `Link`/`Hyperlink` use the same `layout_in_ui`, so links flow inline too. Images/buttons are rectangular blocks.
- `item_spacing.x` is added between consecutive widgets. For seamless runs of styled text:
  `ui.spacing_mut().item_spacing.x = 0.0;` inside the wrapped ui, or build one `LayoutJob` / `WidgetText::concat`
  (single galley, single selection).
```rust
ui.horizontal_wrapped(|ui| {
    ui.spacing_mut().item_spacing.x = 0.0;
    ui.label(egui::RichText::new("alice").strong().color(name_c));
    ui.label(" said: a long message that wraps onto following lines ");
    ui.hyperlink_to("link", "https://example.com");
    ui.add(egui::Image::new(&emoji_tex).fit_to_exact_size(egui::vec2(20.0, 20.0)));  // inline block
});
```

---

## Minimal glow app skeleton
```rust
use eframe::egui;   // eframe re-exports egui, egui::emath, egui::epaint, and (feature glow) egui_glow, glow
struct App { draft: String }
impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _f: &mut eframe::Frame) { /* drain network channel; ctx.request_repaint_after(..) */ }
    fn ui(&mut self, ui: &mut egui::Ui, _f: &mut eframe::Frame) {
        egui::Panel::left("channels").default_size(240.0).show(ui, |ui| { ui.heading("Channels"); });
        egui::Panel::bottom("composer").show(ui, |ui| { /* TextEdit, see §4 */ });
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink(false).show(ui, |ui| { /* messages */ });
        });
    }
}
```
