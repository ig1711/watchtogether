use std::{num::NonZeroU32, rc::Rc};

use egui::{Color32, Frame};
use glutin::{
    config::{ConfigTemplateBuilder, GlConfig},
    context::PossiblyCurrentContext,
    context::{ContextAttributesBuilder, NotCurrentGlContext, PossiblyCurrentGlContext},
    display::{Display, DisplayApiPreference, GetGlDisplay, GlDisplay},
    surface::{GlSurface, Surface, SurfaceAttributesBuilder, SwapInterval, WindowSurface},
};

use egui_winit::winit;

use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy},
    raw_window_handle::{HasDisplayHandle, HasWindowHandle},
    window::{Window, WindowId},
};

use interprocess::local_socket::{
    GenericFilePath, GenericNamespaced,
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

type GlContext = Rc<Display>;

fn get_proc_address(display: &GlContext, name: &str) -> *mut c_void {
    let name = CString::new(name).unwrap();
    display.get_proc_address(name.as_c_str()) as *mut c_void
}

fn main() {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build().unwrap();
    let event_loop_proxy = event_loop.create_proxy();
    let elp = event_loop_proxy.clone();

    let name = "wt_sock".to_ns_name::<GenericNamespaced>().unwrap();

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
    egui_glow: Option<egui_glow::EguiGlow>,
    gl_surface: Option<Surface<WindowSurface>>,
    gl_context: Option<PossiblyCurrentContext>,
    window: Option<Window>,
    sender: mpsc::Sender<String>,
    mpv_renderer: Option<RenderContext>,
    mpv: Option<Mpv>,
    elp: EventLoopProxy<UserEvent>,
    text: String,
}

impl App {
    fn new(sender: mpsc::Sender<String>, elp: EventLoopProxy<UserEvent>) -> Self {
        App {
            egui_glow: None,
            gl_surface: None,
            gl_context: None,
            window: None,
            sender,
            mpv_renderer: None,
            mpv: None,
            elp,
            text: "edit text".to_owned(),
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
    EguiRedraw,
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

        let gl = unsafe {
            glow::Context::from_loader_function(|s| {
                let s = std::ffi::CString::new(s)
                    .expect("failed to construct C string from string for gl proc address");

                gl_display.get_proc_address(&s)
            })
        };

        let gl = std::sync::Arc::new(gl);

        let egui_glow = egui_glow::EguiGlow::new(event_loop, gl.clone(), None, None, true);

        let elp3 = self.elp.clone();
        egui_glow
            .egui_ctx
            .set_request_repaint_callback(move |_info| {
                elp3.clone()
                    .send_event(UserEvent::EguiRedraw)
                    .expect("Cannot send event");
            });

        self.egui_glow = Some(egui_glow);

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
            }
            WindowEvent::RedrawRequested => {
                let Self { text, .. } = self;
                self.egui_glow
                    .as_mut()
                    .unwrap()
                    .run(self.window.as_mut().unwrap(), |egui_ctx| {
                        egui::SidePanel::left("my_side_panel")
                            .frame(Frame::new().fill(Color32::from_rgba_unmultiplied(0, 0, 0, 0)))
                            .show(egui_ctx, |ui| {
                                ui.heading("Hello World!");
                                ui.heading("text");
                                egui::TextEdit::multiline(text)
                                    .hint_text("Type something!")
                                    .show(ui);

                                if ui.button("Send to nodejs").clicked() {
                                    let msg = Message {
                                        username: "ryo".to_string(),
                                        content: text.to_owned(),
                                    };

                                    self.sender
                                        .blocking_send(
                                            serde_json::to_string(&IpcData::Message(msg)).unwrap(),
                                        )
                                        .unwrap();
                                }

                                if ui.button("Quit").clicked() {
                                    println!("clicked");
                                    event_loop.exit();
                                }

                                if ui.button("play video").clicked() {
                                    let path = "./video.mp4";
                                    self.mpv
                                        .as_mut()
                                        .unwrap()
                                        .command("loadfile", &[&path, "replace"])
                                        .unwrap();
                                };

                                // ui.color_edit_button_rgb(self.clear_color.as_mut().try_into().unwrap());
                            });
                    });

                let gl_surface = self.gl_surface.as_ref().unwrap();
                let window = self.window.as_ref().unwrap();
                let gl_context = self.gl_context.as_ref().unwrap();

                let size = window.inner_size();

                self.mpv_renderer
                    .as_mut()
                    .unwrap()
                    .render::<GlContext>(0, size.width as _, size.height as _, true)
                    .expect("Failed to draw on sdl2 window");

                self.egui_glow
                    .as_mut()
                    .unwrap()
                    .paint(self.window.as_mut().unwrap());

                gl_surface.swap_buffers(gl_context).unwrap();
                // window.request_redraw();
            }

            // WindowEvent::KeyboardInput {
            //     event:
            //         KeyEvent {
            //             logical_key: Key::Named(NamedKey::Space),
            //             state: ElementState::Released,
            //             ..
            //         },
            //     ..
            // } => {
            //     let msg = Message::new("ryo", "chat msg");

            //     self.sender
            //         .blocking_send(serde_json::to_string(&IpcData::Message(msg)).unwrap())
            //         .unwrap();

            //     let path = "./video.mp4";
            //     self.mpv
            //         .as_mut()
            //         .unwrap()
            //         .command("loadfile", &[&path, "replace"])
            //         .unwrap();
            // }
            _ => (),
        }

        let event_response = self
            .egui_glow
            .as_mut()
            .unwrap()
            .on_window_event(self.window.as_mut().unwrap(), &event);

        if event_response.repaint {
            self.window.as_mut().unwrap().request_redraw();
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
                self.window.as_ref().unwrap().request_redraw();
            }

            UserEvent::EguiRedraw => {
                self.window.as_ref().unwrap().request_redraw();
            }
        }
    }
}
