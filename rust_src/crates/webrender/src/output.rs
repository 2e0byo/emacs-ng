use std::{cell::RefCell, mem::MaybeUninit, rc::Rc, sync::Arc, num::NonZeroU32, ffi::CString};

use gleam::gl::{self, Gl};
use winit::{
    self,
    dpi::{PhysicalSize, LogicalSize},
    window::{CursorIcon, Window, WindowBuilder},
};

use glutin::{surface::{Surface, WindowSurface, SurfaceAttributesBuilder}, context::PossiblyCurrentContext};
use glutin::prelude::GlSurface;
use glutin::display::{GlDisplay, GetGlDisplay};
use glutin::config::{Config, GlConfig, ColorBufferType};
use glutin::context::{ContextApi, ContextAttributesBuilder, Version, NotCurrentGlContextSurfaceAccessor, PossiblyCurrentContextGlSurfaceAccessor};
use glutin::config::{Api, ConfigTemplateBuilder};
use glutin_winit::DisplayBuilder;
use std::{
    ops::{Deref, DerefMut},
    ptr,
};

use webrender::{self, api::units::*, api::*, RenderApi, Renderer, Transaction, create_webrender_instance};

use emacs::{
    bindings::{wr_output, Emacs_Cursor},
    frame::LispFrameRef,
};

use crate::event_loop::WrEventLoop;

use super::texture::TextureResourceManager;
use super::util::HandyDandyRectBuilder;
use super::{cursor::emacs_to_winit_cursor, display_info::DisplayInfoRef};
use super::{cursor::winit_to_emacs_cursor, font::FontRef};

#[cfg(all(feature = "wayland", not(any(target_os = "macos", windows))))]
use emacs::{bindings::globals, multibyte::LispStringRef};

pub struct Output {
    // Extend `wr_output` struct defined in `wrterm.h`
    pub output: wr_output,

    pub font: FontRef,
    pub fontset: i32,

    pub render_api: RenderApi,
    pub document_id: DocumentId,

    display_list_builder: Option<DisplayListBuilder>,
    previous_frame_image: Option<ImageKey>,

    pub background_color: ColorF,
    pub cursor_color: ColorF,
    pub cursor_foreground_color: ColorF,

    color_bits: u8,

    // The drop order is important here.

    // Need to dropped before window context
    texture_resources: Rc<RefCell<TextureResourceManager>>,

    // Need to droppend before window context
    renderer: Renderer,

    window_context: PossiblyCurrentContext,
    window: Window,
    gl_config: Config,
    surface: Surface<WindowSurface>,

    frame: LispFrameRef,
}

impl Output {
    pub fn build(event_loop: &mut WrEventLoop, frame: LispFrameRef) -> Self {
        // -- in glutin originally --
        let window_builder = WindowBuilder::new()
            .with_visible(true)
            .with_maximized(true)
            .with_inner_size(LogicalSize::new(1920.0,1080.0));

        #[cfg(all(feature = "wayland", not(any(target_os = "macos", windows))))]
        let window_builder = {
            let invocation_name: LispStringRef = unsafe { globals.Vinvocation_name.into() };
            let invocation_name = invocation_name.to_utf8();
            window_builder.with_title(invocation_name)
        };

        let template = ConfigTemplateBuilder::new(); // TODO do we need to do anything to this?

        let display_builder = DisplayBuilder::new().with_window_builder(Some(window_builder));

        let (window, gl_config) = event_loop
            .build_window(template, display_builder
            );

        // from example
        use raw_window_handle::HasRawWindowHandle;
        let raw_window_handle = window.as_ref().map(|window| window.raw_window_handle());
        let gl_display = gl_config.display();

        let context_attributes = ContextAttributesBuilder::new().build(raw_window_handle);

        // Since glutin by default tries to create OpenGL core context, which may not be
        // present we should try gles.
        let fallback_context_attributes = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::Gles(None))
            .build(raw_window_handle);

        // There are also some old devices that support neither modern OpenGL nor GLES.
        // To support these we can try and create a 2.1 context.
        let legacy_context_attributes = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::OpenGl(Some(Version::new(2, 1))))
            .build(raw_window_handle);

        let mut not_current_gl_context = Some(unsafe {
            gl_display.create_context(&gl_config, &context_attributes).unwrap_or_else(|_| {
                gl_display.create_context(&gl_config, &fallback_context_attributes).unwrap_or_else(
                    |_| {
                        gl_display
                            .create_context(&gl_config, &legacy_context_attributes)
                            .expect("failed to create context")
                    },
                )
            })
        });

        // I strongly suspect this all belongs on the event loop.  Are we in the same thread?
        let window = window.unwrap();
        let raw_window_handle = window.raw_window_handle();
        let (width, height): (u32, u32) = window.inner_size().into();
        let attrs = SurfaceAttributesBuilder::<WindowSurface>::new().build(
            raw_window_handle,
            NonZeroU32::new(width).unwrap(),
            NonZeroU32::new(height).unwrap(),
        );
        let gl_surface = unsafe { gl_display.create_window_surface(&gl_config, &attrs).unwrap()};

        // Make it current.
        let gl_context =
            not_current_gl_context.take().unwrap().make_current(&gl_surface).unwrap();

        let gl = Self::get_gl_api(&gl_config);

        // -- into webrender --
        let webrender_opts = webrender::WebRenderOptions {
            // NOTE at one point we unset clear_color here, but that's no longer possible (not optional)
            ..webrender::WebRenderOptions::default()
        };

        let notifier = Box::new(Notifier::new());

        let (mut renderer, sender) =
            create_webrender_instance(gl.clone(), notifier, webrender_opts, None).unwrap();

        let color_buffer = gl_config.color_buffer_type().unwrap();
        let color_bits = match color_buffer {
            ColorBufferType::Rgb { r_size, g_size, b_size }=> r_size + g_size + b_size,
            ColorBufferType::Luminance(_) => unimplemented!(),
        };


        let texture_resources = Rc::new(RefCell::new(TextureResourceManager::new(
            gl.clone(),
            sender.create_api(),
        )));

        let external_image_handler = texture_resources.borrow_mut().new_external_image_handler();

        renderer.set_external_image_handler(external_image_handler);

        let pipeline_id = PipelineId(0, 0);
        let mut txn = Transaction::new();
        txn.set_root_pipeline(pipeline_id);

        let device_size = {
            DeviceIntSize::new(width as i32, height as i32)
        };

        let mut api = sender.create_api();

        let document_id = api.add_document(device_size);
        api.send_transaction(document_id, txn);

        let mut output = Self {
            output: wr_output::default(),
            font: FontRef::new(ptr::null_mut()),
            fontset: 0,
            render_api: api,
            document_id,
            display_list_builder: None,
            previous_frame_image: None,
            background_color: ColorF::WHITE,
            cursor_color: ColorF::BLACK,
            cursor_foreground_color: ColorF::WHITE,
            color_bits,
            renderer,
            window: window,
            window_context: gl_context,
            gl_config,
            surface: gl_surface,
            texture_resources,
            frame,
        };

        Self::build_mouse_cursors(&mut output);

        output
    }

    fn copy_framebuffer_to_texture(&self, device_rect: DeviceIntRect) -> ImageKey {
        let mut origin = device_rect.min;

        let device_size = self.get_device_size();

        if !self.renderer.device.surface_origin_is_top_left() {
            origin.y = device_size.height - origin.y - device_rect.height();
        }

        let fb_rect = FramebufferIntRect::from_origin_and_size(
            FramebufferIntPoint::from_untyped(origin.to_untyped()),
            FramebufferIntSize::from_untyped(device_rect.size().to_untyped()),
        );

        let need_flip = !self.renderer.device.surface_origin_is_top_left();

        let (image_key, texture_id) = self.texture_resources.borrow_mut().new_image(
            self.document_id,
            fb_rect.size(),
            need_flip,
        );

        let gl = Self::get_gl_api(&self.gl_config);
        gl.bind_texture(gl::TEXTURE_2D, texture_id);

        gl.copy_tex_sub_image_2d(
            gl::TEXTURE_2D,
            0,
            0,
            0,
            fb_rect.min.x,
            fb_rect.min.y,
            fb_rect.size().width,
            fb_rect.size().height,
        );

        gl.bind_texture(gl::TEXTURE_2D, 0);

        image_key
    }

    fn get_gl_api(gl_config: &Config) -> Rc<dyn Gl> {
        let flags = gl_config.api();
        if flags.contains(Api::OPENGL) {
            unsafe {gl::GlFns::load_with(|symbol| gl_config.display().get_proc_address(&CString::new(symbol).unwrap()) as *const _)}
        } else if flags.intersects(Api::GLES1 | Api::GLES2 | Api::GLES3 ) {
            unsafe {gl::GlesFns::load_with(|symbol| gl_config.display().get_proc_address(&CString::new(symbol).unwrap()) as *const _)}
        } else {
            unimplemented!();
        }
    }

    fn get_size(&self) -> LayoutSize {
        let dims = self.get_device_size().to_f32();
        LayoutSize::new(dims.width, dims.height)
    }

    fn new_builder(&mut self, image: Option<(ImageKey, LayoutRect)>) -> DisplayListBuilder {
        let pipeline_id = PipelineId(0, 0);

        let layout_size = self.get_size();
        let mut builder = DisplayListBuilder::new(pipeline_id);
        builder.begin();

        if let Some((image_key, image_rect)) = image {
            let space_and_clip = SpaceAndClipInfo::root_scroll(pipeline_id);

            let bounds = (0, 0).by(layout_size.width as i32, layout_size.height as i32);

            builder.push_image(
                &CommonItemProperties::new(bounds, space_and_clip),
                image_rect,
                ImageRendering::Auto,
                AlphaType::PremultipliedAlpha,
                image_key,
                ColorF::WHITE,
            );
        }

        builder
    }

    pub fn show_window(&self) {
        self.get_window().set_visible(true);
    }
    pub fn hide_window(&self) {
        self.get_window().set_visible(false);
    }

    pub fn maximize(&self) {
        self.get_window().set_maximized(true);
    }

    pub fn set_title(&self, title: &str) {
        self.get_window().set_title(title);
    }

    pub fn set_display_info(&mut self, mut dpyinfo: DisplayInfoRef) {
        self.output.display_info = dpyinfo.get_raw().as_mut();
    }

    pub fn get_frame(&self) -> LispFrameRef {
        self.frame
    }

    pub fn display_info(&self) -> DisplayInfoRef {
        DisplayInfoRef::new(self.output.display_info as *mut _)
    }

    pub fn get_inner_size(&self) -> PhysicalSize<u32> {
        self.get_window().inner_size()
    }

    pub fn device_pixel_ratio(&self) -> f32 {
        self.get_window().scale_factor() as f32
    }

    fn get_device_size(&self) -> DeviceIntSize {
        let size = self.get_window().inner_size();
        DeviceIntSize::new(size.width as i32, size.height as i32)
    }

    pub fn display<F>(&mut self, f: F)
    where
        F: Fn(&mut DisplayListBuilder, SpaceAndClipInfo),
    {
        if self.display_list_builder.is_none() {
            let layout_size = self.get_size();

            let image_and_pos = self
                .previous_frame_image
                .map(|image_key| (image_key, LayoutRect::from_size(layout_size)));

            self.display_list_builder = Some(self.new_builder(image_and_pos));
        }

        let pipeline_id = PipelineId(0, 0);

        if let Some(builder) = &mut self.display_list_builder {
            let space_and_clip = SpaceAndClipInfo::root_scroll(pipeline_id);

            f(builder, space_and_clip);
        }
    }

    fn ensure_context_is_current(&mut self) {
        self.window_context.make_current(&self.surface).unwrap();
    }

    pub fn flush(&mut self) {
        let builder = std::mem::replace(&mut self.display_list_builder, None);

        if let Some(mut builder) = builder {
            let layout_size = self.get_size();

            let epoch = Epoch(0);
            let mut txn = Transaction::new();

            txn.set_display_list(epoch, None, layout_size, builder.end());

            txn.generate_frame(0, RenderReasons::empty());

            self.render_api.send_transaction(self.document_id, txn);

            self.render_api.flush_scene_builder();

            let device_size = self.get_device_size();

            self.renderer.update();

            self.ensure_context_is_current();

            self.renderer.render(device_size, 0).unwrap();
            let _ = self.renderer.flush_pipeline_info();

            self.surface.swap_buffers(&self.window_context).ok();

            self.texture_resources.borrow_mut().clear();

            let image_key = self.copy_framebuffer_to_texture(DeviceIntRect::from_size(device_size));
            self.previous_frame_image = Some(image_key);
        }
    }

    pub fn get_previous_frame(&self) -> Option<ImageKey> {
        self.previous_frame_image
    }

    pub fn clear_display_list_builder(&mut self) {
        let _ = std::mem::replace(&mut self.display_list_builder, None);
    }

    pub fn add_font_instance(&mut self, font_key: FontKey, pixel_size: i32) -> FontInstanceKey {
        let mut txn = Transaction::new();

        let font_instance_key = self.render_api.generate_font_instance_key();

        txn.add_font_instance(
            font_instance_key,
            font_key,
            pixel_size as f32,
            None,
            None,
            vec![],
        );

        self.render_api.send_transaction(self.document_id, txn);
        font_instance_key
    }

    pub fn add_font(&mut self, font_bytes: Rc<Vec<u8>>, font_index: u32) -> FontKey {
        let mut txn = Transaction::new();

        let font_key = self.render_api.generate_font_key();

        txn.add_raw_font(font_key, font_bytes.to_vec(), font_index);

        self.render_api.send_transaction(self.document_id, txn);

        font_key
    }

    pub fn get_color_bits(&self) -> u8 {
        self.color_bits
    }

    pub fn get_window(&self) -> &Window {
        &self.window
    }

    fn build_mouse_cursors(output: &mut Output) {
        output.output.text_cursor = winit_to_emacs_cursor(CursorIcon::Text);
        output.output.nontext_cursor = winit_to_emacs_cursor(CursorIcon::Arrow);
        output.output.modeline_cursor = winit_to_emacs_cursor(CursorIcon::Hand);
        output.output.hand_cursor = winit_to_emacs_cursor(CursorIcon::Hand);
        output.output.hourglass_cursor = winit_to_emacs_cursor(CursorIcon::Progress);

        output.output.horizontal_drag_cursor = winit_to_emacs_cursor(CursorIcon::ColResize);
        output.output.vertical_drag_cursor = winit_to_emacs_cursor(CursorIcon::RowResize);

        output.output.left_edge_cursor = winit_to_emacs_cursor(CursorIcon::WResize);
        output.output.right_edge_cursor = winit_to_emacs_cursor(CursorIcon::EResize);
        output.output.top_edge_cursor = winit_to_emacs_cursor(CursorIcon::NResize);
        output.output.bottom_edge_cursor = winit_to_emacs_cursor(CursorIcon::SResize);

        output.output.top_left_corner_cursor = winit_to_emacs_cursor(CursorIcon::NwResize);
        output.output.top_right_corner_cursor = winit_to_emacs_cursor(CursorIcon::NeResize);

        output.output.bottom_left_corner_cursor = winit_to_emacs_cursor(CursorIcon::SwResize);
        output.output.bottom_right_corner_cursor = winit_to_emacs_cursor(CursorIcon::SeResize);
    }

    pub fn set_mouse_cursor(&self, cursor: Emacs_Cursor) {
        let cursor = emacs_to_winit_cursor(cursor);

        self.get_window().set_cursor_icon(cursor)
    }

    pub fn add_image(&mut self, width: i32, height: i32, image_data: Arc<Vec<u8>>) -> ImageKey {
        let image_key = self.render_api.generate_image_key();

        self.update_image(image_key, width, height, image_data);

        image_key
    }

    pub fn update_image(
        &mut self,
        image_key: ImageKey,
        width: i32,
        height: i32,
        image_data: Arc<Vec<u8>>,
    ) {
        let mut txn = Transaction::new();

        txn.add_image(
            image_key,
            ImageDescriptor::new(
                width,
                height,
                ImageFormat::RGBA8,
                ImageDescriptorFlags::empty(),
            ),
            ImageData::Raw(image_data),
            None,
        );

        self.render_api.send_transaction(self.document_id, txn);
    }

    pub fn delete_image(&mut self, image_key: ImageKey) {
        let mut txn = Transaction::new();

        txn.delete_image(image_key);

        self.render_api.send_transaction(self.document_id, txn);
    }

    pub fn resize(&mut self, size: &PhysicalSize<u32>) {
        let device_size = DeviceIntSize::new(size.width as i32, size.height as i32);

        let device_rect =
            DeviceIntRect::from_origin_and_size(DeviceIntPoint::new(0, 0), device_size);

        let mut txn = Transaction::new();
        txn.set_document_view(device_rect);
        self.render_api.send_transaction(self.document_id, txn);
        self.surface.resize(&self.window_context, NonZeroU32::new(size.width).unwrap(), NonZeroU32::new(size.height).unwrap());
    }
}

#[derive(PartialEq)]
#[repr(transparent)]
pub struct OutputRef(*mut Output);

impl Copy for OutputRef {}

// Derive fails for this type so do it manually
impl Clone for OutputRef {
    fn clone(&self) -> Self {
        Self(self.0)
    }
}

impl OutputRef {
    pub const fn new(p: *mut Output) -> Self {
        Self(p)
    }

    pub fn as_mut(&mut self) -> *mut wr_output {
        self.0 as *mut wr_output
    }

    pub fn as_rust_ptr(&mut self) -> *mut Output {
        self.0 as *mut Output
    }
}

impl Deref for OutputRef {
    type Target = Output;
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.0 }
    }
}

impl DerefMut for OutputRef {
    fn deref_mut(&mut self) -> &mut Output {
        unsafe { &mut *self.0 }
    }
}

impl From<*mut wr_output> for OutputRef {
    fn from(o: *mut wr_output) -> Self {
        Self::new(o as *mut Output)
    }
}

struct Notifier;

impl Notifier {
    fn new() -> Notifier {
        Notifier
    }
}

impl RenderNotifier for Notifier {
    fn clone(&self) -> Box<dyn RenderNotifier> {
        Box::new(Notifier)
    }

    fn wake_up(&self, _composite_needed: bool) {}

    fn new_frame_ready(
        &self,
        _: DocumentId,
        _scrolled: bool,
        _composite_needed: bool,
    ) {
    }
}
