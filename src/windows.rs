//! All HWNDs and GDI objects stay on the UI thread. Only the immutable data
//! snapshot is shared with the worker; no background thread touches a window.
#![allow(unsafe_op_in_unsafe_fn)]
use agentbar::model::{QuotaWindow, Snapshot, TaskStatus, now, relative_age, reset_countdown};
use std::{
    cell::{Cell, RefCell},
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    System::{LibraryLoader::*, Registry::*, Threading::*},
    UI::{HiDpi::*, Shell::*, WindowsAndMessaging::*},
};

const TRAY_MESSAGE: u32 = WM_APP + 1;
const MENU_DETAILS: usize = 1;
const MENU_REFRESH: usize = 2;
const MENU_FOLDER: usize = 3;
const MENU_EXIT: usize = 4;
const DETAIL_HEIGHT: i32 = 510;
const DETAIL_WIDTH: i32 = 440;
const QUOTA_COLOR: COLORREF = rgb(0x24, 0x7D, 0xDE);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
const fn rgb(r: u32, g: u32, b: u32) -> COLORREF {
    r | (g << 8) | (b << 16)
}
fn rect(x: i32, y: i32, w: i32, h: i32) -> RECT {
    RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    }
}

// A top-down BGRA surface lets GDI draw the content while we supply the alpha
// channel that ordinary GDI drawing does not preserve.
struct DibSurface {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    pixels: *mut u8,
    len: usize,
}

impl DibSurface {
    unsafe fn new(reference: HDC, width: i32, height: i32) -> Option<Self> {
        if width <= 0 || height <= 0 {
            return None;
        }
        let len = (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(4)?;
        let dc = CreateCompatibleDC(reference);
        if dc.is_null() {
            return None;
        }
        let mut info: BITMAPINFO = zeroed();
        info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = width;
        info.bmiHeader.biHeight = -height;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB;
        let mut pixels = null_mut();
        let bitmap = CreateDIBSection(reference, &info, DIB_RGB_COLORS, &mut pixels, null_mut(), 0);
        if bitmap.is_null() || pixels.is_null() {
            if !bitmap.is_null() {
                DeleteObject(bitmap);
            }
            DeleteDC(dc);
            return None;
        }
        let previous = SelectObject(dc, bitmap);
        std::ptr::write_bytes(pixels.cast::<u8>(), 0, len);
        Some(Self {
            dc,
            bitmap,
            previous,
            pixels: pixels.cast(),
            len,
        })
    }
}

impl Drop for DibSurface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.previous);
            DeleteObject(self.bitmap);
            DeleteDC(self.dc);
        }
    }
}

struct App {
    hwnd: HWND,
    detail: HWND,
    data: Arc<Mutex<Snapshot>>,
    refresh: Arc<AtomicBool>,
    offset: i32,
    width: i32,
    scale: f32,
    taskbar: HWND,
    taskbar_rect: RECT,
    bar_rect: RECT,
    dark: bool,
    taskbar_created: u32,
    visible: bool,
    fonts: Vec<HFONT>,
    mask_mode: Cell<bool>,
}

impl App {
    fn px(&self, n: i32) -> i32 {
        (n as f32 * self.scale).round() as i32
    }
    unsafe fn make_fonts(&mut self) {
        for font in self.fonts.drain(..) {
            DeleteObject(font);
        }
        for (height, weight) in [
            (11, 400),
            (12, 400),
            (13, 400),
            (13, 600),
            (17, 600),
            (22, 600),
        ] {
            self.fonts.push(CreateFontW(
                -self.px(height),
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                OUT_DEFAULT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32,
                // Grayscale coverage works on a transparent surface; ClearType
                // assumes a solid background and can leave colored fringes.
                ANTIALIASED_QUALITY as u32,
                DEFAULT_PITCH as u32,
                wide("Segoe UI").as_ptr(),
            ));
        }
    }
    unsafe fn theme(&mut self) {
        let mut light = 0u32;
        let mut bytes = 4u32;
        let status = RegGetValueW(
            HKEY_CURRENT_USER,
            wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize").as_ptr(),
            wide("SystemUsesLightTheme").as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            &mut light as *mut _ as _,
            &mut bytes,
        );
        self.dark = status != 0 || light == 0;
    }
    fn text_color(&self) -> COLORREF {
        if self.dark {
            rgb(237, 241, 247)
        } else {
            rgb(28, 36, 49)
        }
    }
    fn muted(&self) -> COLORREF {
        if self.dark {
            rgb(156, 167, 184)
        } else {
            rgb(91, 104, 121)
        }
    }
    fn background(&self) -> COLORREF {
        if self.dark {
            rgb(29, 33, 40)
        } else {
            rgb(246, 248, 251)
        }
    }
    fn surface(&self) -> COLORREF {
        if self.dark {
            rgb(39, 45, 55)
        } else {
            rgb(233, 238, 245)
        }
    }
    fn accent(&self) -> COLORREF {
        if self.dark {
            rgb(111, 218, 181)
        } else {
            rgb(15, 126, 100)
        }
    }

    fn draw_color(&self, color: COLORREF) -> COLORREF {
        if self.mask_mode.get() {
            rgb(255, 255, 255)
        } else {
            color
        }
    }

    unsafe fn position(&mut self) {
        self.taskbar = FindWindowW(wide("Shell_TrayWnd").as_ptr(), null());
        let mut appbar: APPBARDATA = zeroed();
        appbar.cbSize = size_of::<APPBARDATA>() as u32;
        if SHAppBarMessage(ABM_GETTASKBARPOS, &mut appbar) == 0 || self.taskbar.is_null() {
            ShowWindow(self.hwnd, SW_HIDE);
            self.visible = false;
            return;
        }
        self.taskbar_rect = appbar.rc;
        let dpi = GetDpiForWindow(self.taskbar).max(96);
        let scale = dpi as f32 / 96.0;
        if (scale - self.scale).abs() > 0.01 || self.fonts.is_empty() {
            self.scale = scale;
            self.make_fonts();
        }
        // Only the primary horizontal bottom taskbar is supported in v0.1.
        let t = self.taskbar_rect;
        let taskbar_height = t.bottom - t.top;
        if taskbar_height > self.px(120) || taskbar_height < self.px(24) {
            ShowWindow(self.hwnd, SW_HIDE);
            self.visible = false;
            return;
        }
        let x = t.left + self.px(self.offset);
        let width = self
            .px(self.width)
            .min(t.left + (t.right - t.left) / 2 - self.px(60) - x);
        if width < self.px(280) {
            ShowWindow(self.hwnd, SW_HIDE);
            self.visible = false;
            return;
        }
        let height = (taskbar_height - self.px(6)).min(self.px(44));
        let y = t.top + (taskbar_height - height) / 2;
        let mut should_show = IsWindowVisible(self.taskbar) != 0;
        // A hidden/animating taskbar must not leave the panel floating over apps.
        let mut actual: RECT = zeroed();
        GetWindowRect(self.taskbar, &mut actual);
        let monitor = MonitorFromWindow(self.taskbar, MONITOR_DEFAULTTOPRIMARY);
        let mut info: MONITORINFO = zeroed();
        info.cbSize = size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(monitor, &mut info);
        if actual.top >= info.rcMonitor.bottom - self.px(6) {
            should_show = false;
        }
        let fg = GetForegroundWindow();
        if !fg.is_null() && fg != self.hwnd && fg != self.detail && fg != self.taskbar {
            let mut cls = [0u16; 128];
            GetClassNameW(fg, cls.as_mut_ptr(), cls.len() as i32);
            let name = String::from_utf16_lossy(
                &cls[..cls.iter().position(|v| *v == 0).unwrap_or(cls.len())],
            );
            let mut bounds: RECT = zeroed();
            GetWindowRect(fg, &mut bounds);
            if name != "Progman"
                && name != "WorkerW"
                && bounds.left <= info.rcMonitor.left
                && bounds.top <= info.rcMonitor.top
                && bounds.right >= info.rcMonitor.right
                && bounds.bottom >= info.rcMonitor.bottom
            {
                should_show = false;
            }
        }
        // Reserve enough room for centered task buttons and shell surfaces. If
        // left-aligned or widgets are enabled, stay available through the tray.
        let read_setting = |name: &str, fallback: u32| {
            let mut value = fallback;
            let mut bytes = 4u32;
            RegGetValueW(
                HKEY_CURRENT_USER,
                wide("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Advanced").as_ptr(),
                wide(name).as_ptr(),
                RRF_RT_REG_DWORD,
                null_mut(),
                &mut value as *mut _ as _,
                &mut bytes,
            );
            value
        };
        if read_setting("TaskbarAl", 1) == 0 || read_setting("TaskbarDa", 0) != 0 {
            should_show = false;
        }
        self.bar_rect = rect(x, y, width, height);
        SetWindowPos(
            self.hwnd,
            HWND_TOPMOST,
            x,
            y,
            width,
            height,
            SWP_NOACTIVATE
                | if should_show {
                    SWP_SHOWWINDOW
                } else {
                    SWP_HIDEWINDOW
                },
        );
        self.visible = should_show;
    }

    unsafe fn tray(&self, action: u32) {
        let mut icon: NOTIFYICONDATAW = zeroed();
        icon.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        icon.hWnd = self.hwnd;
        icon.uID = 1;
        icon.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        icon.uCallbackMessage = TRAY_MESSAGE;
        icon.hIcon = LoadIconW(null_mut(), IDI_APPLICATION);
        let text = wide("AgentBar · Codex 余量与任务");
        icon.szTip[..text.len()].copy_from_slice(&text);
        Shell_NotifyIconW(action, &icon);
    }

    unsafe fn toggle_detail(&mut self) {
        if IsWindowVisible(self.detail) != 0 {
            ShowWindow(self.detail, SW_HIDE);
            return;
        }
        let monitor = MonitorFromWindow(self.taskbar, MONITOR_DEFAULTTOPRIMARY);
        let mut info: MONITORINFO = zeroed();
        info.cbSize = size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(monitor, &mut info);
        let w = self.px(DETAIL_WIDTH);
        let h = self.px(DETAIL_HEIGHT);
        let x = self
            .bar_rect
            .left
            .max(info.rcWork.left)
            .min(info.rcWork.right - w);
        let y = (self.taskbar_rect.top - h - self.px(10)).max(info.rcWork.top);
        SetWindowPos(self.detail, HWND_TOPMOST, x, y, w, h, SWP_SHOWWINDOW);
        SetForegroundWindow(self.detail);
        InvalidateRect(self.detail, null(), 0);
    }

    unsafe fn menu(&mut self) {
        let menu = CreatePopupMenu();
        for (id, label) in [
            (MENU_DETAILS, "显示 / 隐藏详情"),
            (MENU_REFRESH, "刷新数据"),
            (MENU_FOLDER, "打开 Codex 数据目录"),
            (MENU_EXIT, "退出 AgentBar"),
        ] {
            AppendMenuW(menu, MF_STRING, id, wide(label).as_ptr());
        }
        let mut point: POINT = zeroed();
        GetCursorPos(&mut point);
        SetForegroundWindow(self.hwnd);
        let command = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            point.x,
            point.y,
            0,
            self.hwnd,
            null(),
        );
        DestroyMenu(menu);
        PostMessageW(self.hwnd, WM_NULL, 0, 0);
        match command as usize {
            MENU_DETAILS => self.toggle_detail(),
            MENU_REFRESH => self.refresh.store(true, Ordering::Relaxed),
            MENU_FOLDER => {
                let home = self
                    .data
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .codex_home
                    .clone();
                if !home.is_empty() {
                    ShellExecuteW(
                        self.hwnd,
                        wide("open").as_ptr(),
                        wide(&home).as_ptr(),
                        null(),
                        null(),
                        SW_SHOWNORMAL,
                    );
                }
            }
            MENU_EXIT => PostQuitMessage(0),
            _ => {}
        }
    }

    #[allow(clippy::too_many_arguments)] // Mirrors a small drawing operation: bounds, font and color.
    unsafe fn text(
        &self,
        dc: HDC,
        text: &str,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        font: usize,
        color: COLORREF,
    ) {
        let old = SelectObject(dc, self.fonts[font]);
        SetTextColor(dc, self.draw_color(color));
        SetBkMode(dc, TRANSPARENT as i32);
        let mut area = rect(self.px(x), self.px(y), self.px(w), self.px(h));
        let text = wide(text);
        DrawTextW(
            dc,
            text.as_ptr(),
            (text.len() - 1) as i32,
            &mut area,
            DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
        SelectObject(dc, old);
    }
    unsafe fn fill(&self, dc: HDC, area: RECT, color: COLORREF) {
        let brush = CreateSolidBrush(self.draw_color(color));
        FillRect(dc, &area, brush);
        DeleteObject(brush);
    }
    unsafe fn round(&self, dc: HDC, area: RECT, color: COLORREF, radius: i32) {
        let brush = CreateSolidBrush(self.draw_color(color));
        let old = SelectObject(dc, brush);
        let pen = SelectObject(dc, GetStockObject(NULL_PEN));
        RoundRect(
            dc,
            area.left,
            area.top,
            area.right,
            area.bottom,
            self.px(radius),
            self.px(radius),
        );
        SelectObject(dc, pen);
        SelectObject(dc, old);
        DeleteObject(brush);
    }
    unsafe fn progress(
        &self,
        dc: HDC,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        quota: Option<&QuotaWindow>,
    ) {
        self.round(
            dc,
            rect(self.px(x), self.px(y), self.px(w), self.px(h)),
            self.surface(),
            h,
        );
        if let Some(q) = quota.filter(|q| !q.expired(now())) {
            let fill = (w as f64 * q.remaining_percent() / 100.0).round() as i32;
            if fill > 0 {
                self.round(
                    dc,
                    rect(self.px(x), self.px(y), self.px(fill.max(h)), self.px(h)),
                    QUOTA_COLOR,
                    h,
                );
            }
        }
    }
    unsafe fn paint(&self, hwnd: HWND) {
        let mut ps: PAINTSTRUCT = zeroed();
        let dc = BeginPaint(hwnd, &mut ps);
        let mut bounds: RECT = zeroed();
        GetClientRect(hwnd, &mut bounds);
        let snapshot = self.data.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if hwnd == self.hwnd {
            self.paint_transparent_bar(dc, &snapshot, bounds);
            EndPaint(hwnd, &ps);
            return;
        }
        let mem = CreateCompatibleDC(dc);
        let bitmap = CreateCompatibleBitmap(dc, bounds.right.max(1), bounds.bottom.max(1));
        let old = SelectObject(mem, bitmap);
        self.fill(mem, bounds, self.background());
        self.paint_detail(mem, &snapshot);
        BitBlt(dc, 0, 0, bounds.right, bounds.bottom, mem, 0, 0, SRCCOPY);
        SelectObject(mem, old);
        DeleteObject(bitmap);
        DeleteDC(mem);
        EndPaint(hwnd, &ps);
    }

    unsafe fn paint_transparent_bar(&self, reference: HDC, snapshot: &Snapshot, bounds: RECT) {
        let Some(color) = DibSurface::new(reference, bounds.right, bounds.bottom) else {
            return;
        };
        let Some(mask) = DibSurface::new(reference, bounds.right, bounds.bottom) else {
            return;
        };
        self.paint_bar(color.dc, snapshot, bounds);
        // Render the same shapes in white on black to recover grayscale glyph
        // coverage. Color was already drawn on black, so its RGB is premultiplied.
        self.mask_mode.set(true);
        self.paint_bar(mask.dc, snapshot, bounds);
        self.mask_mode.set(false);
        GdiFlush();
        let colors = std::slice::from_raw_parts_mut(color.pixels, color.len);
        let coverage = std::slice::from_raw_parts(mask.pixels, mask.len);
        for (pixel, mask_pixel) in colors
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(coverage.as_chunks::<4>().0)
        {
            let alpha = mask_pixel[0];
            for channel in &mut pixel[..3] {
                *channel = (*channel).min(alpha);
            }
            pixel[3] = alpha;
        }
        let size = SIZE {
            cx: bounds.right,
            cy: bounds.bottom,
        };
        let origin = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        UpdateLayeredWindow(
            self.hwnd,
            null_mut(),
            null(),
            &size,
            color.dc,
            &origin,
            0,
            &blend,
            ULW_ALPHA,
        );
    }
    unsafe fn paint_bar(&self, dc: HDC, snapshot: &Snapshot, bounds: RECT) {
        let logical_w = (bounds.right as f32 / self.scale) as i32;
        let logical_h = (bounds.bottom as f32 / self.scale) as i32;
        let cy = (logical_h - 36) / 2;
        let primary = snapshot.usage.as_ref().and_then(|u| u.primary.as_ref());
        let secondary = snapshot.usage.as_ref().and_then(|u| u.secondary.as_ref());
        let countdown_width = 118;
        let quota_width = (logical_w - 8 - 12 - countdown_width - 8).max(1);
        let at = now();
        for (index, quota) in [primary, secondary].into_iter().enumerate() {
            let y = cy + index as i32 * 18;
            self.progress(dc, 8, y + 6, quota_width, 6, quota);
            let mut countdown = quota
                .map(|q| reset_countdown(q.resets_at.saturating_sub(at)))
                .unwrap_or_else(|| "--".into());
            if index == 0 && snapshot.connection == "演示模式" {
                countdown = format!("演示 {countdown}");
            }
            self.text(
                dc,
                &countdown,
                8 + quota_width + 12,
                y,
                countdown_width,
                18,
                0,
                self.text_color(),
            );
        }
    }
    unsafe fn paint_detail(&self, dc: HDC, snapshot: &Snapshot) {
        let text = self.text_color();
        let muted = self.muted();
        self.text(dc, "AgentBar", 22, 15, 280, 32, 4, text);
        self.text(dc, "×", 400, 17, 22, 28, 4, muted);
        self.text(dc, "CODEX · 余量与任务", 23, 48, 370, 22, 0, muted);
        let primary = snapshot.usage.as_ref().and_then(|u| u.primary.as_ref());
        let secondary = snapshot.usage.as_ref().and_then(|u| u.secondary.as_ref());
        for (i, q) in [primary, secondary].into_iter().enumerate() {
            let x = 22 + i as i32 * 204;
            self.round(
                dc,
                rect(self.px(x), self.px(85), self.px(192), self.px(110)),
                self.surface(),
                14,
            );
            let label = q
                .map(QuotaWindow::label)
                .unwrap_or_else(|| "等待额度数据".into());
            self.text(dc, &label, x + 13, 96, 165, 21, 1, muted);
            let value = q
                .map(|q| {
                    if q.expired(now()) {
                        "待刷新".into()
                    } else {
                        format!("{:.0}% 剩余", q.remaining_percent())
                    }
                })
                .unwrap_or_else(|| "--".into());
            self.text(dc, &value, x + 13, 120, 168, 29, 5, text);
            // A distinct track remains visible against the card background.
            self.fill(
                dc,
                rect(self.px(x + 13), self.px(156), self.px(166), self.px(4)),
                if self.dark {
                    rgb(62, 72, 86)
                } else {
                    rgb(209, 219, 230)
                },
            );
            if let Some(q) = q.filter(|q| !q.expired(now())) {
                let w = (166.0 * q.remaining_percent() / 100.0).round() as i32;
                if w > 0 {
                    self.fill(
                        dc,
                        rect(self.px(x + 13), self.px(156), self.px(w), self.px(4)),
                        QUOTA_COLOR,
                    );
                }
            }
            let reset = q
                .map(|q| {
                    if q.expired(now()) {
                        "周期已结束，等待新数据".into()
                    } else {
                        format!("{}后重置", reset_countdown(q.resets_at - now()))
                    }
                })
                .unwrap_or_else(|| "尚未读取到额度".into());
            self.text(dc, &reset, x + 13, 166, 168, 20, 0, muted);
        }
        self.text(dc, "最近任务", 23, 208, 200, 26, 3, text);
        self.text(dc, "刷新 ↻", 349, 208, 68, 26, 1, self.accent());
        if snapshot.tasks.is_empty() {
            self.text(dc, "还没有可读取的本地任务", 23, 250, 390, 25, 2, muted);
            self.text(
                dc,
                "在 Codex 中执行任务后，这里会自动更新",
                23,
                280,
                390,
                25,
                1,
                muted,
            );
        }
        for (i, task) in snapshot.tasks.iter().take(5).enumerate() {
            let y = 241 + i as i32 * 39;
            let active = matches!(task.status, TaskStatus::Active | TaskStatus::Waiting);
            let color = if active { self.accent() } else { muted };
            self.round(
                dc,
                rect(self.px(24), self.px(y + 9), self.px(6), self.px(6)),
                color,
                6,
            );
            self.text(dc, &task.title, 40, y, 257, 22, 1, text);
            let status = format!(
                "{}{}",
                task.status.label(),
                if task.inferred && task.status == TaskStatus::Active {
                    "·推测"
                } else {
                    ""
                }
            );
            self.text(dc, &status, 310, y, 110, 22, 0, color);
            let age = if active {
                task.started_at
                    .map(|t| format!("已运行 {}", relative_age(now() - t)))
                    .unwrap_or_else(|| "当前本地状态".into())
            } else {
                format!("{}前有活动", relative_age(now() - task.updated_at))
            };
            self.text(dc, &age, 40, y + 20, 350, 15, 0, muted);
        }
        self.fill(
            dc,
            rect(self.px(22), self.px(445), self.px(396), self.px(1)),
            self.surface(),
        );
        let usage = snapshot.usage.as_ref();
        let source = usage
            .map(|u| u.source.as_str())
            .unwrap_or(snapshot.connection.as_str());
        let freshness = usage
            .map(|u| format!("{source} · {}前更新", relative_age(now() - u.observed_at)))
            .unwrap_or_else(|| source.into());
        self.text(dc, &freshness, 23, 455, 390, 21, 0, muted);
        let stale = usage.is_some_and(|u| now() - u.observed_at > 300);
        let note = if snapshot.connection == "演示模式" {
            "模拟数据，仅供预览"
        } else if snapshot.error.is_some() {
            snapshot.error.as_deref().unwrap()
        } else if stale {
            "用量快照较旧，等待 Codex 更新"
        } else if snapshot.connection == "本地快照" {
            "日志推测状态；无新事件 3 分钟后标记未知"
        } else {
            "本地连接 · 点击任务可打开 Codex"
        };
        self.text(dc, note, 23, 477, 395, 18, 0, muted);
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        let create = &*(lparam as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
    }
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<App>;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    // SetWindowPos, ShowWindow and modal shell menus synchronously re-enter
    // WndProc. Do not create a second mutable App reference during those calls.
    let Ok(mut app) = (&*ptr).try_borrow_mut() else {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    };
    if msg == app.taskbar_created {
        app.tray(NIM_ADD);
        app.position();
        return 0;
    }
    match msg {
        WM_TIMER => {
            app.position();
            InvalidateRect(app.hwnd, null(), 0);
            if IsWindowVisible(app.detail) != 0 {
                InvalidateRect(app.detail, null(), 0);
            }
            0
        }
        WM_PAINT => {
            app.paint(hwnd);
            0
        }
        WM_ERASEBKGND => 1,
        WM_MOUSEACTIVATE if hwnd == app.hwnd => MA_NOACTIVATE as isize,
        WM_LBUTTONUP => {
            if hwnd == app.hwnd {
                app.toggle_detail();
            } else {
                let x = (lparam as u16 as i16 as f32 / app.scale) as i32;
                let y = ((lparam >> 16) as u16 as i16 as f32 / app.scale) as i32;
                if x >= 395 && y < 50 {
                    ShowWindow(app.detail, SW_HIDE);
                } else if x >= 340 && (205..238).contains(&y) {
                    app.refresh.store(true, Ordering::Relaxed);
                } else if (241..436).contains(&y) {
                    let index = ((y - 241) / 39) as usize;
                    let id = app
                        .data
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .tasks
                        .get(index)
                        .map(|t| t.id.clone());
                    if let Some(id) = id.filter(|s| {
                        !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
                    }) {
                        ShellExecuteW(
                            hwnd,
                            wide("open").as_ptr(),
                            wide(&format!("codex://threads/{id}")).as_ptr(),
                            null(),
                            null(),
                            SW_SHOWNORMAL,
                        );
                    }
                }
            }
            0
        }
        WM_RBUTTONUP => {
            app.menu();
            0
        }
        TRAY_MESSAGE => {
            match lparam as u32 {
                WM_LBUTTONUP => app.toggle_detail(),
                WM_RBUTTONUP | WM_CONTEXTMENU => app.menu(),
                _ => {}
            }
            0
        }
        WM_ACTIVATE if hwnd == app.detail && (wparam & 0xffff) == WA_INACTIVE as usize => {
            ShowWindow(hwnd, SW_HIDE);
            0
        }
        WM_KEYDOWN if wparam == 27 => {
            ShowWindow(app.detail, SW_HIDE);
            0
        }
        WM_DPICHANGED | WM_DISPLAYCHANGE => {
            app.position();
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_SETTINGCHANGE | WM_THEMECHANGED => {
            app.theme();
            app.position();
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_CLOSE if hwnd == app.detail => {
            ShowWindow(hwnd, SW_HIDE);
            0
        }
        WM_CLOSE => {
            PostQuitMessage(0);
            0
        }
        WM_DESTROY if hwnd == app.hwnd => {
            KillTimer(hwnd, 1);
            app.tray(NIM_DELETE);
            PostQuitMessage(0);
            0
        }
        WM_NCDESTROY => {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        _ => {
            drop(app);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }
}

pub fn run(
    data: Arc<Mutex<Snapshot>>,
    refresh: Arc<AtomicBool>,
    offset: i32,
    width: i32,
) -> Result<(), String> {
    unsafe {
        let singleton = CreateMutexW(null(), 0, wide("Local\\AgentBar-0.1").as_ptr());
        if singleton.is_null() {
            return Err("无法创建 AgentBar 实例".into());
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            CloseHandle(singleton);
            return Err("AgentBar 已在运行，请使用系统托盘图标打开详情。".into());
        }
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let instance = GetModuleHandleW(null());
        let class_name = wide("AgentBar.Window.0.1");
        let mut class: WNDCLASSEXW = zeroed();
        class.cbSize = size_of::<WNDCLASSEXW>() as u32;
        class.lpfnWndProc = Some(window_proc);
        class.hInstance = instance;
        class.lpszClassName = class_name.as_ptr();
        class.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        if RegisterClassExW(&class) == 0 {
            CloseHandle(singleton);
            return Err("无法注册 AgentBar 窗口".into());
        }
        let context = Box::new(RefCell::new(App {
            hwnd: null_mut(),
            detail: null_mut(),
            data,
            refresh,
            offset,
            width,
            scale: 1.0,
            taskbar: null_mut(),
            taskbar_rect: zeroed(),
            bar_rect: zeroed(),
            dark: true,
            taskbar_created: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
            visible: false,
            fonts: Vec::new(),
            mask_mode: Cell::new(false),
        }));
        // Creation/layout can synchronously dispatch messages before the HWND
        // fields are assigned. The default proc handles those setup messages.
        let pointer = &*context as *const RefCell<App>;
        let mut app = context.borrow_mut();
        app.hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_LAYERED,
            class_name.as_ptr(),
            wide("AgentBar").as_ptr(),
            WS_POPUP,
            0,
            0,
            width,
            44,
            null_mut(),
            null_mut(),
            instance,
            pointer as _,
        );
        if app.hwnd.is_null() {
            CloseHandle(singleton);
            return Err("无法创建 AgentBar 面板".into());
        }
        app.detail = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            class_name.as_ptr(),
            wide("AgentBar · Codex").as_ptr(),
            WS_POPUP | WS_BORDER,
            0,
            0,
            DETAIL_WIDTH,
            DETAIL_HEIGHT,
            app.hwnd,
            null_mut(),
            instance,
            pointer as _,
        );
        if app.detail.is_null() {
            DestroyWindow(app.hwnd);
            CloseHandle(singleton);
            return Err("无法创建详情窗口".into());
        }
        app.theme();
        app.position();
        app.tray(NIM_ADD);
        SetTimer(app.hwnd, 1, 1000, None);
        drop(app);
        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let mut app = context.borrow_mut();
        app.tray(NIM_DELETE);
        if IsWindow(app.hwnd) != 0 {
            KillTimer(app.hwnd, 1);
            DestroyWindow(app.hwnd);
        }
        if IsWindow(app.detail) != 0 {
            DestroyWindow(app.detail);
        }
        for font in app.fonts.drain(..) {
            DeleteObject(font);
        }
        UnregisterClassW(class_name.as_ptr(), instance);
        CloseHandle(singleton);
        Ok(())
    }
}

pub fn message(text: &str) {
    unsafe {
        MessageBoxW(
            null_mut(),
            wide(text).as_ptr(),
            wide("AgentBar").as_ptr(),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}
