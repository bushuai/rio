#[cfg(all(feature = "wayland", not(any(target_os = "macos", windows))))]
use {
    wayland_client::protocol::wl_surface::WlSurface,
    wayland_client::{Display as WaylandDisplay, Proxy},
    winit::platform::wayland::{EventLoopWindowTargetExtWayland, WindowExtWayland},
};

use crate::clipboard::ClipboardType;
use crate::event::{ClickState, EventP, EventProxy, RioEvent, RioEventType};
use crate::ime::Preedit;
use crate::scheduler::{Scheduler, TimerId, Topic};
use crate::screen::{window::create_window_builder, Screen};
use crate::utils::watch::watch;
use colors::ColorRgb;
use std::collections::HashMap;
use std::error::Error;
use std::os::raw::c_void;
use std::rc::Rc;
use std::time::{Duration, Instant};
use winit::event::{
    ElementState, Event, Ime, MouseButton, MouseScrollDelta, TouchPhase, WindowEvent,
};
use winit::event_loop::{DeviceEventFilter, EventLoop};
use winit::platform::run_return::EventLoopExtRunReturn;
use winit::window::{CursorIcon, ImePurpose, Window, WindowId};

pub struct SequencerWindow {
    is_focused: bool,
    is_occluded: bool,
    window: Window,
    screen: Screen,
}

impl SequencerWindow {
    async fn new(
        event_loop: &EventLoop<EventP>,
        config: &Rc<config::Config>,
        command: Vec<String>,
    ) -> Result<Self, Box<dyn Error>> {
        let proxy = event_loop.create_proxy();
        let event_proxy = EventProxy::new(proxy.clone());
        let event_proxy_clone = event_proxy.clone();
        let window_builder = create_window_builder("Rio");
        let winit_window = window_builder.build(&event_loop).unwrap();

        let current_mouse_cursor = CursorIcon::Text;
        winit_window.set_cursor_icon(current_mouse_cursor);

        // https://docs.rs/winit/latest/winit;/window/enum.ImePurpose.html#variant.Terminal
        winit_window.set_ime_purpose(ImePurpose::Terminal);
        winit_window.set_ime_allowed(true);

        winit_window.set_transparent(config.window_opacity < 1.);

        // TODO: Update ime position based on cursor
        // winit_window.set_ime_position(winit::dpi::PhysicalPosition::new(500.0, 500.0));

        // This will ignore diacritical marks and accent characters from
        // being processed as received characters. Instead, the input
        // device's raw character will be placed in event queues with the
        // Alt modifier set.
        #[cfg(target_os = "macos")]
        {
            // OnlyLeft - The left `Option` key is treated as `Alt`.
            // OnlyRight - The right `Option` key is treated as `Alt`.
            // Both - Both `Option` keys are treated as `Alt`.
            // None - No special handling is applied for `Option` key.
            use winit::platform::macos::{OptionAsAlt, WindowExtMacOS};

            match config.option_as_alt.to_lowercase().as_str() {
                "both" => winit_window.set_option_as_alt(OptionAsAlt::Both),
                "left" => winit_window.set_option_as_alt(OptionAsAlt::OnlyLeft),
                "right" => winit_window.set_option_as_alt(OptionAsAlt::OnlyRight),
                _ => {}
            }
        }

        #[cfg(all(feature = "wayland", not(any(target_os = "macos", windows))))]
        let display: Option<*mut c_void> = event_loop.wayland_display();
        #[cfg(any(not(feature = "wayland"), target_os = "macos", windows))]
        let display: Option<*mut c_void> = Option::None;

        let mut screen =
            Screen::new(&winit_window, &config, event_proxy, display, command).await?;

        screen.init(config.colors.background.1);

        Ok(Self {
            is_focused: false,
            is_occluded: false,
            window: winit_window,
            screen,
        })
    }

    fn new_sync(event_loop: &EventLoop<EventP>, config: &Rc<config::Config>) -> () {
        SequencerWindow::new(event_loop, config, vec![]);
    }

    fn set_focus(&mut self, is_focused: bool) {
        self.is_focused = is_focused;
    }
}

pub struct Sequencer {
    config: Rc<config::Config>,
    windows: HashMap<WindowId, SequencerWindow>,
    #[cfg(all(feature = "wayland", not(any(target_os = "macos", windows))))]
    has_wayland_forcefully_reloaded: bool,
}

impl Sequencer {
    pub fn new(config: config::Config) -> Sequencer {
        Sequencer {
            config: Rc::new(config),
            windows: HashMap::new(),
            #[cfg(all(feature = "wayland", not(any(target_os = "macos", windows))))]
            has_wayland_forcefully_reloaded: false,
        }
    }

    pub async fn run(
        &mut self,
        mut event_loop: EventLoop<EventP>,
        command: Vec<String>,
    ) -> Result<(), Box<dyn Error>> {
        let proxy = event_loop.create_proxy();
        let event_proxy = EventProxy::new(proxy.clone());
        let _ = watch(config::config_dir_path(), event_proxy);
        let mut scheduler = Scheduler::new(proxy);

        #[cfg(all(feature = "wayland", not(any(target_os = "macos", windows))))]
        let mut wayland_event_queue = event_loop.wayland_display().map(|display| {
            let display = unsafe { WaylandDisplay::from_external_display(display as _) };
            display.create_event_queue()
        });

        #[cfg(all(feature = "wayland", not(any(target_os = "macos", windows))))]
        let _wayland_surface = if event_loop.is_wayland() {
            // Attach surface to Rio internal wayland queue to handle frame callbacks.
            let surface = winit_window.wayland_surface().unwrap();
            let proxy: Proxy<WlSurface> = unsafe { Proxy::from_c_ptr(surface as _) };
            Some(proxy.attach(wayland_event_queue.as_ref().unwrap().token()))
        } else {
            None
        };

        let seq_win = SequencerWindow::new(&event_loop, &self.config, command).await?;
        self.windows.insert(seq_win.window.id(), seq_win);

        event_loop.set_device_event_filter(DeviceEventFilter::Always);
        event_loop.run_return(move |event, _, control_flow| {
            match event {
                Event::UserEvent(EventP {
                    payload, window_id, ..
                }) => {
                    if let RioEventType::Rio(event) = payload {
                        match event {
                            RioEvent::Wakeup => {
                                if let Some(sequencer_window) =
                                    self.windows.get_mut(&window_id)
                                {
                                    sequencer_window.window.request_redraw();
                                }
                            }
                            RioEvent::Render => {
                                // if self.config.advanced.disable_render_when_unfocused
                                //     && self.is_window_focused
                                // {
                                //     return;
                                // }
                                // screen.render();
                                if let Some(sequencer_window) =
                                    self.windows.get_mut(&window_id)
                                {
                                    sequencer_window.window.request_redraw();
                                }
                            }
                            RioEvent::UpdateConfig => {
                                if let Some(sequencer_window) =
                                    self.windows.get_mut(&window_id)
                                {
                                    let config = config::Config::load();
                                    self.config = config.into();
                                    sequencer_window.screen.update_config(&self.config);
                                    sequencer_window.window.request_redraw();
                                }
                                // self.has_render_updates = true;
                            }
                            RioEvent::Exit => {
                                if let Some(sequencer_window) =
                                    self.windows.get_mut(&window_id)
                                {
                                    if !sequencer_window.screen.try_close_existent_tab() {
                                        *control_flow =
                                            winit::event_loop::ControlFlow::Exit;
                                    }
                                }
                            }
                            RioEvent::PrepareRender(millis) => {
                                let timer_id = TimerId::new(Topic::Frame, 0);
                                let event = EventP::new(
                                    RioEventType::Rio(RioEvent::Render),
                                    window_id,
                                );

                                if !scheduler.scheduled(timer_id) {
                                    scheduler.schedule(
                                        event,
                                        Duration::from_millis(millis),
                                        false,
                                        timer_id,
                                    );
                                }
                            }
                            RioEvent::Title(_title) => {
                                // if !self.ctx.preserve_title && self.ctx.config.window.dynamic_title {
                                // self.ctx.window().set_title(title);
                                // }
                            }
                            RioEvent::MouseCursorDirty => {
                                if let Some(sequencer_window) =
                                    self.windows.get_mut(&window_id)
                                {
                                    sequencer_window.screen.reset_mouse();
                                }
                            }
                            RioEvent::Scroll(scroll) => {
                                if let Some(sequencer_window) =
                                    self.windows.get_mut(&window_id)
                                {
                                    let mut terminal = sequencer_window
                                        .screen
                                        .ctx()
                                        .current()
                                        .terminal
                                        .lock();
                                    terminal.scroll_display(scroll);
                                    drop(terminal);
                                }
                            }
                            RioEvent::ClipboardLoad(clipboard_type, format) => {
                                if let Some(sequencer_window) =
                                    self.windows.get_mut(&window_id)
                                {
                                    if sequencer_window.is_focused {
                                        let text = format(
                                            sequencer_window
                                                .screen
                                                .clipboard_get(clipboard_type)
                                                .as_str(),
                                        );
                                        sequencer_window
                                            .screen
                                            .ctx_mut()
                                            .current_mut()
                                            .messenger
                                            .send_bytes(text.into_bytes());
                                    }
                                }
                            }
                            RioEvent::ColorRequest(index, format) => {
                                // TODO: colors could be coming terminal as well
                                // if colors has been declaratively changed
                                // Rio doesn't cover this case yet.
                                //
                                // In the future should try first get
                                // from Crosswords then state colors
                                // screen.colors()[index] or screen.state.colors[index]
                                if let Some(sequencer_window) =
                                    self.windows.get_mut(&window_id)
                                {
                                    let color =
                                        sequencer_window.screen.state.colors[index];
                                    let rgb = ColorRgb::from_color_arr(color);
                                    sequencer_window
                                        .screen
                                        .ctx_mut()
                                        .current_mut()
                                        .messenger
                                        .send_bytes(format(rgb).into_bytes());
                                }
                            }
                            RioEvent::WindowCreateNew => {
                                // SequencerWindow::new_sync(&event_loop, &self.config);
                            }
                            _ => {}
                        }
                    }
                }
                Event::Resumed => {
                    // self.windows.insert(winit_window.id(), winit_window);

                    // Emitted when the application has been resumed.
                    // This is a hack to avoid an odd scenario in wayland window initialization
                    // wayland windows starts with the wrong width/height.
                    // Rio is ignoring wayland new dimension events, so the terminal
                    // start with the wrong width/height (fix the ignore would be the best fix though)
                    //
                    // The code below forcefully reload dimensions in the terminal initialization
                    // to load current width/height.
                    #[cfg(all(
                        feature = "wayland",
                        not(any(target_os = "macos", windows))
                    ))]
                    {
                        if !self.has_wayland_forcefully_reloaded {
                            screen.update_config(&self.config);
                            self.has_render_updates = true;
                            self.has_wayland_forcefully_reloaded = true;
                        }
                    }
                }

                Event::WindowEvent {
                    event: winit::event::WindowEvent::CloseRequested,
                    window_id,
                    ..
                } => {
                    self.windows.remove(&window_id);

                    if self.windows.is_empty() {
                        *control_flow = winit::event_loop::ControlFlow::Exit;
                    }
                }

                Event::WindowEvent {
                    event: winit::event::WindowEvent::ModifiersChanged(modifiers),
                    window_id,
                    ..
                } => {
                    if let Some(sequencer_window) = self.windows.get_mut(&window_id) {
                        sequencer_window.screen.set_modifiers(modifiers);
                    }
                }

                Event::WindowEvent {
                    event: WindowEvent::MouseInput { state, button, .. },
                    window_id,
                    ..
                } => {
                    if let Some(sequencer_window) = self.windows.get_mut(&window_id) {
                        sequencer_window.window.set_cursor_visible(true);

                        match button {
                            MouseButton::Left => {
                                sequencer_window.screen.mouse.left_button_state = state
                            }
                            MouseButton::Middle => {
                                sequencer_window.screen.mouse.middle_button_state = state
                            }
                            MouseButton::Right => {
                                sequencer_window.screen.mouse.right_button_state = state
                            }
                            _ => (),
                        }

                        match state {
                            ElementState::Pressed => {
                                // Process mouse press before bindings to update the `click_state`.
                                if !sequencer_window.screen.modifiers.shift()
                                    && sequencer_window.screen.mouse_mode()
                                {
                                    sequencer_window.screen.mouse.click_state =
                                        ClickState::None;

                                    let code = match button {
                                        MouseButton::Left => 0,
                                        MouseButton::Middle => 1,
                                        MouseButton::Right => 2,
                                        // Can't properly report more than three buttons..
                                        MouseButton::Other(_) => return,
                                    };

                                    sequencer_window
                                        .screen
                                        .mouse_report(code, ElementState::Pressed);
                                } else {
                                    // Calculate time since the last click to handle double/triple clicks.
                                    let now = Instant::now();
                                    let elapsed = now
                                        - sequencer_window
                                            .screen
                                            .mouse
                                            .last_click_timestamp;
                                    sequencer_window.screen.mouse.last_click_timestamp =
                                        now;

                                    let threshold = Duration::from_millis(300);
                                    let mouse = &sequencer_window.screen.mouse;
                                    sequencer_window.screen.mouse.click_state =
                                        match mouse.click_state {
                                            // Reset click state if button has changed.
                                            _ if button != mouse.last_click_button => {
                                                sequencer_window
                                                    .screen
                                                    .mouse
                                                    .last_click_button = button;
                                                ClickState::Click
                                            }
                                            ClickState::Click if elapsed < threshold => {
                                                ClickState::DoubleClick
                                            }
                                            ClickState::DoubleClick
                                                if elapsed < threshold =>
                                            {
                                                ClickState::TripleClick
                                            }
                                            _ => ClickState::Click,
                                        };

                                    // Load mouse point, treating message bar and padding as the closest square.
                                    let display_offset =
                                        sequencer_window.screen.display_offset();

                                    if let MouseButton::Left = button {
                                        let point = sequencer_window
                                            .screen
                                            .mouse_position(display_offset);
                                        sequencer_window.screen.on_left_click(point);
                                    }

                                    // sequencer_window.has_render_updates = true;
                                }
                                // sequencer_window.screen.process_mouse_bindings(button);
                            }
                            ElementState::Released => {
                                if !sequencer_window.screen.modifiers.shift()
                                    && sequencer_window.screen.mouse_mode()
                                {
                                    let code = match button {
                                        MouseButton::Left => 0,
                                        MouseButton::Middle => 1,
                                        MouseButton::Right => 2,
                                        // Can't properly report more than three buttons.
                                        MouseButton::Other(_) => return,
                                    };
                                    sequencer_window
                                        .screen
                                        .mouse_report(code, ElementState::Released);
                                    return;
                                }

                                if let MouseButton::Left | MouseButton::Right = button {
                                    // Copy selection on release, to prevent flooding the display server.
                                    sequencer_window
                                        .screen
                                        .copy_selection(ClipboardType::Selection);
                                }
                            }
                        }
                    }
                }

                Event::WindowEvent {
                    event: WindowEvent::CursorMoved { position, .. },
                    window_id,
                    ..
                } => {
                    if let Some(sw) = self.windows.get_mut(&window_id) {
                        sw.window.set_cursor_visible(true);
                        let x = position.x;
                        let y = position.y;

                        let lmb_pressed =
                            sw.screen.mouse.left_button_state == ElementState::Pressed;
                        let rmb_pressed =
                            sw.screen.mouse.right_button_state == ElementState::Pressed;

                        if !sw.screen.selection_is_empty() && (lmb_pressed || rmb_pressed)
                        {
                            sw.screen.update_selection_scrolling(y);
                        }

                        let display_offset = sw.screen.display_offset();
                        let old_point = sw.screen.mouse_position(display_offset);

                        let x = x.clamp(0.0, sw.screen.sugarloaf.layout.width.into())
                            as usize;
                        let y = y.clamp(0.0, sw.screen.sugarloaf.layout.height.into())
                            as usize;
                        sw.screen.mouse.x = x;
                        sw.screen.mouse.y = y;

                        let point = sw.screen.mouse_position(display_offset);
                        let square_changed = old_point != point;

                        let inside_text_area = sw.screen.contains_point(x, y);
                        let square_side = sw.screen.side_by_pos(x);

                        // If the mouse hasn't changed cells, do nothing.
                        if !square_changed
                            && sw.screen.mouse.square_side == square_side
                            && sw.screen.mouse.inside_text_area == inside_text_area
                        {
                            return;
                        }

                        sw.screen.mouse.inside_text_area = inside_text_area;
                        sw.screen.mouse.square_side = square_side;

                        let cursor_icon =
                            if !sw.screen.modifiers.shift() && sw.screen.mouse_mode() {
                                CursorIcon::Default
                            } else {
                                CursorIcon::Text
                            };

                        sw.window.set_cursor_icon(cursor_icon);

                        if (lmb_pressed || rmb_pressed)
                            && (sw.screen.modifiers.shift() || !sw.screen.mouse_mode())
                        {
                            sw.screen.update_selection(point, square_side);
                        } else if square_changed && sw.screen.has_mouse_motion_and_drag()
                        {
                            if lmb_pressed {
                                sw.screen.mouse_report(32, ElementState::Pressed);
                            } else if sw.screen.mouse.middle_button_state
                                == ElementState::Pressed
                            {
                                sw.screen.mouse_report(33, ElementState::Pressed);
                            } else if sw.screen.mouse.right_button_state
                                == ElementState::Pressed
                            {
                                sw.screen.mouse_report(34, ElementState::Pressed);
                            } else if sw.screen.has_mouse_motion() {
                                sw.screen.mouse_report(35, ElementState::Pressed);
                            }
                        }

                        sw.window.request_redraw();
                        // sequencer_window.has_render_updates = true;
                    }
                }

                Event::WindowEvent {
                    event: WindowEvent::MouseWheel { delta, phase, .. },
                    window_id,
                    ..
                } => {
                    if let Some(sw) = self.windows.get_mut(&window_id) {
                        sw.window.set_cursor_visible(true);
                        match delta {
                            MouseScrollDelta::LineDelta(columns, lines) => {
                                let new_scroll_px_x =
                                    columns * sw.screen.sugarloaf.layout.font_size;
                                let new_scroll_px_y =
                                    lines * sw.screen.sugarloaf.layout.font_size;
                                sw.screen.scroll(
                                    new_scroll_px_x as f64,
                                    new_scroll_px_y as f64,
                                );
                            }
                            MouseScrollDelta::PixelDelta(mut lpos) => {
                                match phase {
                                    TouchPhase::Started => {
                                        // Reset offset to zero.
                                        sw.screen.mouse.accumulated_scroll =
                                            Default::default();
                                    }
                                    TouchPhase::Moved => {
                                        // When the angle between (x, 0) and (x, y) is lower than ~25 degrees
                                        // (cosine is larger that 0.9) we consider this scrolling as horizontal.
                                        if lpos.x.abs() / lpos.x.hypot(lpos.y) > 0.9 {
                                            lpos.y = 0.;
                                        } else {
                                            lpos.x = 0.;
                                        }

                                        sw.screen.scroll(lpos.x, lpos.y);
                                    }
                                    _ => (),
                                }
                            }
                        }
                    }
                }
                Event::WindowEvent {
                    event: winit::event::WindowEvent::ReceivedCharacter(character),
                    window_id,
                    ..
                } => {
                    if let Some(sw) = self.windows.get_mut(&window_id) {
                        sw.screen.input_character(character);
                    }
                }

                Event::WindowEvent {
                    event:
                        winit::event::WindowEvent::KeyboardInput {
                            is_synthetic: false,
                            input:
                                winit::event::KeyboardInput {
                                    virtual_keycode,
                                    scancode,
                                    state,
                                    ..
                                },
                            ..
                        },
                    window_id,
                    ..
                } => match state {
                    ElementState::Pressed => {
                        if let Some(sw) = self.windows.get_mut(&window_id) {
                            sw.window.set_cursor_visible(false);
                            sw.screen.input_keycode(virtual_keycode, scancode);
                        }
                    }

                    ElementState::Released => {
                        if let Some(sw) = self.windows.get_mut(&window_id) {
                            sw.window.request_redraw();
                        }
                    }
                },

                Event::WindowEvent {
                    event: WindowEvent::Ime(ime),
                    window_id,
                    ..
                } => {
                    if let Some(sw) = self.windows.get_mut(&window_id) {
                        match ime {
                            Ime::Commit(text) => {
                                sw.screen.paste(&text, true);
                            }
                            Ime::Preedit(text, cursor_offset) => {
                                let preedit = if text.is_empty() {
                                    None
                                } else {
                                    Some(Preedit::new(
                                        text,
                                        cursor_offset.map(|offset| offset.0),
                                    ))
                                };

                                if sw.screen.ime.preedit() != preedit.as_ref() {
                                    sw.screen.ime.set_preedit(preedit);
                                    sw.screen.render();
                                }
                            }
                            Ime::Enabled => {
                                sw.screen.ime.set_enabled(true);
                            }
                            Ime::Disabled => {
                                sw.screen.ime.set_enabled(false);
                            }
                        }
                    }
                }

                Event::WindowEvent {
                    event: winit::event::WindowEvent::Focused(focused),
                    window_id,
                    ..
                } => {
                    if let Some(sequencer_window) = self.windows.get_mut(&window_id) {
                        sequencer_window.window.set_cursor_visible(true);
                        sequencer_window.is_focused = focused;
                    }
                }

                Event::WindowEvent {
                    event: winit::event::WindowEvent::Occluded(occluded),
                    window_id,
                    ..
                } => {
                    if let Some(sequencer_window) = self.windows.get_mut(&window_id) {
                        sequencer_window.is_occluded = occluded;
                    }
                }

                Event::WindowEvent {
                    event: winit::event::WindowEvent::DroppedFile(path),
                    window_id,
                    ..
                } => {
                    if let Some(sw) = self.windows.get_mut(&window_id) {
                        let path: String = path.to_string_lossy().into();
                        sw.screen.paste(&(path + " "), true);
                    }
                }

                Event::WindowEvent {
                    event: winit::event::WindowEvent::Resized(new_size),
                    window_id,
                    ..
                } => {
                    if new_size.width == 0 || new_size.height == 0 {
                        return;
                    }

                    if let Some(sw) = self.windows.get_mut(&window_id) {
                        sw.screen.resize(new_size);
                        // sw.has_render_updates = true;
                    }
                }

                Event::WindowEvent {
                    event:
                        winit::event::WindowEvent::ScaleFactorChanged {
                            new_inner_size,
                            scale_factor,
                        },
                    window_id,
                    ..
                } => {
                    if let Some(sw) = self.windows.get_mut(&window_id) {
                        sw.screen.set_scale(scale_factor as f32, *new_inner_size);
                        sw.window.request_redraw();
                        // sequencer_window.has_render_updates = true;
                    }
                }

                // Emitted when the event loop is being shut down.
                // This is irreversible - if this event is emitted, it is guaranteed to be the last event that gets emitted.
                // You generally want to treat this as an “do on quit” event.
                Event::LoopDestroyed { .. } => {
                    // TODO: Now we are forcing an exit operation
                    // but it should be revaluated since CloseRequested in MacOs
                    // not necessarily exit the process
                    std::process::exit(0);
                }
                Event::RedrawEventsCleared { .. } => {
                    // Skip render for macos and x11 windows that are fully occluded
                    #[cfg(all(
                        feature = "wayland",
                        not(any(target_os = "macos", target_os = "windows"))
                    ))]
                    if let Some(w_event_queue) = wayland_event_queue.as_mut() {
                        w_event_queue
                            .dispatch_pending(&mut (), |_, _, _| {})
                            .expect("failed to dispatch wayland event queue");
                    }

                    scheduler.update();
                }
                Event::MainEventsCleared { .. } => {}
                Event::RedrawRequested(window_id) => {
                    if let Some(sw) = self.windows.get_mut(&window_id) {
                        // *control_flow = winit::event_loop::ControlFlow::Wait;
                        if sw.is_occluded {
                            return;
                        }

                        sw.screen.render();
                    }
                }
                _ => {}
            }
        });

        Ok(())
    }
}
