// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 图表渲染
//
//   文件:       src/ui/charts.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! 图表渲染抽象。
//! Direct2D 与 GDI 都是受支持的图表后端。初始化时明确选择后端，运行期绘制错误会
//! 记录 HRESULT，调用方只在已知的 GDI 后端或已记录的帧失败后使用 GDI 绘制器。

use std::cell::{Cell, Ref, RefCell};
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{D2DERR_RECREATE_TARGET, RECT as WinRect};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1_RENDER_TARGET_USAGE_NONE, D2D1CreateFactory,
    ID2D1DCRenderTarget, ID2D1Factory, ID2D1SolidColorBrush,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::HDC as WinHdc;
use windows_numerics::Vector2;

use crate::infrastructure::diagnostics::{self, Field, Level};
use crate::infrastructure::native::record_hresult_error;

type SysRect = windows_sys::Win32::Foundation::RECT;
type SysHdc = windows_sys::Win32::Graphics::Gdi::HDC;

const GRID_STROKE_WIDTH: f32 = 1.0;
const SERIES_STROKE_WIDTH: f32 = 2.0;
static CHART_BACKEND_FALLBACK_RECORDED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy)]
pub enum ChartColor {
    // 统一图表配色枚举，避免页面代码直接散落 RGB 常量。
    Black,
    Green,
    Yellow,
    Red,
    Cyan,
    Grid,
}

pub struct ChartRenderer {
    // `ChartRenderer` 是页面能看到的唯一渲染入口。
    backend: RefCell<RendererBackend>,
    last_frame_error: Cell<Option<i32>>,
    recreate_pending: Cell<bool>,
}

enum RendererBackend {
    // GDI 路径由性能页/网络页现有的完整软件绘制器实现。
    Direct2D(Direct2DRenderer),
    Gdi,
}

struct Direct2DRenderer {
    // 共享一份 D2D DC RenderTarget，逐帧绑定到不同 HDC 上使用。
    target: ID2D1DCRenderTarget,
    black: ID2D1SolidColorBrush,
    green: ID2D1SolidColorBrush,
    yellow: ID2D1SolidColorBrush,
    red: ID2D1SolidColorBrush,
    cyan: ID2D1SolidColorBrush,
    grid: ID2D1SolidColorBrush,
}

pub struct ChartFrame<'a> {
    // `ChartFrame` 代表一帧有效的绘制上下文，结束时由调用方显式提交。
    renderer: Ref<'a, Direct2DRenderer>,
    last_frame_error: &'a Cell<Option<i32>>,
    recreate_pending: &'a Cell<bool>,
    rect: WinRect,
    ended: bool,
}

impl ChartRenderer {
    pub fn new() -> Self {
        let backend = match Direct2DRenderer::new() {
            Ok(renderer) => RendererBackend::Direct2D(renderer),
            Err(error) => {
                record_backend_fallback(error);
                RendererBackend::Gdi
            }
        };
        Self {
            backend: RefCell::new(backend),
            last_frame_error: Cell::new(None),
            recreate_pending: Cell::new(false),
        }
    }

    pub fn begin_frame(&self, hdc: SysHdc, rect: SysRect) -> Option<ChartFrame<'_>> {
        if !self.recover_target_with(Direct2DRenderer::new) {
            return None;
        }
        let renderer = Ref::filter_map(self.backend.borrow(), |backend| match backend {
            RendererBackend::Direct2D(renderer) => Some(renderer),
            RendererBackend::Gdi => None,
        })
        .ok()?;
        match renderer.bind_frame(hdc, rect) {
            Ok(rect) => Some(ChartFrame {
                renderer,
                last_frame_error: &self.last_frame_error,
                recreate_pending: &self.recreate_pending,
                rect,
                ended: false,
            }),
            Err(error) => {
                self.frame_failed("Direct2D chart BindDC", error);
                None
            }
        }
    }

    fn frame_failed(&self, context: &'static str, error: i32) {
        record_frame_error(&self.last_frame_error, context, error);
        if error == D2DERR_RECREATE_TARGET.0 {
            self.recreate_pending.set(true);
        }
    }

    fn recover_target_with(&self, create: impl FnOnce() -> Result<Direct2DRenderer, i32>) -> bool {
        if !self.recreate_pending.get() {
            return true;
        }
        // An outstanding frame owns a shared Ref: keep its COM resources alive until it ends.
        let Ok(mut backend) = self.backend.try_borrow_mut() else {
            return false;
        };
        *backend = match create() {
            Ok(renderer) => RendererBackend::Direct2D(renderer),
            Err(error) => {
                record_backend_fallback(error);
                RendererBackend::Gdi
            }
        };
        self.recreate_pending.set(false);
        self.last_frame_error.set(None);
        true
    }

    pub fn is_available(&self) -> bool {
        !self.recreate_pending.get()
            && matches!(*self.backend.borrow(), RendererBackend::Direct2D(_))
    }

    #[cfg(debug_assertions)]
    #[allow(dead_code)]
    pub fn debug_backend_name(&self) -> &'static str {
        match *self.backend.borrow() {
            RendererBackend::Direct2D(_) => "direct2d",
            RendererBackend::Gdi => "gdi",
        }
    }

    #[cfg(test)]
    pub(crate) fn simulate_failed_recreation(&self) {
        self.frame_failed("test target lost", D2DERR_RECREATE_TARGET.0);
        assert!(self.recover_target_with(|| Err(0x80004005u32 as i32)));
    }
}

impl Default for ChartRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl Direct2DRenderer {
    fn new() -> Result<Self, i32> {
        // 安全性: Direct2D COM wrapper methods validate HRESULTs; the factory and render target
        // are retained by `Direct2DRenderer` and used on the UI drawing thread.
        unsafe {
            // 图表绘制是 2D 单线程工作负载，因此单线程 D2D factory 足够。
            let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)
                .map_err(windows_error_code)?;
            let properties = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    // A DC target cannot infer a surface format from its destination HDC.
                    // Direct2D requires this explicit GDI-compatible format.
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_IGNORE,
                },
                dpiX: 0.0,
                dpiY: 0.0,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let target = factory
                .CreateDCRenderTarget(&properties)
                .map_err(windows_error_code)?;
            let black = target
                .CreateSolidColorBrush(&color(0, 0, 0), None)
                .map_err(windows_error_code)?;
            let green = target
                .CreateSolidColorBrush(&color(0, 255, 0), None)
                .map_err(windows_error_code)?;
            let yellow = target
                .CreateSolidColorBrush(&color(255, 255, 0), None)
                .map_err(windows_error_code)?;
            let red = target
                .CreateSolidColorBrush(&color(255, 0, 0), None)
                .map_err(windows_error_code)?;
            let cyan = target
                .CreateSolidColorBrush(&color(0, 200, 255), None)
                .map_err(windows_error_code)?;
            let grid = target
                .CreateSolidColorBrush(&color(0, 128, 64), None)
                .map_err(windows_error_code)?;
            Ok(Self {
                target,
                black,
                green,
                yellow,
                red,
                cyan,
                grid,
            })
        }
    }

    fn bind_frame(&self, hdc: SysHdc, rect: SysRect) -> Result<WinRect, i32> {
        // 安全性: `hdc` and `rect` come from the current paint operation; D2D calls are
        // synchronous and brushes are owned by the returned frame until `end`.
        unsafe {
            // D2D 绑定到 HDC 后，后续绘制统一使用局部坐标系。
            let rect = to_win_rect(rect);
            let local_rect = WinRect {
                left: 0,
                top: 0,
                right: rect.right - rect.left,
                bottom: rect.bottom - rect.top,
            };
            self.target
                .BindDC(WinHdc(hdc as _), &rect)
                .map_err(windows_error_code)?;
            self.target.BeginDraw();

            Ok(local_rect)
        }
    }
}

impl ChartFrame<'_> {
    pub fn clear_black(&self) {
        // 大多数图表都以黑底开始，因此单独提供快捷入口。
        self.fill_color(self.rect, ChartColor::Black);
    }

    pub fn bounds(&self) -> SysRect {
        // 暴露当前帧的局部边界，方便上层布局和裁剪。
        SysRect {
            left: self.rect.left,
            top: self.rect.top,
            right: self.rect.right,
            bottom: self.rect.bottom,
        }
    }

    pub(crate) fn fill_color(&self, rect: impl IntoWinRect, color: ChartColor) {
        // 统一走颜色枚举，避免页面层直接碰 D2D brush。
        self.fill_rect(rect.into_win_rect(), self.brush(color));
    }

    pub fn draw_grid_line(&self, x0: f32, y0: f32, x1: f32, y1: f32, color: ChartColor) {
        self.draw_line_with_width(x0, y0, x1, y1, color, GRID_STROKE_WIDTH);
    }

    pub fn draw_series_line(&self, x0: f32, y0: f32, x1: f32, y1: f32, color: ChartColor) {
        self.draw_line_with_width(x0, y0, x1, y1, color, SERIES_STROKE_WIDTH);
    }

    fn draw_line_with_width(
        &self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        color: ChartColor,
        width: f32,
    ) {
        // 网格线和数据折线共用这一条底层线段绘制路径。
        // 安全性: the render target is inside an active BeginDraw/EndDraw frame and the brush
        // belongs to this frame.
        unsafe {
            self.renderer.target.DrawLine(
                Vector2 { X: x0, Y: y0 },
                Vector2 { X: x1, Y: y1 },
                self.brush(color),
                width,
                None,
            );
        }
    }

    pub fn fill_rect(&self, rect: WinRect, brush: &ID2D1SolidColorBrush) {
        // 填充矩形是最常见的图表背景/块状绘制原语。
        let rect = to_d2d_rect(rect);
        // 安全性: the render target is inside an active frame and `rect`/`brush` are valid for
        // the duration of the synchronous call.
        unsafe {
            self.renderer.target.FillRectangle(&rect, brush);
        }
    }

    pub fn end(mut self) -> bool {
        // `EndDraw` 失败时由调用方决定是否退回 GDI。
        // 安全性: this consumes the active frame and closes the matching BeginDraw call.
        self.finish()
    }

    fn brush(&self, color: ChartColor) -> &ID2D1SolidColorBrush {
        match color {
            ChartColor::Black => &self.renderer.black,
            ChartColor::Green => &self.renderer.green,
            ChartColor::Yellow => &self.renderer.yellow,
            ChartColor::Red => &self.renderer.red,
            ChartColor::Cyan => &self.renderer.cyan,
            ChartColor::Grid => &self.renderer.grid,
        }
    }

    fn finish(&mut self) -> bool {
        if self.ended {
            return true;
        }
        self.ended = true;
        match unsafe { self.renderer.target.EndDraw(None, None) } {
            Ok(()) => {
                self.last_frame_error.set(None);
                true
            }
            Err(error) => {
                record_frame_error(
                    self.last_frame_error,
                    "Direct2D chart EndDraw",
                    error.code().0,
                );
                if error.code() == D2DERR_RECREATE_TARGET {
                    self.recreate_pending.set(true);
                }
                false
            }
        }
    }
}

impl Drop for ChartFrame<'_> {
    fn drop(&mut self) {
        self.finish();
    }
}

#[track_caller]
fn record_backend_fallback(error: i32) {
    if !mark_backend_fallback_recorded(&CHART_BACKEND_FALLBACK_RECORDED) {
        return;
    }
    diagnostics::event(
        Level::Warn,
        "charts.backend_fallback",
        "charts",
        "Direct2D chart initialization failed; GDI was selected",
        &[
            Field::text("source_backend", "direct2d"),
            Field::text("target_backend", "gdi"),
            Field::text("error_domain", "hresult"),
            Field::unsigned("error_code", u64::from(error as u32)),
        ],
    );
}

fn mark_backend_fallback_recorded(state: &AtomicBool) -> bool {
    !state.swap(true, Ordering::AcqRel)
}

fn windows_error_code(error: windows::core::Error) -> i32 {
    error.code().0
}

fn record_frame_error(state: &Cell<Option<i32>>, component: &str, error: i32) {
    if state.replace(Some(error)) != Some(error) {
        record_hresult_error(component, error);
    }
}

fn to_d2d_rect(rect: WinRect) -> D2D_RECT_F {
    // Win32 像素矩形和 D2D 浮点矩形之间的轻量转换。
    D2D_RECT_F {
        left: rect.left as f32,
        top: rect.top as f32,
        right: rect.right as f32,
        bottom: rect.bottom as f32,
    }
}

pub(crate) trait IntoWinRect {
    // 让页面层可以在 `windows` 和 `windows-sys` 两套 RECT 之间无痛复用接口。
    fn into_win_rect(self) -> WinRect;
}

impl IntoWinRect for WinRect {
    fn into_win_rect(self) -> WinRect {
        self
    }
}

impl IntoWinRect for SysRect {
    fn into_win_rect(self) -> WinRect {
        to_win_rect(self)
    }
}

fn to_win_rect(rect: SysRect) -> WinRect {
    WinRect {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
    }
}

fn color(red: u8, green: u8, blue: u8) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: f32::from(red) / 255.0,
        g: f32::from(green) / 255.0,
        b: f32::from(blue) / 255.0,
        a: 1.0,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::ptr::null_mut;
    use windows::core::Interface;
    use windows_sys::Win32::Graphics::Gdi::{
        CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, HBITMAP,
        HGDIOBJ, ReleaseDC, SelectObject,
    };

    pub(crate) struct TestSurface {
        pub(crate) dc: SysHdc,
        bitmap: HBITMAP,
        previous: HGDIOBJ,
    }

    impl TestSurface {
        pub(crate) fn new() -> Self {
            unsafe {
                let screen = GetDC(null_mut());
                assert!(!screen.is_null());
                let dc = CreateCompatibleDC(screen);
                let bitmap = CreateCompatibleBitmap(screen, 64, 64);
                ReleaseDC(null_mut(), screen);
                assert!(!dc.is_null() && !bitmap.is_null());
                let previous = SelectObject(dc, bitmap as HGDIOBJ);
                assert!(!previous.is_null());
                Self {
                    dc,
                    bitmap,
                    previous,
                }
            }
        }
    }

    impl Drop for TestSurface {
        fn drop(&mut self) {
            // SAFETY: restore the fixture's selection before deleting its owned GDI resources.
            unsafe {
                SelectObject(self.dc, self.previous);
                DeleteObject(self.bitmap as HGDIOBJ);
                DeleteDC(self.dc);
            }
        }
    }

    fn rect() -> SysRect {
        SysRect {
            left: 0,
            top: 0,
            right: 64,
            bottom: 64,
        }
    }

    #[test]
    fn target_loss_rebuilds_native_target_and_brushes_after_the_frame_releases_them() {
        let surface = TestSurface::new();
        let renderer = ChartRenderer::new();
        assert!(
            renderer.is_available(),
            "fixture requires a working D2D DC backend: {:?}",
            Direct2DRenderer::new().err()
        );
        let old_target = match &*renderer.backend.borrow() {
            RendererBackend::Direct2D(r) => r.target.clone(),
            RendererBackend::Gdi => unreachable!(),
        };
        let frame = renderer.begin_frame(surface.dc, rect()).unwrap();
        frame.clear_black();
        renderer.frame_failed("test target lost", D2DERR_RECREATE_TARGET.0);
        assert!(!renderer.is_available());
        assert!(!renderer.recover_target_with(|| panic!("outstanding frame resources replaced")));
        assert!(frame.end());
        let frame = renderer
            .begin_frame(surface.dc, rect())
            .expect("target should recover");
        assert_ne!(old_target.as_raw(), frame.renderer.target.as_raw());
        frame.clear_black();
        for color in [
            ChartColor::Green,
            ChartColor::Yellow,
            ChartColor::Red,
            ChartColor::Grid,
        ] {
            frame.fill_color(
                WinRect {
                    left: 2,
                    top: 2,
                    right: 8,
                    bottom: 8,
                },
                color,
            );
        }
        assert!(frame.end());
        assert_eq!(
            unsafe { windows_sys::Win32::Graphics::Gdi::GetPixel(surface.dc, 4, 4) },
            0x00408000
        );
        assert!(renderer.is_available());
    }

    #[test]
    fn dc_target_rejects_the_old_unknown_pixel_format() {
        use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_UNKNOWN;
        let factory: ID2D1Factory =
            unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None) }.unwrap();
        let properties = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_UNKNOWN,
                alphaMode: D2D1_ALPHA_MODE_IGNORE,
            },
            dpiX: 0.0,
            dpiY: 0.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        assert!(unsafe { factory.CreateDCRenderTarget(&properties) }.is_err());
    }

    #[test]
    fn failed_recreation_selects_gdi_without_retrying_the_failed_target_every_frame() {
        let renderer = ChartRenderer::new();
        renderer.simulate_failed_recreation();
        assert!(!renderer.is_available());
        assert!(renderer.recover_target_with(|| panic!("GDI fallback rebuilt each frame")));
        assert!(renderer.begin_frame(null_mut(), rect()).is_none());
    }

    #[test]
    fn dc_render_target_creates_with_a_gdi_compatible_pixel_format() {
        let renderer = Direct2DRenderer::new()
            .unwrap_or_else(|error| panic!("DC render target creation failed: {error:#010X}"));
        // Safety: the renderer owns a successfully created target for this whole query.
        let format = unsafe { renderer.target.GetPixelFormat() };
        assert_eq!(format.format, DXGI_FORMAT_B8G8R8A8_UNORM);
        assert_eq!(format.alphaMode, D2D1_ALPHA_MODE_IGNORE);
    }

    #[test]
    fn backend_fallback_is_reported_once_per_process_state() {
        let state = AtomicBool::new(false);
        assert!(mark_backend_fallback_recorded(&state));
        assert!(!mark_backend_fallback_recorded(&state));
    }
}
