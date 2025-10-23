use std::{num::NonZeroU32, rc::Rc};

use glutin::{
    config::{ConfigTemplateBuilder, GlConfig},
    context::PossiblyCurrentContext,
    context::{ContextAttributesBuilder, NotCurrentGlContext, PossiblyCurrentGlContext},
    display::{Display, DisplayApiPreference, GetGlDisplay, GlDisplay},
    surface::{GlSurface, Surface, SurfaceAttributesBuilder, SwapInterval, WindowSurface},
};

use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::{ElementState, KeyEvent, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy},
    keyboard::{Key, NamedKey},
    raw_window_handle::{HasDisplayHandle, HasWindowHandle},
    window::{Window, WindowId},
};

use interprocess::local_socket::{
    GenericFilePath,
    tokio::{Stream, prelude::*},
};

use serde::{Deserialize, Serialize};

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    runtime::Builder,
    sync::mpsc,
};

use libmpv2::{
    Mpv,
    render::{OpenGLInitParams, RenderContext, RenderParam, RenderParamApiType},
};

use std::ffi::{CString, c_void};

use ab_glyph::{point, Font, FontRef, Glyph};

type GlContext = Rc<Display>;

fn get_proc_address(display: &GlContext, name: &str) -> *mut c_void {
    let name = CString::new(name).unwrap();
    display.get_proc_address(name.as_c_str()) as *mut c_void
}

fn main() {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build().unwrap();
    let event_loop_proxy = event_loop.create_proxy();
    let elp = event_loop_proxy.clone();

    let name = "/tmp/wt_sock".to_fs_name::<GenericFilePath>().unwrap();

    let runtime = Builder::new_multi_thread()
        .worker_threads(4)
        .thread_name("tokio thread")
        .thread_stack_size(3 * 1024 * 1024)
        .enable_all()
        .build()
        .unwrap();

    let (tx, mut rx) = mpsc::channel::<String>(512);

    runtime.spawn(async move {
        let conn = Stream::connect(name).await.unwrap();
        let (receiver, mut sender) = conn.split();

        tokio::spawn(async move {
            let mut buffer = String::with_capacity(1024);
            let mut conn = BufReader::new(receiver);
            loop {
                match conn.read_line(&mut buffer).await {
                    Ok(0) => {
                        println!("Connection ended by nodejs server");
                        break;
                    }

                    Ok(_) => {
                        event_loop_proxy
                            .send_event(UserEvent::IpcMessage(
                                serde_json::from_str(&buffer.clone()).unwrap(),
                            ))
                            .unwrap();
                        buffer.clear();
                    }

                    Err(_) => {
                        println!("error occured while reading socket");
                    }
                };
            }
        });

        while let Some(res) = rx.recv().await {
            println!("info: {}", res);
            sender
                .write_all(format!("{}\n", res).as_bytes())
                .await
                .unwrap();
        }
    });

    let mut app = App::new(tx, elp);

    event_loop.run_app(&mut app).unwrap();
}

struct App {
    renderer: Option<Renderer>,
    gl_surface: Option<Surface<WindowSurface>>,
    gl_context: Option<PossiblyCurrentContext>,
    window: Option<Window>,
    sender: mpsc::Sender<String>,
    mpv_renderer: Option<RenderContext>,
    mpv: Option<Mpv>,
    elp: EventLoopProxy<UserEvent>,
}

impl App {
    fn new(sender: mpsc::Sender<String>, elp: EventLoopProxy<UserEvent>) -> Self {
        App {
            renderer: None,
            gl_surface: None,
            gl_context: None,
            window: None,
            sender,
            mpv_renderer: None,
            mpv: None,
            elp,
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
enum IpcData {
    Message(Message),
    Command(Command),
}

#[derive(Serialize, Deserialize, Debug)]
struct Message {
    username: String,
    content: String,
}

impl Message {
    fn new(username: &'static str, content: &'static str) -> Self {
        Self {
            username: username.to_string(),
            content: content.to_string(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
enum Command {
    Connect(String),
    Disconnect,
    Paused(u32),
    Resumed(u32),

    LoadVideo(String),
    Play,
    Pause,
    Seek(u32),
    Stop,
    GetTimestamp,
}

#[derive(Debug)]
enum UserEvent {
    IpcMessage(IpcData),
    MpvRedrawRequested,
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let template = ConfigTemplateBuilder::new()
            .with_alpha_size(8)
            .with_transparency(true)
            .build();

        let window_attributes = Window::default_attributes()
            .with_inner_size(LogicalSize::new(960, 540))
            .with_transparent(true)
            .with_title("Watch Together");

        let window = event_loop.create_window(window_attributes).unwrap();

        self.window = Some(window);

        let raw_display_handle = event_loop.display_handle().unwrap().as_raw();

        #[cfg(windows)]
        let gl_display = unsafe {
            Display::new(
                raw_display_handle,
                DisplayApiPreference::Wgl(Some(
                    self.window
                        .as_ref()
                        .unwrap()
                        .window_handle()
                        .unwrap()
                        .as_raw(),
                )),
            )
            .unwrap()
        };

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

        let size = self.window.as_ref().unwrap().inner_size();
        let width = size.width;
        let height = size.height;

        let raw_window_handle = self
            .window
            .as_ref()
            .unwrap()
            .window_handle()
            .unwrap()
            .as_raw();

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

        self.renderer
            .get_or_insert_with(|| Renderer::new(&gl_config.display()));

        self.mpv = Some(
            Mpv::with_initializer(|init| {
                init.set_property("vo", "libmpv")?;
                Ok(())
            })
            .unwrap(),
        );

        self.mpv_renderer = Some(
            RenderContext::new(
                unsafe { self.mpv.as_mut().unwrap().ctx.as_mut() },
                vec![
                    RenderParam::ApiType(RenderParamApiType::OpenGl),
                    RenderParam::InitParams(OpenGLInitParams::<GlContext> {
                        get_proc_address,
                        ctx: Rc::new(gl_display),
                    }),
                ],
            )
            .unwrap(),
        );

        let elp2 = self.elp.clone();
        self.mpv_renderer
            .as_mut()
            .unwrap()
            .set_update_callback(move || {
                elp2.send_event(UserEvent::MpvRedrawRequested).unwrap();
            });

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
                // let window = self.window.as_ref().unwrap();
                let gl_context = self.gl_context.as_ref().unwrap();
                let renderer = self.renderer.as_ref().unwrap();
                renderer.draw();
                // window.request_redraw();

                gl_surface.swap_buffers(gl_context).unwrap();
            }

            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key: Key::Named(NamedKey::Space),
                        state: ElementState::Released,
                        ..
                    },
                ..
            } => {
                let msg = Message::new("ryo", "chat msg");

                self.sender
                    .blocking_send(serde_json::to_string(&IpcData::Message(msg)).unwrap())
                    .unwrap();

                let path = "./video.mp4";
                self.mpv
                    .as_mut()
                    .unwrap()
                    .command("loadfile", &[&path, "replace"])
                    .unwrap();
            }

            _ => (),
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::IpcMessage(data) => match data {
                IpcData::Message(msg) => {
                    println!("{}: {}", msg.username, msg.content);
                }
                IpcData::Command(Command::Play) => {
                    println!("play command invoked");
                }

                _ => (),
            },
            UserEvent::MpvRedrawRequested => {
                let gl_surface = self.gl_surface.as_ref().unwrap();
                let window = self.window.as_ref().unwrap();
                let gl_context = self.gl_context.as_ref().unwrap();

                let size = window.inner_size();

                self.mpv_renderer
                    .as_mut()
                    .unwrap()
                    .render::<GlContext>(0, size.width as _, size.height as _, true)
                    .expect("Failed to draw on sdl2 window");

                let renderer = self.renderer.as_ref().unwrap();
                renderer.draw();

                gl_surface.swap_buffers(gl_context).unwrap();
            }
        }
    }
}

use std::ffi::CStr;
use std::ops::Deref;

pub mod gl {
    #![allow(clippy::all)]
    include!(concat!(env!("OUT_DIR"), "/gl_bindings.rs"));
}

// use gl::types::GLfloat;

pub struct Renderer {
    program: gl::types::GLuint,
    vao: gl::types::GLuint,
    vbo: gl::types::GLuint,
    gl: gl::Gl,
    texture: gl::types::GLuint,
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

            // gl.Enable(gl::CULL_FACE);
            gl.Enable(gl::BLEND);
            gl.BlendFunc(gl::SRC_ALPHA, gl::ONE_MINUS_SRC_ALPHA);

            gl.PixelStorei(gl::UNPACK_ALIGNMENT, 1);


            let font = FontRef::try_from_slice(include_bytes!("/usr/share/fonts/TTF/FiraCode-Regular.ttf")).unwrap();
            
            let q_glyph: Glyph = font
            .glyph_id('A')
            .with_scale_and_position(600.0, point(0.0, 0.0));
        
            let mut texture = std::mem::zeroed();
            
            font.outline_glyph(q_glyph).map(|outlined| {
                let px_bounds = outlined.px_bounds();
                let width = px_bounds.width().ceil() as u32;
                let height = px_bounds.height().ceil() as u32;

                let mut image = image::GrayImage::new(width, height);

                outlined.draw(|x, y, val| image.put_pixel(x, y, image::Luma([(val * 255.) as u8])));
                
                image.reverse();
                
                gl.GenTextures(1, &mut texture);
                gl.BindTexture(gl::TEXTURE_2D, texture);
                gl.TexImage2D(
                    gl::TEXTURE_2D,
                    0,
                    gl::RED as _,
                    width as _,
                    height as _,
                    0,
                    gl::RED,
                    gl::UNSIGNED_BYTE,
                    image.as_ptr() as *const _,
                );
            });

            gl.TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as _);
            gl.TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as _);
            gl.TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER,gl::LINEAR as _);
            gl.TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as _);
            


            let mut vao = std::mem::zeroed();
            gl.GenVertexArrays(1, &mut vao);
            gl.BindVertexArray(vao);

            let mut vbo = std::mem::zeroed();
            gl.GenBuffers(1, &mut vbo);
            gl.BindBuffer(gl::ARRAY_BUFFER, vbo);
            gl.BufferData(
                gl::ARRAY_BUFFER,
                (24 * std::mem::size_of::<f32>()) as gl::types::GLsizeiptr,
                // (VERTEX_DATA.len() * std::mem::size_of::<f32>()) as gl::types::GLsizeiptr,
                std::ptr::null(),
                // VERTEX_DATA.as_ptr() as *const _,
                gl::DYNAMIC_DRAW,
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
                4,
                gl::FLOAT,
                0,
                4 * std::mem::size_of::<f32>() as gl::types::GLsizei,
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
                texture,
            }
        }
    }

    pub fn draw(&self) {
        // self.draw_with_clear_color(0.0, 0.0, 0.0, 0.0)
        self.draw_with_clear_color()
    }

    pub fn draw_with_clear_color(
        &self,
        // red: GLfloat,
        // green: GLfloat,
        // blue: GLfloat,
        // alpha: GLfloat,
    ) {
        unsafe {
            self.gl.UseProgram(self.program);
            self.gl.Enable(gl::BLEND);
            self.gl.BlendFunc(gl::SRC_ALPHA, gl::ONE_MINUS_SRC_ALPHA);


            self.gl.BindVertexArray(self.vao);
            self.gl.BindBuffer(gl::ARRAY_BUFFER, self.vbo);

            // self.gl.ClearColor(red, green, blue, alpha);
            // self.gl.Clear(gl::COLOR_BUFFER_BIT);

            self.gl.ActiveTexture(gl::TEXTURE0);

            let vertices: [f32; 24] = [
                0.0 - 0.5 + 0.2, 0.0 - 0.5, 0.0, 0.0,
                0.0 - 0.5 + 0.2, 1.0 - 0.5, 0.0, 1.0,
                1.0 - 0.5 - 0.2, 1.0 - 0.5, 1.0, 1.0,
                0.0 - 0.5 + 0.2, 0.0 - 0.5, 0.0, 0.0,
                1.0 - 0.5 - 0.2, 1.0 - 0.5, 1.0, 1.0,
                1.0 - 0.5 - 0.2, 0.0 - 0.5, 1.0, 0.0,
            ];

            self.gl.BindTexture(gl::TEXTURE_2D, self.texture);

            self.gl.BufferSubData(gl::ARRAY_BUFFER, 0, 24 * std::mem::size_of::<f32>() as gl::types::GLsizeiptr, vertices.as_ptr() as *const _);

            self.gl.DrawArrays(gl::TRIANGLES, 0, 6);
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

// #[rustfmt::skip]
// static VERTEX_DATA: [f32; 12] = [
//     -0.5, -0.5,
//     -0.5,  0.0,
//      0.0, -0.5,

//     -0.5,  0.0,
//      0.0, -0.5,
//      0.0,  0.0,

// ];

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
layout (location = 0) in vec4 vertex;
out vec2 TexCoords;

void main()
{
    gl_Position = vec4(vertex.xy, 0.0, 1.0);
    TexCoords = vertex.zw;
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
in vec2 TexCoords;
out vec4 color;

uniform sampler2D text;

void main()
{    
    color = vec4(1.0, 1.0, 1.0, texture(text, TexCoords).r);
    // color = vec4(1.0, 1.0, 1.0, 1.0);
}  
\0";
