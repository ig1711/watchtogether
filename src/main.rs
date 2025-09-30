use glutin::{
    config::{ConfigTemplateBuilder, GlConfig},
    context::PossiblyCurrentContext,
    context::{ContextAttributesBuilder, NotCurrentGlContext, PossiblyCurrentGlContext},
    display::{Display, DisplayApiPreference, GetGlDisplay, GlDisplay},
    surface::{GlSurface, Surface, SurfaceAttributesBuilder, SwapInterval, WindowSurface},
};

use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    raw_window_handle::{HasDisplayHandle, HasWindowHandle},
    window::{Window, WindowId},
};

use std::num::NonZeroU32;

fn main() {
    let event_loop = EventLoop::new().unwrap();

    let mut app = App::new();

    event_loop.run_app(&mut app).unwrap();
}

struct App {
    renderer: Option<Renderer>,
    gl_surface: Option<Surface<WindowSurface>>,
    gl_context: Option<PossiblyCurrentContext>,
    window: Option<Window>,
}

impl App {
    fn new() -> Self {
        App {
            renderer: None,
            gl_surface: None,
            gl_context: None,
            window: None,
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let template = ConfigTemplateBuilder::new()
            .with_alpha_size(8)
            .with_transparency(true)
            .build();

        let window_attributes = Window::default_attributes()
            .with_transparent(true)
            .with_title("Watch Together");

        let raw_display_handle = event_loop.display_handle().unwrap().as_raw();
        
        #[cfg(windows)]
        let gl_display =
            unsafe { Display::new(raw_display_handle, DisplayApiPreference::Wgl(self.window.as_ref().unwrap().window_handle().unwrap().as_raw())).unwrap() };

        #[cfg(unix)]
        let gl_display =
            unsafe { Display::new(raw_display_handle, DisplayApiPreference::Egl).unwrap() };


        let configs = unsafe { gl_display.find_configs(template).unwrap() };

        let gl_config = configs
            .reduce(|accum, config| {
                let transparency_check = config.supports_transparency().unwrap_or(false)
                    & !accum.supports_transparency().unwrap_or(false);

                if transparency_check || config.num_samples() > accum.num_samples() {
                    config
                } else {
                    accum
                }
            })
            .unwrap();

        let window = event_loop.create_window(window_attributes).unwrap();

        let size = window.inner_size();
        let width = size.width;
        let height = size.height;

        let raw_window_handle = window.window_handle().unwrap().as_raw();

        let surface_attributes = SurfaceAttributesBuilder::<WindowSurface>::new().build(
            raw_window_handle,
            NonZeroU32::new(width).unwrap(),
            NonZeroU32::new(height).unwrap(),
        );

        let gl_surface = unsafe {
            gl_config
                .display()
                .create_window_surface(&gl_config, &surface_attributes)
                .unwrap()
        };

        let context_attributes = ContextAttributesBuilder::new().build(Some(raw_window_handle));

        // shadowed
        let gl_display = gl_config.display();

        let possibly_current_context = unsafe {
            gl_display
                .create_context(&gl_config, &context_attributes)
                .unwrap()
                .treat_as_possibly_current()
        };

        possibly_current_context.make_current(&gl_surface).unwrap();

        self.gl_context = Some(possibly_current_context);
        self.gl_surface = Some(gl_surface);

        self.window = Some(window);

        self.renderer
            .get_or_insert_with(|| Renderer::new(&gl_config.display()));

        let gl_context = self.gl_context.as_ref().unwrap();
        let gl_surface = self.gl_surface.as_ref().unwrap();

        // Try setting vsync.
        gl_surface
            .set_swap_interval(&gl_context, SwapInterval::Wait(NonZeroU32::new(1).unwrap()))
            .unwrap();
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                println!("The close button was pressed; stopping");
                event_loop.exit();
            }
            WindowEvent::Resized(size) if size.width != 0 && size.height != 0 => {
                let gl_context = self.gl_context.as_ref().unwrap();
                let gl_surface = self.gl_surface.as_ref().unwrap();
                gl_surface.resize(
                    gl_context,
                    NonZeroU32::new(size.width).unwrap(),
                    NonZeroU32::new(size.height).unwrap(),
                );

                let renderer = self.renderer.as_ref().unwrap();
                renderer.resize(size.width as i32, size.height as i32);
            }
            WindowEvent::RedrawRequested => {
                let gl_surface = self.gl_surface.as_ref().unwrap();
                let window = self.window.as_ref().unwrap();
                let gl_context = self.gl_context.as_ref().unwrap();
                let renderer = self.renderer.as_ref().unwrap();
                renderer.draw();
                window.request_redraw();

                gl_surface.swap_buffers(gl_context).unwrap();
            }

            _ => (),
        }
    }
}

use std::ffi::{CStr, CString};
use std::ops::Deref;

pub mod gl {
    #![allow(clippy::all)]
    include!(concat!(env!("OUT_DIR"), "/gl_bindings.rs"));
}

use gl::types::GLfloat;

pub struct Renderer {
    program: gl::types::GLuint,
    vao: gl::types::GLuint,
    vbo: gl::types::GLuint,
    gl: gl::Gl,
}

impl Renderer {
    pub fn new<D: GlDisplay>(gl_display: &D) -> Self {
        unsafe {
            let gl = gl::Gl::load_with(|symbol| {
                let symbol = CString::new(symbol).unwrap();
                gl_display.get_proc_address(symbol.as_c_str()).cast()
            });

            if let Some(renderer) = get_gl_string(&gl, gl::RENDERER) {
                println!("Running on {}", renderer.to_string_lossy());
            }
            if let Some(version) = get_gl_string(&gl, gl::VERSION) {
                println!("OpenGL Version {}", version.to_string_lossy());
            }

            if let Some(shaders_version) = get_gl_string(&gl, gl::SHADING_LANGUAGE_VERSION) {
                println!("Shaders version on {}", shaders_version.to_string_lossy());
            }

            let vertex_shader = create_shader(&gl, gl::VERTEX_SHADER, VERTEX_SHADER_SOURCE);
            let fragment_shader = create_shader(&gl, gl::FRAGMENT_SHADER, FRAGMENT_SHADER_SOURCE);

            let program = gl.CreateProgram();

            gl.AttachShader(program, vertex_shader);
            gl.AttachShader(program, fragment_shader);

            gl.LinkProgram(program);

            gl.UseProgram(program);

            gl.DeleteShader(vertex_shader);
            gl.DeleteShader(fragment_shader);

            let mut vao = std::mem::zeroed();
            gl.GenVertexArrays(1, &mut vao);
            gl.BindVertexArray(vao);

            let mut vbo = std::mem::zeroed();
            gl.GenBuffers(1, &mut vbo);
            gl.BindBuffer(gl::ARRAY_BUFFER, vbo);
            gl.BufferData(
                gl::ARRAY_BUFFER,
                (VERTEX_DATA.len() * std::mem::size_of::<f32>()) as gl::types::GLsizeiptr,
                VERTEX_DATA.as_ptr() as *const _,
                gl::STATIC_DRAW,
            );

            // let pos_attrib = gl.GetAttribLocation(program, b"position\0".as_ptr() as *const _);
            // let color_attrib = gl.GetAttribLocation(program, b"color\0".as_ptr() as *const _);
            // gl.VertexAttribPointer(
            //     pos_attrib as gl::types::GLuint,
            //     2,
            //     gl::FLOAT,
            //     0,
            //     5 * std::mem::size_of::<f32>() as gl::types::GLsizei,
            //     std::ptr::null(),
            // );

            gl.VertexAttribPointer(
                0 as gl::types::GLuint,
                3,
                gl::FLOAT,
                0,
                3 * std::mem::size_of::<f32>() as gl::types::GLsizei,
                std::ptr::null(),
            );

            // gl.VertexAttribPointer(
            //     color_attrib as gl::types::GLuint,
            //     3,
            //     gl::FLOAT,
            //     0,
            //     5 * std::mem::size_of::<f32>() as gl::types::GLsizei,
            //     (2 * std::mem::size_of::<f32>()) as *const () as *const _,
            // );
            // gl.EnableVertexAttribArray(pos_attrib as gl::types::GLuint);
            gl.EnableVertexAttribArray(0 as gl::types::GLuint);
            // gl.EnableVertexAttribArray(color_attrib as gl::types::GLuint);

            Self {
                program,
                vao,
                vbo,
                gl,
            }
        }
    }

    pub fn draw(&self) {
        self.draw_with_clear_color(0.1, 0.1, 0.1, 0.9)
    }

    pub fn draw_with_clear_color(
        &self,
        red: GLfloat,
        green: GLfloat,
        blue: GLfloat,
        alpha: GLfloat,
    ) {
        unsafe {
            self.gl.UseProgram(self.program);

            self.gl.BindVertexArray(self.vao);
            self.gl.BindBuffer(gl::ARRAY_BUFFER, self.vbo);

            self.gl.ClearColor(red, green, blue, alpha);
            self.gl.Clear(gl::COLOR_BUFFER_BIT);
            self.gl.DrawArrays(gl::TRIANGLES, 0, 3);
        }
    }

    pub fn resize(&self, width: i32, height: i32) {
        unsafe {
            self.gl.Viewport(0, 0, width, height);
        }
    }
}

impl Deref for Renderer {
    type Target = gl::Gl;

    fn deref(&self) -> &Self::Target {
        &self.gl
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            self.gl.DeleteProgram(self.program);
            self.gl.DeleteBuffers(1, &self.vbo);
            self.gl.DeleteVertexArrays(1, &self.vao);
        }
    }
}

unsafe fn create_shader(
    gl: &gl::Gl,
    shader: gl::types::GLenum,
    source: &[u8],
) -> gl::types::GLuint {
    let shader = unsafe { gl.CreateShader(shader) };
    unsafe {
        gl.ShaderSource(
            shader,
            1,
            [source.as_ptr().cast()].as_ptr(),
            std::ptr::null(),
        );
        gl.CompileShader(shader);
    }
    shader
}

fn get_gl_string(gl: &gl::Gl, variant: gl::types::GLenum) -> Option<&'static CStr> {
    unsafe {
        let s = gl.GetString(variant);
        (!s.is_null()).then(|| CStr::from_ptr(s.cast()))
    }
}

// // #[rustfmt::skip]
// // static VERTEX_DATA: [f32; 15] = [
// //     -0.5, -0.5,  1.0,  0.0,  0.0,
// //      0.0,  0.5,  0.0,  1.0,  0.0,
// //      0.5, -0.5,  0.0,  0.0,  1.0,
// // ];

#[rustfmt::skip]
static VERTEX_DATA: [f32; 9] = [
    -0.5, -0.5, 0.0,
     0.5, -0.5, 0.0,
     0.0,  0.5, 0.0
];

// // const VERTEX_SHADER_SOURCE: &[u8] = b"
// // #version 100
// // precision mediump float;

// // attribute vec2 position;
// // attribute vec3 color;

// // varying vec3 v_color;

// // void main() {
// //     gl_Position = vec4(position, 0.0, 1.0);
// //     v_color = color;
// // }
// // \0";

const VERTEX_SHADER_SOURCE: &[u8] = b"
#version 460 core
layout (location = 0) in vec3 aPos;

void main()
{
    gl_Position = vec4(aPos.x, aPos.y, aPos.z, 1.0);
}
\0";

// // const FRAGMENT_SHADER_SOURCE: &[u8] = b"
// // #version 100
// // precision mediump float;

// // varying vec3 v_color;

// // void main() {
// //     gl_FragColor = vec4(v_color, 1.0);
// // }
// // \0";

const FRAGMENT_SHADER_SOURCE: &[u8] = b"
#version 460 core
out vec4 FragColor;

void main()
{
    FragColor = vec4(1.0f, 1.0f, 0.2f, 1.0f);
}
\0";
